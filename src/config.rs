use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::brew::Environment;

#[derive(Clone, Copy)]
pub enum ConfigKind {
    Service,
    PhpIni,
    FpmIni,
}

#[derive(Clone)]
pub struct ServicePaths {
    pub executable: PathBuf,
    pub config: Option<PathBuf>,
    pub data: Option<PathBuf>,
    pub php_ini: Option<PathBuf>,
    pub initial_credentials: bool,
}

pub fn open(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "配置路径必须是绝对路径");
    let metadata = path
        .metadata()
        .with_context(|| format!("找不到或无法访问配置文件 {}", path.display()))?;
    ensure!(metadata.is_file(), "{} 不是配置文件", path.display());
    let status = Command::new("/usr/bin/open")
        .args(["-t", "--"])
        .arg(path)
        .status()
        .context("无法启动文本编辑器")?;
    ensure!(status.success(), "文本编辑器无法打开 {}", path.display());
    Ok(())
}

pub fn open_or_create(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "配置路径必须是绝对路径");
    let directory = path.parent().context("无法确定配置文件目录")?;
    fs::create_dir_all(directory)?;
    match OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error).with_context(|| format!("无法创建 {}", path.display())),
    }
    open(path)
}

pub fn open_directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.is_dir(),
        "找不到目录 {}",
        path.display()
    );
    let status = Command::new("/usr/bin/open").arg("--").arg(path).status()?;
    ensure!(status.success(), "Finder 无法打开 {}", path.display());
    Ok(())
}

pub fn php_ini(php_bin: &Path) -> Result<PathBuf> {
    php_loaded_ini(php_bin)?.context("此 PHP 版本没有加载 php.ini")
}

pub fn php_loaded_ini(php_bin: &Path) -> Result<Option<PathBuf>> {
    let output = command_text(
        Command::new(php_bin.join("php")).arg("--ini"),
        "读取 PHP 配置",
    )?;
    Ok(output
        .lines()
        .find_map(|line| {
            line.strip_prefix("Loaded Configuration File:")
                .map(str::trim)
        })
        .map(PathBuf::from)
        .filter(|path| path.is_absolute()))
}

pub fn mysql_socket(opt: &Path, config: &Path) -> Result<Option<String>> {
    let mut defaults_file = std::ffi::OsString::from("--defaults-file=");
    defaults_file.push(config);
    let options = command_text(
        Command::new(opt.join("bin/my_print_defaults"))
            .arg(defaults_file)
            .args(["mysqld", "server"]),
        "读取 MySQL 服务选项",
    )?;
    Ok(options
        .lines()
        .filter_map(|line| {
            line.strip_prefix("--socket=")
                .or_else(|| line.strip_prefix("--loose-socket="))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .next_back())
}

pub fn service_file(
    prefix: &Path,
    opt: &Path,
    formula: &str,
    environment: Environment,
    plist: &Path,
) -> Result<PathBuf> {
    LaunchJob::read(plist)?.config_file(prefix, opt, formula, environment)
}

pub fn service_paths(
    prefix: &Path,
    opt: &Path,
    formula: &str,
    environment: Environment,
    plist: &Path,
) -> Result<ServicePaths> {
    let job = LaunchJob::read(plist)?;
    let cwd = job.working_directory.as_deref().unwrap_or(Path::new("/"));
    let args = job.program_arguments.get(1..).unwrap_or_default();
    let executable = cwd.join(job.program()?);
    let config = job.config_file(prefix, opt, formula, environment).ok();
    let data = match environment {
        Environment::Mysql | Environment::Mariadb => {
            option(args, &["--datadir"]).map(|value| cwd.join(value))
        }
        Environment::Meilisearch => option(args, &["--db-path"])
            .or_else(|| {
                job.environment_variables
                    .get("MEILI_DB_PATH")
                    .map(String::as_str)
            })
            .map(|value| cwd.join(value))
            .or_else(|| {
                config_setting(config.as_deref(), "db_path", "=").map(|value| cwd.join(value))
            }),
        Environment::Postgresql => args
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, arg)| {
                arg.strip_prefix("--data-directory=")
                    .or_else(|| arg.strip_prefix("--data_directory="))
                    .or_else(|| option_at(args, index, "-c")?.strip_prefix("data_directory="))
            })
            .map(|value| cwd.join(value))
            .or_else(|| {
                config_setting(config.as_deref(), "data_directory", "=")
                    .map(|value| cwd.join(value))
            })
            .or_else(|| {
                option(args, &["-D"])
                    .or_else(|| job.environment_variables.get("PGDATA").map(String::as_str))
                    .map(|value| cwd.join(value))
            }),
        Environment::Redis => option(args, &["--dir"])
            .map(|value| cwd.join(value))
            .or_else(|| config_setting(config.as_deref(), "dir", " ").map(|value| cwd.join(value))),
        _ => None,
    };
    let php_ini = (environment == Environment::Php)
        .then(|| {
            option(args, &["-c", "--php-ini"])
                .map(|value| cwd.join(value))
                .map(|path| {
                    if path.is_dir() {
                        path.join("php.ini")
                    } else {
                        path
                    }
                })
                .or_else(|| php_loaded_ini(&opt.join("bin")).ok().flatten())
        })
        .flatten();
    Ok(ServicePaths {
        executable,
        config,
        data,
        php_ini,
        initial_credentials: false,
    })
}

fn config_setting(path: Option<&Path>, key: &str, separator: &str) -> Option<String> {
    let source = std::fs::read_to_string(path?).ok()?;
    source.lines().rev().find_map(|line| {
        let (name, value) = line.trim().split_once(separator)?;
        if name.trim() != key {
            return None;
        }
        let value = value.split('#').next()?.trim().trim_matches(['\'', '"']);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

impl LaunchJob {
    fn read(plist: &Path) -> Result<Self> {
        let mut command = Command::new("/usr/bin/plutil");
        command
            .args(["-convert", "json", "-o", "-", "--"])
            .arg(plist);
        let source = command_text(&mut command, "读取 Homebrew 服务启动项")
            .with_context(|| format!("无法读取 {}", plist.display()))?;
        serde_json::from_str(&source).context("无法解析 Homebrew 服务启动项")
    }

    fn program(&self) -> Result<&str> {
        self.program
            .as_deref()
            .or_else(|| self.program_arguments.first().map(String::as_str))
            .context("服务启动项缺少可执行文件")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchJob {
    program: Option<String>,
    #[serde(default)]
    program_arguments: Vec<String>,
    working_directory: Option<PathBuf>,
    root_directory: Option<PathBuf>,
    #[serde(default)]
    environment_variables: BTreeMap<String, String>,
}

impl LaunchJob {
    fn config_file(
        &self,
        prefix: &Path,
        opt: &Path,
        formula: &str,
        environment: Environment,
    ) -> Result<PathBuf> {
        ensure!(
            self.root_directory.is_none(),
            "服务使用了独立根目录，无法定位配置文件"
        );
        let program = self.program()?;
        let program_name = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str());
        let args = self.program_arguments.get(1..).unwrap_or_default();
        let cwd = self.working_directory.as_deref().unwrap_or(Path::new("/"));
        match (environment, program_name) {
            (Environment::Redis, Some("redis-server")) => {
                let file = args
                    .first()
                    .filter(|arg| !arg.starts_with('-'))
                    .context("Redis 启动项没有指定配置文件")?;
                Ok(cwd.join(file))
            }
            (Environment::Postgresql, Some("postgres" | "postmaster")) => {
                let config = args
                    .iter()
                    .enumerate()
                    .filter_map(|(index, arg)| {
                        arg.strip_prefix("--config_file=")
                            .or_else(|| arg.strip_prefix("--config-file="))
                            .or_else(|| option_at(args, index, "-c")?.strip_prefix("config_file="))
                    })
                    .next_back();
                if let Some(config) = config {
                    return Ok(cwd.join(config));
                }
                let directory = option(args, &["-D"])
                    .or_else(|| self.environment_variables.get("PGDATA").map(String::as_str))
                    .context("PostgreSQL 启动项没有指定配置目录")?;
                Ok(cwd.join(directory).join("postgresql.conf"))
            }
            (Environment::Mysql, Some("mysqld" | "mysqld_safe"))
            | (
                Environment::Mariadb,
                Some("mariadbd" | "mariadbd-safe" | "mysqld" | "mysqld_safe"),
            ) => {
                ensure!(
                    !args.iter().any(|arg| arg == "--no-defaults"),
                    "{formula} 启动项关闭了配置文件读取"
                );
                if let Some(file) = option(args, &["--defaults-file"])
                    .or_else(|| option(args, &["--defaults-extra-file"]))
                {
                    return Ok(cwd.join(file));
                }
                let config_dir = self
                    .environment_variables
                    .get(if environment == Environment::Mariadb {
                        "MARIADB_HOME"
                    } else {
                        "MYSQL_HOME"
                    })
                    .or_else(|| self.environment_variables.get("MYSQL_HOME"))
                    .map(|dir| cwd.join(dir))
                    .unwrap_or_else(|| prefix.join("etc"));
                Ok(config_dir.join("my.cnf"))
            }
            (Environment::Meilisearch, Some("meilisearch")) => {
                if let Some(file) = option(args, &["--config-file-path"]).or_else(|| {
                    self.environment_variables
                        .get("MEILI_CONFIG_FILE_PATH")
                        .map(String::as_str)
                }) {
                    return Ok(cwd.join(file));
                }
                let file = cwd.join("config.toml");
                ensure!(
                    file.is_file(),
                    "Homebrew 服务当前没有 Meilisearch 配置文件，请先启用独立配置"
                );
                Ok(file)
            }
            (Environment::Nginx, Some("nginx")) => {
                if let Some(file) = option(args, &["-c"]) {
                    let base = option(args, &["-p"])
                        .map(|path| cwd.join(path))
                        .unwrap_or_else(|| opt.to_owned());
                    return Ok(base.join(file));
                }
                Ok(prefix.join("etc/nginx/nginx.conf"))
            }
            (Environment::Php, Some("php-fpm")) => {
                let ini_override = || {
                    args.iter()
                        .enumerate()
                        .filter_map(|(index, _)| {
                            option_at(args, index, "-d")?.strip_prefix("fpm.config=")
                        })
                        .next_back()
                };
                if let Some(file) = option(args, &["-y", "--fpm-config"]).or_else(ini_override) {
                    return Ok(cwd.join(file));
                }
                if let Some(directory) = option(args, &["-p", "--prefix"]) {
                    return Ok(cwd.join(directory).join("etc/php-fpm.conf"));
                }
                let php_config = opt.join("bin/php-config");
                let version =
                    command_text(Command::new(php_config).arg("--version"), "读取 PHP 版本")?;
                let (major, minor) = version
                    .trim()
                    .split_once('.')
                    .context("无法识别 PHP 版本")?;
                let minor = minor.split('.').next().unwrap_or_default();
                let (major, minor) = (major.parse::<u32>()?, minor.parse::<u32>()?);
                Ok(prefix.join(format!("etc/php/{major}.{minor}/php-fpm.conf")))
            }
            _ => bail!("无法从 {formula} 的自定义启动项识别配置文件"),
        }
    }
}

fn option<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    (0..args.len())
        .rev()
        .find_map(|index| names.iter().find_map(|name| option_at(args, index, name)))
}

fn option_at<'a>(args: &'a [String], index: usize, name: &str) -> Option<&'a str> {
    match args[index].strip_prefix(name)? {
        "" => args
            .get(index + 1)
            .map(String::as_str)
            .filter(|value| !value.starts_with('-')),
        value if name.starts_with("--") => {
            value.strip_prefix('=').filter(|value| !value.is_empty())
        }
        value => Some(value),
    }
}

fn command_text(command: &mut Command, operation: &str) -> Result<String> {
    let Output { status, stdout, .. } = command
        .output()
        .with_context(|| format!("无法{operation}"))?;
    ensure!(status.success(), "{operation}失败（{status}）");
    String::from_utf8(stdout).with_context(|| format!("{operation}返回了无效文本"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_config_from_version_and_explicit_launch_arguments() -> Result<()> {
        let prefix = Path::new("/opt/homebrew");
        for (environment, formula, args, expected) in [
            (
                Environment::Redis,
                "redis@7",
                &[
                    "/opt/homebrew/opt/redis@7/bin/redis-server",
                    "redis settings/redis.conf",
                ][..],
                "/opt/homebrew/var/redis settings/redis.conf",
            ),
            (
                Environment::Postgresql,
                "postgresql@18",
                &[
                    "postgres",
                    "-D",
                    "/opt/homebrew/var/postgresql@18",
                    "-c",
                    "config_file=postgres config/main.conf",
                    "-c",
                    "port=5434",
                ],
                "/opt/homebrew/var/postgres config/main.conf",
            ),
            (
                Environment::Postgresql,
                "postgresql@17",
                &["postgres", "-D", "/opt/homebrew/var/postgresql@17"],
                "/opt/homebrew/var/postgresql@17/postgresql.conf",
            ),
            (
                Environment::Mysql,
                "mysql",
                &[
                    "mysqld_safe",
                    "--defaults-file=/custom profile/mysql.cnf",
                    "--datadir=/data/mysql",
                ],
                "/custom profile/mysql.cnf",
            ),
            (
                Environment::Mariadb,
                "mariadb@11.8",
                &["mariadbd-safe", "--datadir=/opt/homebrew/var/mysql"],
                "/opt/homebrew/etc/my.cnf",
            ),
            (
                Environment::Meilisearch,
                "meilisearch",
                &["meilisearch", "--config-file-path", "search/config.toml"],
                "/opt/homebrew/var/search/config.toml",
            ),
            (
                Environment::Nginx,
                "nginx",
                &["nginx", "-p", "/custom nginx/", "-cconf/main.conf"],
                "/custom nginx/conf/main.conf",
            ),
            (
                Environment::Php,
                "php@8.4",
                &[
                    "php-fpm",
                    "--fpm-config=/custom php/fpm.conf",
                    "-dfpm.config=/unused/fpm.conf",
                ],
                "/custom php/fpm.conf",
            ),
        ] {
            let job: LaunchJob = serde_json::from_value(serde_json::json!({
                "ProgramArguments": args,
                "WorkingDirectory": "/opt/homebrew/var",
            }))?;
            assert_eq!(
                job.config_file(
                    prefix,
                    &prefix.join("opt").join(formula),
                    formula,
                    environment
                )?,
                Path::new(expected)
            );
        }
        Ok(())
    }
}
