use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::brew::{Brew, Environment, is_executable};
use crate::command;
use crate::runtimes::{Mise, MiseTool, home_directory, valid_mise_version};

#[derive(Clone, Copy)]
pub enum DeveloperTool {
    Pnpm,
    Yarn,
    Gopls,
    Delve,
    CargoNextest,
    Uv,
    Ruff,
    Maven,
    Gradle,
}

impl DeveloperTool {
    pub const ALL: [Self; 9] = [
        Self::Pnpm,
        Self::Yarn,
        Self::Gopls,
        Self::Delve,
        Self::CargoNextest,
        Self::Uv,
        Self::Ruff,
        Self::Maven,
        Self::Gradle,
    ];

    pub fn for_environment(environment: Environment) -> &'static [Self] {
        match environment {
            Environment::Node => &[Self::Pnpm, Self::Yarn],
            Environment::Go => &[Self::Gopls, Self::Delve],
            Environment::Rust => &[Self::CargoNextest],
            Environment::Python => &[Self::Uv, Self::Ruff],
            Environment::Java => &[Self::Maven, Self::Gradle],
            _ => &[],
        }
    }

    pub fn formula(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Gopls => "gopls",
            Self::Delve => "delve",
            Self::CargoNextest => "cargo-nextest",
            Self::Uv => "uv",
            Self::Ruff => "ruff",
            Self::Maven => "maven",
            Self::Gradle => "gradle",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Yarn => "Yarn Classic",
            Self::Delve => "Delve",
            Self::Ruff => "Ruff",
            Self::Maven => "Maven",
            Self::Gradle => "Gradle",
            _ => self.formula(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRuntime {
    Brew(String),
    Mise { name: String, version: String },
}

impl ToolRuntime {
    fn mise_tool<'a>(
        &self,
        runtimes: &'a [MiseTool],
        environment: Environment,
    ) -> Result<&'a MiseTool> {
        let Self::Mise { name, version } = self else {
            bail!("未选择 Mise 版本");
        };
        runtimes
            .iter()
            .find(|tool| {
                tool.environment == environment && tool.name == *name && tool.version == *version
            })
            .context("未找到该 Mise 版本，请重新检测")
    }

    fn executable(
        &self,
        brew: Option<&Brew>,
        runtimes: &[MiseTool],
        environment: Environment,
        binary: &str,
    ) -> Result<PathBuf> {
        let executable = match self {
            Self::Brew(formula) => {
                ensure!(
                    Environment::from_formula(formula) == Some(environment),
                    "此 Homebrew 公式不属于 {}",
                    environment.label()
                );
                brew.context("未找到 Homebrew")?
                    .opt_path(formula)?
                    .join(environment.brew_bin_dir())
                    .join(binary)
            }
            Self::Mise { .. } => self
                .mise_tool(runtimes, environment)?
                .executable
                .clone()
                .context("Mise 未返回该版本的可执行文件")?,
        };
        ensure!(
            is_executable(&executable),
            "可执行文件不存在或不可执行：{}",
            executable.display()
        );
        Ok(executable)
    }
}

#[derive(Clone, Debug)]
pub struct NpmInfo {
    pub version: String,
    pub prefix: PathBuf,
    pub packages: Vec<NpmPackage>,
}

#[derive(Clone, Debug)]
pub struct NpmPackage {
    pub name: String,
    pub version: String,
}

#[derive(Deserialize)]
struct NpmListing {
    #[serde(default)]
    dependencies: BTreeMap<String, NpmDependency>,
}

#[derive(Deserialize)]
struct NpmDependency {
    version: Option<String>,
}

struct Npm<'a> {
    executable: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    mise: Option<(&'a Mise, String)>,
}

impl<'a> Npm<'a> {
    fn resolve(
        brew: Option<&Brew>,
        mise: Option<&'a Mise>,
        runtimes: &[MiseTool],
        target: &ToolRuntime,
    ) -> Result<Self> {
        let node = target.executable(brew, runtimes, Environment::Node, "node")?;
        let bin = node.parent().context("无法识别该 Node.js 的安装目录")?;
        let executable = bin.join("npm");
        ensure!(is_executable(&executable), "该 Node.js 版本没有可用的 npm");
        let mise = match target {
            ToolRuntime::Brew(_) => None,
            ToolRuntime::Mise { name, version } => {
                ensure!(valid_mise_version(version), "无法识别该 Mise 版本");
                Some((mise.context("未找到 Mise")?, format!("{name}@{version}")))
            }
        };
        Ok(Self {
            executable,
            bin: bin.into(),
            home: home_directory().context("无法确定当前用户的主目录")?,
            mise,
        })
    }

    fn command(&self, args: &[&str], offline: bool) -> Result<Command> {
        let mut command = if let Some((mise, tool)) = &self.mise {
            let mut command = mise.command(&["exec", tool, "--"], offline);
            command.arg(&self.executable);
            command
        } else {
            let path = env::join_paths(
                std::iter::once(self.bin.clone()).chain(
                    env::split_paths(&env::var_os("PATH").unwrap_or_default())
                        .filter(|path| path.is_absolute()),
                ),
            )
            .context("无法为该 npm 版本设置 PATH")?;
            let mut command = Command::new(&self.executable);
            command.env("PATH", path);
            command
        };
        command
            .current_dir(&self.home)
            .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
            .args(args);
        Ok(command)
    }

    fn version(&self) -> Result<String> {
        let output = command::output(self.command(&["--version"], true)?, "读取 npm 版本", None)?;
        let version = output.trim();
        ensure!(!version.is_empty(), "npm 未返回版本号");
        Ok(version.to_owned())
    }

    fn prefix(&self) -> Result<PathBuf> {
        let output = command::output(
            self.command(&["prefix", "--global"], true)?,
            "读取 npm 全局安装前缀",
            None,
        )?;
        let prefix = PathBuf::from(output.trim());
        ensure!(prefix.is_absolute(), "npm 未返回有效的全局安装前缀");
        Ok(prefix)
    }
}

pub fn npm_info(
    brew: Option<&Brew>,
    mise: Option<&Mise>,
    runtimes: &[MiseTool],
    target: &ToolRuntime,
) -> Result<NpmInfo> {
    let npm = Npm::resolve(brew, mise, runtimes, target)?;
    let version = npm.version()?;
    let prefix = npm.prefix()?;
    let listing = command::output(
        npm.command(&["ls", "--global", "--depth=0", "--json"], true)?,
        "读取 npm 全局包",
        None,
    )?;
    let listing: NpmListing = serde_json::from_str(&listing).context("无法读取 npm 全局包列表")?;
    let packages = listing
        .dependencies
        .into_iter()
        .filter(|(name, _)| !protected_npm_package(name))
        .map(|(name, package)| NpmPackage {
            name,
            version: package.version.unwrap_or_else(|| "未知版本".into()),
        })
        .collect();
    Ok(NpmInfo {
        version,
        prefix,
        packages,
    })
}

pub fn npm_change(
    brew: Option<&Brew>,
    mise: Option<&Mise>,
    runtimes: &[MiseTool],
    target: &ToolRuntime,
    package: &str,
    install: bool,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let (name, version) = npm_package_spec(package.trim())?;
    ensure!(!protected_npm_package(name), "请通过 Node.js 管理随附的 {name}");
    let npm = Npm::resolve(brew, mise, runtimes, target)?;
    let prefix = npm.prefix()?;
    let (operation, process) = if install {
        let spec = format!("{name}@{}", version.unwrap_or("latest"));
        (
            format!("安装 npm 全局包 {spec}"),
            npm.command(&["install", "--global", "--foreground-scripts", "--", &spec], false)?,
        )
    } else {
        ensure!(version.is_none(), "卸载 npm 全局包时只需提供包名");
        (
            format!("卸载 npm 全局包 {name}"),
            npm.command(&["uninstall", "--global", "--", name], false)?,
        )
    };
    stream_install(process, &operation, proxy, log, || {
        format!(
            "无法写入 npm 全局安装位置 {}。请在终端检查目录权限或配置可写的 npm 全局路径，再重试",
            prefix.display()
        )
    })
}

fn npm_package_spec(input: &str) -> Result<(&str, Option<&str>)> {
    let version_start = input
        .get(1..)
        .and_then(|suffix| suffix.find('@'))
        .map(|index| index + 1);
    let (name, version) = match version_start {
        Some(index) => (&input[..index], Some(&input[index + 1..])),
        None => (input, None),
    };
    let valid_part = |part: &str| {
        part.as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    };
    let valid_name = name.strip_prefix('@').map_or_else(
        || valid_part(name),
        |scope| {
            scope
                .split_once('/')
                .is_some_and(|(owner, package)| valid_part(owner) && valid_part(package))
        },
    );
    ensure!(
        name.len() <= 214 && valid_name && version.is_none_or(valid_mise_version),
        "请输入 npm 包名，可附加版本或标签，例如 pnpm、pnpm@10.0.0 或 @scope/tool"
    );
    Ok((name, version))
}

fn protected_npm_package(name: &str) -> bool {
    ["npm", "npx", "corepack"]
        .into_iter()
        .any(|package| name.eq_ignore_ascii_case(package))
}

#[derive(Clone, Debug)]
pub struct RustComponents {
    pub rustfmt: bool,
    pub clippy: bool,
    pub installable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RustComponent {
    Rustfmt,
    Clippy,
}

impl RustComponent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rustfmt => "rustfmt",
            Self::Clippy => "clippy",
        }
    }
}

pub fn rust_components(
    brew: Option<&Brew>,
    mise: Option<&Mise>,
    runtimes: &[MiseTool],
    target: &ToolRuntime,
) -> Result<RustComponents> {
    let rustc = target.executable(brew, runtimes, Environment::Rust, "rustc")?;
    if let Some(rustup) = Rustup::resolve(mise, runtimes, target, &rustc)
        && let Ok(output) = command::output(
            rustup.command(
                &["component", "list", "--installed", "--toolchain", &rustup.version],
                true,
            ),
            "读取 Rust 工具链组件",
            None,
        )
    {
        let installed = |component: RustComponent| {
            output.lines().map(str::trim).any(|line| {
                line == component.label()
                    || line
                        .strip_prefix(component.label())
                        .is_some_and(|suffix| suffix.starts_with('-'))
            })
        };
        return Ok(RustComponents {
            rustfmt: installed(RustComponent::Rustfmt),
            clippy: installed(RustComponent::Clippy),
            installable: true,
        });
    }
    let bin = rustc.parent().context("无法识别该 Rust 的安装目录")?;
    Ok(RustComponents {
        rustfmt: is_executable(&bin.join("rustfmt")),
        clippy: is_executable(&bin.join("cargo-clippy")),
        installable: false,
    })
}

pub fn install_rust_component(
    brew: Option<&Brew>,
    mise: Option<&Mise>,
    runtimes: &[MiseTool],
    target: &ToolRuntime,
    component: RustComponent,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let rustc = target.executable(brew, runtimes, Environment::Rust, "rustc")?;
    let rustup = Rustup::resolve(mise, runtimes, target, &rustc)
        .context("此 Rust 版本不由 Rustup 管理，无法单独安装组件")?;
    let process = rustup.command(
        &["component", "add", "--toolchain", &rustup.version, component.label()],
        false,
    );
    stream_install(
        process,
        &format!("安装 Rust 组件 {}", component.label()),
        proxy,
        log,
        || "无法写入该 Rust 工具链。在终端检查工具链目录权限后重试".into(),
    )
}

struct Rustup<'a> {
    mise: &'a Mise,
    tool: String,
    version: String,
    executable: String,
}

impl<'a> Rustup<'a> {
    fn resolve(
        mise: Option<&'a Mise>,
        runtimes: &[MiseTool],
        target: &ToolRuntime,
        rustc: &Path,
    ) -> Option<Self> {
        let mise = mise?;
        let runtime = target.mise_tool(runtimes, Environment::Rust).ok()?;
        if !matches!(runtime.name.as_str(), "rust" | "core:rust")
            || !valid_mise_version(&runtime.version)
        {
            return None;
        }
        let version = runtime.version.clone();
        let tool = format!("{}@{version}", runtime.name);
        let selector = format!("--tool={tool}");
        let executable = command::output(
            mise.command(&["which", "rustup", &selector], true),
            "查找该版本的 Rustup",
            None,
        )
        .ok()
        .map(|output| PathBuf::from(output.trim()))
        .filter(|path| path.is_absolute() && is_executable(path))
        .or_else(|| {
            let rustup = runtime.install_path.as_ref()?.join("rustup");
            is_executable(&rustup).then_some(rustup)
        })?;
        let rustup = Self {
            mise,
            tool,
            version,
            executable: executable.into_os_string().into_string().ok()?,
        };
        let resolved = command::output(
            rustup.command(&["which", "--toolchain", &rustup.version, "rustc"], true),
            "匹配 Rustup 工具链",
            None,
        )
        .ok()?;
        let resolved = Path::new(resolved.trim());
        if !resolved.is_absolute()
            || fs::canonicalize(resolved).ok()? != fs::canonicalize(rustc).ok()?
        {
            return None;
        }
        Some(rustup)
    }

    fn command(&self, args: &[&str], offline: bool) -> Command {
        let mut mise_args = vec!["exec", self.tool.as_str(), "--", self.executable.as_str()];
        mise_args.extend_from_slice(args);
        let mut command = self.mise.command(&mise_args, offline);
        command.env("RUSTUP_AUTO_INSTALL", "0");
        command
    }
}

fn stream_install(
    process: Command,
    operation: &str,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
    hint: impl FnOnce() -> String,
) -> Result<()> {
    let permission_error = |message: &str| {
        let message = message.to_ascii_lowercase();
        ["eacces", "eperm", "permission denied", "operation not permitted"]
            .into_iter()
            .any(|pattern| message.contains(pattern))
    };
    let requires_permission = AtomicBool::new(false);
    command::stream(process, operation, proxy, &|line| {
        if !line.starts_with("$ ") && permission_error(line) {
            requires_permission.store(true, Ordering::Relaxed);
        }
        log(line);
    })
    .map_err(|error| {
        if requires_permission.load(Ordering::Relaxed) || permission_error(&format!("{error:#}")) {
            error.context(hint())
        } else {
            error
        }
    })
}
