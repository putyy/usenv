use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::brew::{Environment, is_executable};
use crate::command;

#[derive(Clone, Deserialize, Serialize)]
pub struct MiseTool {
    pub environment: Environment,
    pub name: String,
    pub version: String,
    pub install_path: Option<PathBuf>,
    pub executable: Option<PathBuf>,
    pub cargo_executable: Option<PathBuf>,
    pub active: Option<bool>,
}

pub(crate) fn home_directory() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn search_paths(home: &Path) -> Vec<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .filter(|path| path.is_absolute())
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            home.join(".local/bin"),
        ])
        .collect()
}

fn find_command<'a>(paths: impl IntoIterator<Item = &'a PathBuf>, name: &str) -> Option<PathBuf> {
    paths
        .into_iter()
        .map(|path| path.join(name))
        .find(|path| is_executable(path))
}

#[derive(Clone)]
pub struct Mise {
    executable: PathBuf,
    home: PathBuf,
}

impl Mise {
    #[cfg(test)]
    pub(crate) fn for_test(home: &Path) -> Self {
        Self { executable: home.join("mise"), home: home.to_owned() }
    }

    pub fn detect() -> Option<Self> {
        let home = home_directory()?;
        let executable = find_command(&search_paths(&home), "mise")?;
        Some(Self { executable, home })
    }

    pub fn path(&self) -> &Path {
        &self.executable
    }

    pub fn version(&self) -> Result<String> {
        let output = self.run(&["--version"], true, None)?;
        output.lines().next().map(str::to_owned).context("Mise 未返回版本")
    }

    pub fn global_config_path(&self) -> PathBuf {
        self.path_override("MISE_GLOBAL_CONFIG_FILE")
            .or_else(|| self.path_override("MISE_CONFIG_FILE"))
            .unwrap_or_else(|| {
                self.path_override("MISE_CONFIG_DIR")
                    .or_else(|| self.path_override("XDG_CONFIG_HOME").map(|path| path.join("mise")))
                    .unwrap_or_else(|| self.home.join(".config/mise"))
                    .join("config.toml")
            })
    }

    pub fn installs_directory(&self) -> PathBuf {
        self.path_override("MISE_INSTALLS_DIR").unwrap_or_else(|| {
            self.path_override("MISE_DATA_DIR")
                .or_else(|| self.path_override("XDG_DATA_HOME").map(|path| path.join("mise")))
                .unwrap_or_else(|| self.home.join(".local/share/mise"))
                .join("installs")
        })
    }

    fn path_override(&self, name: &str) -> Option<PathBuf> {
        let path = PathBuf::from(env::var_os(name).filter(|value| !value.is_empty())?);
        Some(self.home.join(path.strip_prefix("~").unwrap_or(&path)))
    }

    pub fn loaded_configs(&self) -> Result<Vec<PathBuf>> {
        #[derive(Deserialize)]
        struct LoadedConfig {
            path: PathBuf,
        }
        let output = self.run(&["config", "ls", "--json"], true, None)?;
        let entries: Vec<LoadedConfig> = serde_json::from_str(&output)
            .context("无法解析 Mise 返回的配置文件列表")?;
        let mut seen = BTreeSet::new();
        Ok(entries.into_iter().map(|entry| entry.path)
            .filter(|path| path.is_absolute() && seen.insert(path.clone()))
            .collect())
    }

    pub fn supports(environment: Environment) -> bool {
        mise_binary(environment).is_some()
    }

    pub fn available_versions(
        &self,
        environment: Environment,
        proxy: Option<&str>,
    ) -> Result<Vec<String>> {
        ensure!(Self::supports(environment), "该环境暂不支持 Mise");
        let output = self.run(&["ls-remote", mise_install_name(environment)], false, proxy)?;
        let mut seen = BTreeSet::new();
        let mut versions: Vec<_> = output
            .lines()
            .map(str::trim)
            .filter(|version| valid_mise_version(version) && seen.insert(*version))
            .map(str::to_owned)
            .collect();
        ensure!(!versions.is_empty(), "Mise 未返回可安装版本");
        versions.sort_by_cached_key(|version| {
            let number = version.strip_prefix('v').unwrap_or(version);
            (
                Reverse(version_numbers(Some(number))),
                version.contains('-'),
            )
        });
        Ok(versions)
    }

    pub fn install(
        &self,
        environment: Environment,
        version: &str,
        proxy: Option<&str>,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        ensure!(Self::supports(environment), "该环境暂不支持 Mise");
        ensure!(
            valid_mise_version(version),
            "请输入 Mise 版本，如 latest、22 或 22.14.0"
        );
        let tool = format!("{}@{version}", mise_install_name(environment));
        let mut command = self.command(&["install", &tool], false);
        command.env("MISE_RAW", "1").env("MISE_COLOR", "0");
        command::stream(command, &format!("mise install {tool}"), proxy, log)
    }

    pub fn set_global(&self, environment: Environment, name: &str, version: &str) -> Result<()> {
        ensure!(valid_mise_version(version), "该 Mise 版本无法设为全局版本");
        ensure!(
            self.installed()?.iter().any(|tool| {
                tool.environment == environment
                    && tool.name == name
                    && tool.version == version
                    && tool.executable.is_some()
            }),
            "该 Mise 版本已不在安装列表中，请刷新"
        );
        let tool = format!("{name}@{version}");
        self.run(&["use", "--global", "--pin", &tool], true, None)?;
        Ok(())
    }

    pub fn uninstall(
        &self,
        environment: Environment,
        name: &str,
        version: &str,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        ensure!(
            mise_environment(name) == Some(environment) && valid_mise_version(version),
            "无法识别该 Mise 版本"
        );
        let globals = self.run(&["ls", "--global", "--installed", "--json"], true, None)?;
        let globals: MiseVersions = serde_json::from_str(&globals)
            .context("无法确认当前 Mise 全局版本")?;
        ensure!(!globals.get(name).is_some_and(|entries| {
            entries.iter().any(|entry| entry.version == version && entry.active)
        }), "请先切换 {name} 的 Mise 全局版本");
        let output = self.run(&["ls", "--prunable", "--json"], true, None)
            .context("无法确认版本引用状态，请检查 Mise 版本后重试")?;
        let prunable: MiseVersions = serde_json::from_str(&output)
            .context("无法读取 Mise 可卸载版本列表")?;
        ensure!(prunable.get(name).is_some_and(|entries| {
            entries.iter().any(|entry| entry.version == version)
        }), "{name} {version} 未列为可卸载版本，可能已被移除或仍被 Mise 配置引用");
        let tool = format!("{name}@{version}");
        let mut process = self.command(&["uninstall", &tool], true);
        process.env("MISE_COLOR", "0");
        command::stream(process, &format!("mise uninstall {tool}"), None, log)
    }

    pub fn installed(&self) -> Result<Vec<MiseTool>> {
        let output = self.run(&["ls", "--installed", "--json"], true, None)?;
        let entries = serde_json::from_str(&output).context("无法读取 Mise 版本列表")?;
        let globals = self
            .run(&["ls", "--global", "--installed", "--json"], true, None)
            .ok()
            .and_then(|output| serde_json::from_str(&output).ok());
        Ok(self.installed_tools(&entries, globals.as_ref()))
    }

    fn installed_tools(&self, entries: &MiseVersions, globals: Option<&MiseVersions>) -> Vec<MiseTool> {
        let mut tools = Vec::new();
        for (name, versions) in entries {
            let Some(environment) = mise_environment(name) else {
                continue;
            };
            for entry in versions.iter().filter(|entry| entry.installed) {
                let install_path = entry.install_path.clone().filter(|path| path.is_absolute());
                let (executable, cargo_executable) =
                    self.tool_paths(environment, name, &entry.version, install_path.as_deref());
                let active = globals.map(|entries| entries.get(name).is_some_and(|versions| {
                    versions.iter().any(|global| {
                        global.active
                            && global.installed
                            && global.version == entry.version
                            && global.install_path == entry.install_path
                    })
                }));
                tools.push(MiseTool {
                    environment,
                    name: name.clone(),
                    version: entry.version.clone(),
                    install_path,
                    executable,
                    cargo_executable,
                    active,
                });
            }
        }
        tools.sort_by(|left, right| {
            left.environment
                .cmp(&right.environment)
                .then(right.active.cmp(&left.active))
                .then_with(|| {
                    version_numbers(Some(&right.version)).cmp(&version_numbers(Some(&left.version)))
                })
        });
        tools
    }

    fn tool_paths(
        &self,
        environment: Environment,
        name: &str,
        version: &str,
        install_path: Option<&Path>,
    ) -> (Option<PathBuf>, Option<PathBuf>) {
        let Some(binary) = mise_binary(environment).filter(|_| valid_mise_version(version)) else {
            return (None, None);
        };
        if environment == Environment::Elasticsearch {
            let executable = install_path
                .map(|path| path.join("bin/elasticsearch"))
                .filter(|path| is_executable(path));
            return (executable, None);
        }
        let tool = format!("{name}@{version}");
        let selector = format!("--tool={tool}");
        if matches!(name, "rust" | "core:rust")
            && let Some(rustup) = self
                .executable_path(&["which", "rustup", &selector])
                .or_else(|| {
                    install_path
                        .map(|path| path.join("rustup"))
                        .filter(|path| is_executable(path))
                })
            && let Some(rustup) = rustup.to_str()
        {
            // Mise selects the version and Rustup resolves the binaries behind its shared proxies.
            let resolve = |bin| {
                let path = self.executable_path(&[
                    "exec",
                    &tool,
                    "--",
                    rustup,
                    "which",
                    "--toolchain",
                    version,
                    bin,
                ])?;
                fs::canonicalize(path).ok()
            };
            return (resolve("rustc"), resolve("cargo"));
        }
        let executable = self.executable_path(&["which", binary, &selector]);
        let cargo = (environment == Environment::Rust)
            .then(|| self.executable_path(&["which", "cargo", &selector]))
            .flatten();
        (executable, cargo)
    }

    fn executable_path(&self, args: &[&str]) -> Option<PathBuf> {
        let output = self.run(args, true, None).ok()?;
        let path = Path::new(output.trim());
        if !path.is_absolute() || !is_executable(path) {
            return None;
        }
        Some(path.to_owned())
    }

    fn run(&self, args: &[&str], offline: bool, proxy: Option<&str>) -> Result<String> {
        let command = self.command(args, offline);
        command::output(command, &format!("mise {}", args.join(" ")), proxy)
    }

    pub(crate) fn command(&self, args: &[&str], offline: bool) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.home)
            .args(args)
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_NOT_FOUND_AUTO_INSTALL", "0");
        if offline {
            command
                .env("MISE_OFFLINE", "1")
                .env("RUSTUP_AUTO_INSTALL", "0");
        }
        if let Ok(path) = env::join_paths(search_paths(&self.home)) {
            command.env("PATH", path);
        }
        command
    }
}

fn mise_binary(environment: Environment) -> Option<&'static str> {
    match environment {
        Environment::Node => Some("node"),
        Environment::Go => Some("go"),
        Environment::Rust => Some("rustc"),
        Environment::Flutter => Some("flutter"),
        Environment::Python => Some("python3"),
        Environment::Java => Some("java"),
        Environment::Elasticsearch => Some("elasticsearch"),
        _ => None,
    }
}

fn mise_install_name(environment: Environment) -> &'static str {
    match environment {
        // Older Mise releases map the shorthand to the archived asdf plugin.
        Environment::Elasticsearch => "vfox:jdx/vfox-elasticsearch",
        _ => environment.key(),
    }
}

fn mise_environment(name: &str) -> Option<Environment> {
    let short = match name.split_once(':') {
        Some(("vfox", "jdx/vfox-elasticsearch"))
        | Some(("asdf", "mise-plugins/mise-elasticsearch")) => "elasticsearch",
        Some(("core" | "http" | "asdf" | "vfox", short)) => short,
        Some(_) => return None,
        None if name == "vfox-jdx-vfox-elasticsearch" => "elasticsearch",
        None => name,
    };
    Environment::SIDEBAR
        .into_iter()
        .find(|environment| Mise::supports(*environment) && environment.key() == short)
}

pub(crate) fn valid_mise_version(version: &str) -> bool {
    version.len() <= 128
        && version
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_+".contains(&byte))
}

#[derive(Deserialize)]
struct MiseVersion {
    version: String,
    install_path: Option<PathBuf>,
    #[serde(default)]
    installed: bool,
    #[serde(default)]
    active: bool,
}

type MiseVersions = BTreeMap<String, Vec<MiseVersion>>;

fn version_numbers(version: Option<&str>) -> Vec<u32> {
    version
        .unwrap_or_default()
        .split(|ch: char| !ch.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .take(3)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn mise_catalog_reads_remote_versions_through_proxy_and_sorts_them() -> Result<()> {
        let home = env::temp_dir().join(format!("usenv-mise-catalog-{}", std::process::id()));
        fs::create_dir(&home)?;
        let result = (|| -> Result<()> {
            let executable = home.join("mise");
            fs::write(
                &executable,
                r#"#!/bin/sh
[ "$#" -eq 2 ] && [ "$1" = ls-remote ] && [ "$2" = node ] || exit 1
[ "$https_proxy" = 'http://127.0.0.1:7890' ] || exit 2
printf '%s\n' 22.9.1 22.10.0-rc.1 v24.1.0 22.10.0 v24.1.0 'not a version'
"#,
            )?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
            let mise = Mise {
                home: home.clone(),
                executable,
            };
            assert_eq!(
                mise.available_versions(Environment::Node, Some("http://127.0.0.1:7890"))?,
                ["v24.1.0", "22.10.0", "22.10.0-rc.1", "22.9.1"]
            );
            Ok(())
        })();
        fs::remove_dir_all(home)?;
        result
    }

    #[test]
    fn mise_install_passes_version_and_proxy_and_rejects_tool_syntax() -> Result<()> {
        let home = env::temp_dir().join(format!("usenv-mise-install-{}", std::process::id()));
        fs::create_dir(&home)?;
        let result = (|| -> Result<()> {
            let executable = home.join("mise");
            fs::write(
                &executable,
                r#"#!/bin/sh
[ "$#" -eq 2 ] && [ "$1" = install ] && [ "$2" = go@1.24.2 ] || exit 1
[ "$https_proxy" = 'http://127.0.0.1:7890' ] || exit 2
[ "$MISE_AUTO_INSTALL" = 0 ] || exit 3
"#,
            )?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
            let mise = Mise {
                home: home.clone(),
                executable,
            };
            mise.install(Environment::Go, "1.24.2", Some("http://127.0.0.1:7890"), &|_| {})?;
            assert!(
                mise.install(Environment::Go, "path:/tmp/go", None, &|_| {})
                    .unwrap_err()
                    .to_string()
                    .contains("请输入 Mise 版本")
            );
            Ok(())
        })();
        fs::remove_dir_all(home)?;
        result
    }

    #[test]
    fn mise_listing_keeps_installed_node_and_flutter_with_their_current_status()
    -> anyhow::Result<()> {
        let path = env::temp_dir().join(format!("usenv-mise-{}", std::process::id()));
        fs::create_dir(&path)?;
        let result = (|| -> anyhow::Result<()> {
            let node = path.join("node/26.1.0");
            let flutter = path.join("flutter/3.47.2");
            let previous = path.join("flutter/3.44.0");
            for (root, command) in [
                (&node, "node"),
                (&flutter, "flutter"),
                (&previous, "flutter"),
            ] {
                let binary = root.join("cli programs").join(command);
                fs::create_dir_all(binary.parent().unwrap())?;
                fs::write(&binary, "#!/bin/sh\n")?;
                fs::set_permissions(binary, fs::Permissions::from_mode(0o700))?;
            }
            let json = serde_json::json!({
                "node": [{"version":"26.1.0", "install_path":node, "active":true, "installed":true}],
                "asdf:flutter": [
                    {"version":"3.47.2", "install_path":flutter, "active":true, "installed":true},
                    {"version":"3.44.0", "install_path":previous, "active":false, "installed":true},
                    {"version":"latest", "install_path":flutter, "active":false, "installed":false}
                ]
            });
            let entries = serde_json::from_value(json)?;
            let executable = path.join("mise");
            fs::write(
                &executable,
                r#"#!/bin/sh
[ "$1" = which ] || exit 1
tool="${3#--tool=}"
version="${tool##*@}"
printf '%s/%s/%s/cli programs/%s\n' "$PWD" "$2" "$version" "$2"
"#,
            )?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
            let mise = Mise {
                home: path.clone(),
                executable,
            };
            let unknown = mise.installed_tools(&entries, None);
            assert_eq!(unknown.len(), 3);
            assert!(unknown.iter().all(|tool| tool.active.is_none()));
            let tools = mise.installed_tools(&entries, Some(&entries));
            assert_eq!(tools.len(), 3);
            assert!(tools.iter().any(|tool| {
                tool.environment == Environment::Node
                    && tool.active == Some(true)
                    && tool.version == "26.1.0"
                    && tool
                        .executable
                        .as_ref()
                        .is_some_and(|binary| binary.ends_with("cli programs/node"))
            }));
            assert!(
                tools
                    .iter()
                    .any(|tool| tool.environment == Environment::Flutter
                        && tool.active == Some(true)
                        && tool.version == "3.47.2")
            );
            assert!(
                tools
                    .iter()
                    .any(|tool| tool.environment == Environment::Flutter
                        && tool.active == Some(false)
                        && tool.version == "3.44.0")
            );
            Ok(())
        })();
        fs::remove_dir_all(path)?;
        result
    }

    #[test]
    fn mise_rust_resolves_each_version_from_the_shared_rustup_proxy_directory() -> Result<()> {
        let home = env::temp_dir().join(format!("usenv-mise-rust-{}", std::process::id()));
        fs::create_dir(&home)?;
        let result = (|| -> Result<()> {
            let proxies = home.join(".cargo/bin");
            fs::create_dir_all(&proxies)?;
            fs::write(proxies.join("rustup"), "#!/bin/sh\nexit 1\n")?;
            fs::set_permissions(proxies.join("rustup"), fs::Permissions::from_mode(0o700))?;
            for proxy in ["rustc", "cargo"] {
                std::os::unix::fs::symlink("rustup", proxies.join(proxy))?;
            }

            let mut versions = Vec::new();
            for (version, active) in [("1.100.0", true), ("1.99.0", false), ("1.98.0", false)] {
                let install_path = home.join(format!("installs/rust/{version}"));
                fs::create_dir_all(install_path.parent().unwrap())?;
                std::os::unix::fs::symlink(&proxies, &install_path)?;
                versions.push(serde_json::json!({
                    "version": version, "install_path": install_path,
                    "active": active, "installed": true,
                }));
                if version != "1.98.0" {
                    let bin = home.join(format!("custom rustup/toolchains/{version}/bin"));
                    fs::create_dir_all(&bin)?;
                    for name in ["rustc", "cargo"] {
                        let binary = bin.join(name);
                        fs::write(&binary, "#!/bin/sh\n")?;
                        fs::set_permissions(binary, fs::Permissions::from_mode(0o700))?;
                    }
                }
            }

            let executable = home.join("mise");
            fs::write(
                &executable,
                r#"#!/bin/sh
[ "$MISE_OFFLINE" = 1 ] && [ "$RUSTUP_AUTO_INSTALL" = 0 ] || exit 1
if [ "$1" = which ] && [ "$2" = rustup ]; then
    [ "$3" != --tool=rust@1.99.0 ] || exit 1
    printf '%s/.cargo/bin/rustup\n' "$PWD"
    exit 0
fi
[ "$#" -eq 8 ] && [ "$1" = exec ] && [ "$2" = "rust@$7" ] && [ "$3" = -- ] || exit 1
[ "$4" -ef "./.cargo/bin/rustup" ] && [ "$5" = which ] && [ "$6" = --toolchain ] || exit 1
path="$PWD/custom rustup/toolchains/$7/bin/$8"
[ -x "$path" ] && printf '%s\n' "$path"
"#,
            )?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
            let mise = Mise {
                home: home.clone(),
                executable,
            };
            let entries = serde_json::from_value(serde_json::json!({ "rust": versions }))?;
            let tools = mise.installed_tools(&entries, Some(&entries));
            assert_eq!(tools.len(), 3);
            for (tool, (version, active)) in
                tools.iter().zip([("1.100.0", true), ("1.99.0", false)])
            {
                let bin =
                    fs::canonicalize(home.join(format!("custom rustup/toolchains/{version}/bin")))?;
                assert_eq!(tool.environment, Environment::Rust);
                assert_eq!(tool.name, "rust");
                assert_eq!(tool.version, version);
                assert_eq!(tool.active, Some(active));
                assert_eq!(tool.executable, Some(bin.join("rustc")));
                assert_eq!(tool.cargo_executable, Some(bin.join("cargo")));
            }
            assert!(tools[2].executable.is_none());
            assert!(tools[2].cargo_executable.is_none());
            Ok(())
        })();
        fs::remove_dir_all(home)?;
        result
    }
}
