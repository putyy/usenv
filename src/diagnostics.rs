use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::brew::Environment;
use crate::command;
use crate::config::ServicePaths;

const SHELL_TIMEOUT: Duration = Duration::from_secs(12);
const LOG_BYTES: u64 = 12 * 1024;
const CONFIG_BYTES: u64 = 1024 * 1024;

const SHELL_PROBE: &str = r#"
builtin exec 3>| "$1" || exit 1
shift
builtin printf 'ready\0' >&3
for name in "$@"; do
    builtin printf 'check\0%s\0' "$name" >&3
    kind=$(builtin whence -w -- "$name") || true
    executable=$(builtin whence -p -- "$name") || true
    alternatives=$(builtin whence -pa -- "$name") || true
    resolved=''
    version=''
    if [[ "$kind" == *': hashed' ]]; then
        executable=$(builtin whence -- "$name") || true
    fi
    builtin printf 'lookup\0%s\0%s\0%s\0%.2500s\0' "$name" "$kind" "$executable" "$alternatives" >&3
    if [[ -n "$executable" && ( "$kind" == *': command' || "$kind" == *': hashed' ) ]]; then
        resolved="${executable:A}"
        if [[ ( "$name" == java || "$name" == javac ) && "$executable" == "/usr/bin/$name" ]]; then
            builtin printf 'command\0/usr/libexec/java_home\0-F\0' >&3
            system_jdk=$(/usr/libexec/java_home -F 2>/dev/null) || system_jdk=''
            if [[ -z "$system_jdk" || ! -x "$system_jdk/bin/$name" ]]; then
                builtin printf 'version\0%s\0macOS 未找到可用的 Java 运行时\0\0' "$name" >&3
                continue
            fi
            resolved="$system_jdk/bin/$name"
        fi
        if [[ "${resolved:t}" == mise || "${resolved:t}" == rustup ]]; then
            builtin printf 'command\0%s\0which %s\0' "$resolved" "$name" >&3
            resolved=$("$resolved" which "$name" 2>/dev/null) || true
        fi
        if [[ -n "$resolved" && ( "$name" == rustc || "$name" == cargo ) ]]; then
            rustup="${resolved:h}/rustup"
            if [[ -x "$rustup" && "$rustup" -ef "$resolved" ]]; then
                builtin printf 'command\0%s\0which %s\0' "$rustup" "$name" >&3
                resolved=$("$rustup" which "$name" 2>/dev/null) || true
            fi
        fi
        if [[ "$resolved" == /* && "$resolved" != "$executable" ]]; then
            builtin printf 'resolved\0%s\0' "$resolved" >&3
        fi
        case "$name" in
            flutter)
                builtin printf 'flutter\0' >&3
                flutter_root="${resolved:h:h}"
                if [[ -f "$flutter_root/version" ]]; then
                    IFS= read -r version < "$flutter_root/version" || true
                fi
                ;;
            *)
                case "$name" in
                    go) argument=version ;;
                    java|javac) argument=-version ;;
                    *) argument=--version ;;
                esac
                builtin printf 'command\0%s\0%s\0' "$executable" "$argument" >&3
                if [[ "$name" == go ]]; then
                    version=$(GOTOOLCHAIN=local "$executable" "$argument" </dev/null 2>&1) || true
                else
                    version_status=0
                    version=$("$executable" "$argument" </dev/null 2>&1) || version_status=$?
                    if [[ "$name" == java && "$version_status" == 0 ]]; then
                        builtin printf 'java_runtime\0%s\0%s\0%.1500s\0%s\0' \
                            "$executable" "$resolved" "$version" "${JAVA_HOME-}" >&3
                    fi
                fi
                ;;
        esac
        builtin printf 'version\0%s\0%.1500s\0%s\0' "$name" "$version" "$resolved" >&3
    fi
done
if [[ "$1" == java && -n "${JAVA_HOME-}" ]]; then
    builtin printf 'java_home\0%s\0' "$JAVA_HOME" >&3
fi
builtin printf 'done\0' >&3
"#;

#[derive(Clone)]
pub struct CurrentJava {
    pub executable: PathBuf,
    pub version: Option<String>,
    pub java_home: Option<PathBuf>,
}

pub struct TerminalReport {
    pub summary: String,
    pub java: Option<CurrentJava>,
}

pub fn terminal(
    environment: Environment,
    directory: Option<&Path>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<TerminalReport> {
    let commands = match environment {
        Environment::Go => &["go"][..],
        Environment::Java => &["java", "javac"],
        Environment::Python => &["python3", "pip3", "uv"],
        Environment::Php => &["php", "composer"],
        Environment::Node => &["node", "npm", "npx", "pnpm", "yarn"],
        Environment::Flutter => &["flutter", "dart"],
        Environment::Rust => &["rustc", "cargo"],
        _ => bail!("该环境没有终端版本诊断"),
    };
    probe_terminal(commands, directory, log)
}

pub fn current_java() -> Result<Option<CurrentJava>> {
    Ok(probe_terminal(&["java"], None, &|_| {})?.java)
}

fn probe_terminal(
    commands: &[&str],
    directory: Option<&Path>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<TerminalReport> {
    let home = env::var_os("HOME").context("无法确定当前用户的主目录")?;
    let directory = directory.unwrap_or_else(|| Path::new(&home));
    ensure!(
        directory.is_absolute() && directory.is_dir(),
        "找不到诊断目录 {}",
        directory.display()
    );
    let events = tempfile::NamedTempFile::new().context("无法创建诊断输出")?;
    let mut shell_errors = tempfile::tempfile().context("无法创建 zsh 错误输出")?;
    let mut reader = BufReader::new(events.as_file());
    let session = format!("新建 zsh 登录会话 · {}", directory.display());
    let mut progress = TerminalProgress {
        summary: format!("{session}\n"),
        step: "启动 zsh".into(),
        ..TerminalProgress::default()
    };
    log(&session);
    log("正在启动 /bin/zsh 并读取启动配置…");
    let mut probe = Command::new("/bin/zsh");
    probe
        .args(["-lic", SHELL_PROBE, "usenv-terminal-diagnostics"])
        .arg(events.path())
        .args(commands)
        .current_dir(directory)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("TERM", "dumb")
        .env_remove("MISE_SHELL")
        .env_remove("__MISE_ORIG_PATH")
        .env_remove("__MISE_DIFF")
        .env_remove("__MISE_SESSION")
        .env("MISE_AUTO_INSTALL", "false")
        .env("MISE_NOT_FOUND_AUTO_INSTALL", "false")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("COREPACK_ENABLE_NETWORK", "0")
        .env("COREPACK_ENABLE_AUTO_PIN", "0")
        .env("YARN_ENABLE_NETWORK", "0")
        .env("npm_config_manage_package_manager_versions", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(shell_errors.try_clone()?);
    // The login shell can open /dev/tty even when its standard streams are redirected.
    // Only the async-signal-safe system call runs between fork and exec.
    unsafe {
        probe.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = probe.spawn().context("无法启动 zsh 诊断")?;
    let started = Instant::now();
    loop {
        if let Err(error) = progress.read_available(&mut reader, log) {
            stop_terminal_probe(&mut child);
            return Err(error);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                progress.read_available(&mut reader, log)?;
                if !status.success() || !progress.done {
                    log_shell_errors(&mut shell_errors, log);
                }
                ensure!(status.success(), "zsh 在{}时退出（{status}）", progress.step);
                ensure!(progress.done, "zsh 未完成检测，请检查终端启动文件");
                return Ok(TerminalReport {
                    summary: progress.summary,
                    java: progress.java,
                });
            }
            Ok(None) => (),
            Err(error) => {
                stop_terminal_probe(&mut child);
                return Err(error).context("无法读取 zsh 退出状态");
            }
        }
        if started.elapsed() >= SHELL_TIMEOUT {
            stop_terminal_probe(&mut child);
            let _ = progress.read_available(&mut reader, log);
            log_shell_errors(&mut shell_errors, log);
            bail!("终端检测超过 12 秒，最后一步：{}", progress.step);
        }
        thread::sleep(Duration::from_millis(30));
    }
}

fn log_shell_errors(stderr: &mut File, log: &dyn Fn(&str)) {
    let Ok(output) = file_tail(stderr, 4096) else { return };
    for line in output.lines().map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('+'))
    {
        log(line);
    }
}

fn stop_terminal_probe(child: &mut Child) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--"])
        .arg(format!("-{}", child.id()))
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Default)]
struct TerminalProgress {
    field: Vec<u8>,
    record: Vec<String>,
    summary: String,
    step: String,
    done: bool,
    java: Option<CurrentJava>,
}

impl TerminalProgress {
    fn read_available(&mut self, reader: &mut impl BufRead, log: &dyn Fn(&str)) -> Result<()> {
        const MAX_FIELD_BYTES: usize = 8192;
        loop {
            let remaining = (MAX_FIELD_BYTES + 1 - self.field.len()) as u64;
            if (&mut *reader).take(remaining).read_until(0, &mut self.field)? == 0 {
                return Ok(());
            }
            ensure!(self.field.len() <= MAX_FIELD_BYTES, "终端检测输出字段过长");
            if self.field.last() != Some(&0) {
                // The shell can still be writing this field.
                return Ok(());
            }
            self.field.pop();
            self.record.push(String::from_utf8_lossy(&self.field).into_owned());
            self.field.clear();
            let size = match self.record[0].as_str() {
                "ready" | "flutter" | "done" => 1,
                "check" | "resolved" | "java_home" => 2,
                "command" => 3,
                "version" => 4,
                "lookup" | "java_runtime" => 5,
                _ => bail!("无法识别终端检测输出"),
            };
            if self.record.len() == size {
                let record = std::mem::take(&mut self.record);
                self.record_event(record, log);
            }
        }
    }

    fn record_event(&mut self, record: Vec<String>, log: &dyn Fn(&str)) {
        let [kind, fields @ ..] = record.as_slice() else { return };
        match (kind.as_str(), fields) {
            ("ready", []) => log("zsh 启动完成"),
            ("check", [name]) => {
                self.step = format!("查找 {name}");
                log(&self.step);
            }
            ("lookup", [name, kind, executable, alternatives]) => {
                let _ = writeln!(self.summary, "\n{name}");
                if kind.ends_with(": alias") || kind.ends_with(": function") {
                    self.report("被 zsh 的别名或函数覆盖", log);
                }
                if executable.is_empty() {
                    self.report("PATH 中未找到可执行文件", log);
                } else {
                    self.report(executable, log);
                    for alternate in alternatives.lines().filter(|path| *path != executable.as_str()).take(4) {
                        self.report(&format!("PATH 中另有 {alternate}"), log);
                    }
                }
            }
            ("resolved", [path]) => self.report(&format!("实际文件 {path}"), log),
            ("command", [program, arguments]) => {
                self.step = format!("{} {arguments}", command::shell_quote(program));
                log(&format!("$ {}", self.step));
            }
            ("flutter", []) => {
                self.step = "读取 Flutter 版本文件".into();
                log(&self.step);
            }
            ("version", [name, version, resolved]) => {
                if !version.trim().is_empty() {
                    self.report(version.trim(), log);
                } else if name == "flutter"
                    && let Some(version) = flutter_version(Path::new(resolved))
                {
                    self.report(&version, log);
                } else {
                    self.report("未返回版本信息", log);
                }
            }
            ("java_runtime", [executable, resolved, version, home]) => {
                let executable = [resolved, executable].into_iter()
                    .map(Path::new)
                    .find(|path| path.is_absolute());
                if let Some(executable) = executable {
                    let version = version.lines().map(str::trim)
                        .find(|line| line.starts_with("openjdk ") || line.starts_with("java "))
                        .map(str::to_owned);
                    self.java = Some(CurrentJava {
                        executable: executable.to_owned(),
                        version,
                        java_home: Path::new(home).is_absolute().then(|| PathBuf::from(home)),
                    });
                }
            }
            ("java_home", [path]) => {
                self.summary.push_str("\nJAVA_HOME\n");
                self.report(path, log);
            }
            ("done", []) => {
                self.step = "等待 zsh 退出".into();
                self.done = true;
            }
            _ => (),
        }
    }

    fn report(&mut self, text: &str, log: &dyn Fn(&str)) {
        let _ = writeln!(self.summary, "{text}");
        for line in text.lines() {
            log(line);
        }
    }
}

fn flutter_version(executable: &Path) -> Option<String> {
    let path = executable.parent()?.join("cache/flutter.version.json");
    let version: serde_json::Value = serde_json::from_str(&read_config(&path)?).ok()?;
    version.get("frameworkVersion")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod terminal_tests {
    use std::cell::RefCell;
    use std::io::Cursor;

    use super::*;

    #[test]
    fn displays_complete_events_and_retains_an_incomplete_field() -> Result<()> {
        let lines = RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_owned());
        let mut progress = TerminalProgress::default();
        let mut first = Cursor::new(b"ready\x00check\x00no");
        progress.read_available(&mut first, &log)?;
        assert_eq!(lines.borrow().first().map(String::as_str), Some("zsh 启动完成"));
        assert_eq!(lines.borrow().len(), 1);
        assert!(!progress.done);

        let mut second = Cursor::new(
            b"de\x00lookup\x00node\x00node: command\x00/opt/homebrew/bin/node\x00\x00command\x00/opt/homebrew/bin/node\x00--version\x00",
        );
        progress.read_available(&mut second, &log)?;
        assert!(progress.summary.contains("/opt/homebrew/bin/node"));
        assert!(progress.step.ends_with("--version"));
        assert!(!progress.done);

        let mut third = Cursor::new(b"version\x00node\x00v24.0.0\x00/opt/homebrew/bin/node\x00done\x00");
        progress.read_available(&mut third, &log)?;
        assert_eq!(lines.borrow().last().map(String::as_str), Some("v24.0.0"));
        assert!(progress.done);
        Ok(())
    }
}

#[derive(Clone)]
pub struct ServiceReport {
    pub details: String,
    pub connection: Option<String>,
}

pub fn service(
    environment: Environment,
    paths: &ServicePaths,
    managed_root: Option<&Path>,
    launch_plist: Option<&Path>,
) -> Result<ServiceReport> {
    ensure!(environment.service_label().is_some(), "该环境没有可诊断的服务");
    let job = match managed_root {
        Some(root) => LaunchJob::read(&root.join("service.plist")),
        None => launch_plist.and_then(LaunchJob::read),
    }
    .unwrap_or_default();
    let config = paths.config.as_deref().and_then(read_config).unwrap_or_default();
    let (ports, socket) = endpoints(environment, &job, paths, &config, managed_root);
    let mut details = String::new();
    if let Some(path) = &paths.config {
        let _ = writeln!(details, "配置文件 {}", path.display());
    }
    if let Some(socket) = &socket {
        let _ = writeln!(details, "Unix socket {socket}");
    }
    for port in ports.iter().take(12) {
        let _ = writeln!(details, "\nTCP {port}");
        match listeners(*port) {
            Ok(output) if output.is_empty() => {
                let _ = writeln!(details, "当前用户未发现监听进程");
            }
            Ok(output) => {
                let _ = writeln!(details, "{output}");
            }
            Err(error) => {
                let _ = writeln!(details, "{error:#}");
            }
        }
    }
    if ports.is_empty() && socket.is_none() {
        let _ = writeln!(details, "配置中未找到可识别的本地监听地址");
    }
    if ports.len() > 12 {
        let _ = writeln!(details, "另有 {} 个配置端口", ports.len() - 12);
    }
    let _ = writeln!(details, "\n近期日志");
    let logs = log_paths(environment, paths, &job, &config, managed_root);
    let mut shown = 0;
    for path in logs {
        if shown >= 3 {
            break;
        }
        let Some(mut file) = regular_file(&path) else {
            continue;
        };
        match file_tail(&mut file, LOG_BYTES) {
            Ok(output) if !output.trim().is_empty() => {
                let lines = output.lines().rev().take(70).collect::<Vec<_>>();
                let output = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
                let _ = writeln!(details, "\n{}\n{output}", path.display());
                shown += 1;
            }
            _ => (),
        }
    }
    if shown == 0 {
        let _ = writeln!(details, "暂未找到有内容的日志");
    }
    Ok(ServiceReport {
        details,
        connection: connection(environment, paths, ports.first().copied(), socket.as_deref(), managed_root.is_some()),
    })
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchJob {
    #[serde(default)]
    program_arguments: Vec<String>,
    working_directory: Option<PathBuf>,
    standard_out_path: Option<PathBuf>,
    standard_error_path: Option<PathBuf>,
    #[serde(default)]
    environment_variables: std::collections::BTreeMap<String, String>,
}

impl LaunchJob {
    fn read(path: &Path) -> Option<Self> {
        path.metadata().ok()?.is_file().then_some(())?;
        let output = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--"])
            .arg(path)
            .output()
            .ok()?;
        output.status.success().then_some(())?;
        serde_json::from_slice(&output.stdout).ok()
    }

    fn arg(&self, key: &str) -> Option<&str> {
        option(&self.program_arguments, key)
    }

    fn var(&self, key: &str) -> Option<&str> {
        self.environment_variables.get(key).map(String::as_str)
    }
}

fn option<'a>(arguments: &'a [String], key: &str) -> Option<&'a str> {
    arguments.iter().enumerate().rev().find_map(|(index, argument)| {
        if argument == key {
            arguments.get(index + 1).map(String::as_str)
        } else {
            argument.strip_prefix(key)?.strip_prefix('=')
        }
    })
}

fn assignment<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    config.lines().rev().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim().eq_ignore_ascii_case(key)).then(|| config_value(value))
    })
}

fn directives<'a>(
    config: &'a str,
    key: &'a str,
) -> impl DoubleEndedIterator<Item = &'a str> + 'a {
    config.lines().flat_map(move |line| {
        let line = line.split('#').next().unwrap_or_default();
        line.split([';', '{', '}']).filter_map(move |entry| {
            let entry = entry.trim();
            let (name, value) = entry.split_once(char::is_whitespace)?;
            (name == key).then(|| value.trim())
        })
    })
}

fn config_value(value: &str) -> &str {
    value
        .split(['#', ';'])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\''])
}

fn endpoints(
    environment: Environment,
    job: &LaunchJob,
    paths: &ServicePaths,
    config: &str,
    managed_root: Option<&Path>,
) -> (BTreeSet<u16>, Option<String>) {
    let tcp_port = |value: &str| value.parse::<u16>().ok().filter(|port| *port != 0);
    let address_port = |value: &str| match value.rsplit_once(':') {
        Some((_, port)) => tcp_port(port),
        None => tcp_port(value),
    };
    let mut ports = BTreeSet::new();
    let mut socket = None;
    let port = match environment {
        Environment::Mysql | Environment::Mariadb => {
            let sections = config_lines(config, &["mysqld", "server", "mariadb", "mariadbd"]);
            let managed_socket = (environment == Environment::Mariadb)
                .then(|| managed_root.map(|root| root.join("mariadb.sock")))
                .flatten();
            socket = job
                .arg("--socket")
                .or_else(|| managed_socket.as_deref().and_then(Path::to_str))
                .or_else(|| assignment(&sections, "socket"))
                .map(str::to_owned);
            let skip_networking = [
                "skip-networking",
                "skip_networking",
            ]
            .into_iter()
            .any(|key| {
                sections.lines().any(|line| {
                    line.eq_ignore_ascii_case(key)
                        || assignment(line, key).is_some_and(|value| {
                            matches!(value.to_ascii_lowercase().as_str(), "1" | "on" | "true")
                        })
                })
            });
            if skip_networking || job.program_arguments.iter().any(|arg| arg == "--skip-networking") {
                None
            } else {
                let default = if managed_root.is_some() && environment == Environment::Mariadb { 3307 } else { 3306 };
                job.arg("--port")
                    .or_else(|| assignment(&sections, "port"))
                    .map_or(Some(default), tcp_port)
            }
        }
        Environment::Postgresql => {
            let runtime_port = paths
                .data
                .as_ref()
                .and_then(|dir| read_config(&dir.join("postmaster.pid")))
                .and_then(|pid| pid.lines().nth(3).and_then(tcp_port));
            runtime_port.or_else(|| {
                job.arg("-p")
                    .or_else(|| job.arg("--port"))
                    .or_else(|| {
                        job.program_arguments
                            .windows(2)
                            .rev()
                            .find_map(|args| (args[0] == "-c").then_some(&args[1])?.strip_prefix("port="))
                    })
                    .or_else(|| assignment(config, "port"))
                    .map_or(Some(5432), tcp_port)
            })
        }
        Environment::Redis => {
            socket = job
                .arg("--unixsocket")
                .or_else(|| directives(config, "unixsocket").next_back())
                .map(|value| config_value(value).to_owned());
            job.arg("--port")
                .or_else(|| directives(config, "port").next_back())
                .map_or(Some(6379), tcp_port)
        }
        Environment::Nginx => {
            for address in directives(config, "listen").filter_map(first_word) {
                if let Some(path) = address.strip_prefix("unix:") {
                    socket = Some(path.to_owned());
                } else if let Some(port) = address_port(address) {
                    ports.insert(port);
                }
            }
            None
        }
        Environment::Php => {
            let pools = php_pool_sources(paths.config.as_deref(), config);
            for source in std::iter::once(config).chain(pools.iter().map(String::as_str)) {
                for address in source.lines().filter_map(|line| assignment(line, "listen")) {
                    if address.starts_with('/') {
                        socket = Some(address.to_owned());
                    } else if let Some(port) = address_port(address) {
                        ports.insert(port);
                    }
                }
            }
            None
        }
        Environment::Meilisearch => job
            .arg("--http-addr")
            .or_else(|| job.var("MEILI_HTTP_ADDR"))
            .or_else(|| assignment(config, "http_addr"))
            .map_or(Some(7700), address_port),
        Environment::Elasticsearch => job
            .arg("-Ehttp.port")
            .or_else(|| {
                config.lines().rev().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name.trim() == "http.port").then(|| config_value(value))
                })
            })
            .and_then(|value| tcp_port(value).or_else(|| value.split('-').next().and_then(tcp_port)))
            .or(managed_root.is_some().then_some(9200)),
        _ => None,
    };
    ports.extend(port);
    (ports, socket)
}

fn config_lines(config: &str, sections: &[&str]) -> String {
    let mut included = true;
    let mut selected = String::new();
    for line in config.lines() {
        let line = config_value(line);
        if let Some(name) = line.strip_prefix('[').and_then(|name| name.strip_suffix(']')) {
            included = sections.contains(&name);
        } else if included {
            let _ = writeln!(selected, "{line}");
        }
    }
    selected
}

fn php_pool_sources(path: Option<&Path>, config: &str) -> Vec<String> {
    let Some(parent) = path.and_then(Path::parent) else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for include in config.lines().filter_map(|line| assignment(line, "include")) {
        if let Some(directory) = include.strip_suffix("/*.conf") {
            if let Ok(entries) = parent.join(directory).read_dir() {
                let paths = entries
                    .take(64)
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "conf"));
                sources.extend(paths.filter_map(|path| read_config(&path)));
            }
        } else if let Some(source) = read_config(&parent.join(include)) {
            sources.push(source);
        }
    }
    sources
}

fn listeners(port: u16) -> Result<String> {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "+c", "0", "-a", "-sTCP:LISTEN", "-i"])
        .arg(format!("TCP:{port}"))
        .output()
        .context("无法查询监听进程")?;
    if !output.status.success() && !output.stderr.is_empty() {
        bail!("无法查询监听进程");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn first_word(value: &str) -> Option<&str> {
    value.split_whitespace().next()
}

fn log_paths(
    environment: Environment,
    paths: &ServicePaths,
    job: &LaunchJob,
    config: &str,
    managed_root: Option<&Path>,
) -> Vec<PathBuf> {
    let cwd = job.working_directory.as_deref().or(managed_root);
    let mut logs = [job.standard_error_path.clone(), job.standard_out_path.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if let Some(root) = managed_root {
        logs.push(root.join("logs/service.log"));
    }
    let mut push = |value: Option<&str>| {
        if let Some(value) = value.filter(|value| !matches!(*value, "stderr" | "stdout" | "syslog" | "off" | "")) {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                logs.push(path);
            } else if let Some(cwd) = cwd {
                logs.push(cwd.join(path));
            }
        }
    };
    match environment {
        Environment::Nginx => {
            for value in directives(config, "error_log").filter_map(first_word) {
                push(Some(value));
            }
            for value in directives(config, "access_log").filter_map(first_word) {
                push(Some(value));
            }
        }
        Environment::Php => push(assignment(config, "error_log")),
        Environment::Redis => push(job.arg("--logfile").or_else(|| directives(config, "logfile").next_back())),
        Environment::Mysql | Environment::Mariadb => {
            let sections = config_lines(config, &["mysqld", "server", "mariadb", "mariadbd"]);
            push(
                job.arg("--log-error")
                    .or_else(|| assignment(&sections, "log_error"))
                    .or_else(|| assignment(&sections, "log-error")),
            );
            if let Some(directory) = &paths.data {
                logs.extend(newest_logs(directory, &["err"]));
            }
        }
        Environment::Postgresql => {
            if let Some(directory) = &paths.data
                && let Some(files) = read_config(&directory.join("current_logfiles"))
            {
                logs.extend(files.lines().filter_map(|line| {
                    let (_, name) = line.split_once(' ')?;
                    Some(directory.join(name.trim()))
                }));
            }
        }
        Environment::Elasticsearch => {
            if let Some(root) = managed_root {
                logs.extend(newest_logs(&root.join("logs"), &["log"]));
            }
        }
        _ => (),
    }
    let mut seen = BTreeSet::new();
    logs.retain(|path| {
        path.is_absolute()
            && path.file_name().is_some_and(|name| !name.to_string_lossy().starts_with("initial-password."))
            && seen.insert(path.clone())
    });
    logs
}

fn newest_logs(directory: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let Ok(entries) = directory.read_dir() else {
        return Vec::new();
    };
    let mut files = entries
        .take(256)
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let extension = path.extension()?.to_str()?;
            let metadata = entry.metadata().ok()?;
            (extensions.contains(&extension) && metadata.is_file())
                .then(|| (metadata.modified().ok(), path))
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| right.0.cmp(&left.0));
    files.into_iter().take(3).map(|(_, path)| path).collect()
}

fn connection(
    environment: Environment,
    paths: &ServicePaths,
    port: Option<u16>,
    socket: Option<&str>,
    managed: bool,
) -> Option<String> {
    let bin = |name: &str| {
        paths
            .executable
            .parent()
            .map(|directory| directory.join(name))
            .filter(|path| path.is_file())
            .and_then(|path| path.to_str().map(command::shell_quote))
            .unwrap_or_else(|| name.to_owned())
    };
    match environment {
        Environment::Mariadb if managed => Some(format!(
            "{} --no-defaults --protocol=SOCKET --socket={}",
            bin("mariadb"),
            command::shell_quote(socket?),
        )),
        Environment::Mysql | Environment::Mariadb => {
            let name = if environment == Environment::Mysql { "mysql" } else { "mariadb" };
            let endpoint = if let Some(port) = port {
                format!("--protocol=TCP --host=127.0.0.1 --port={port}")
            } else {
                format!("--socket={}", command::shell_quote(socket?))
            };
            Some(format!("{} {endpoint} --user=root --password", bin(name)))
        }
        Environment::Postgresql => Some(format!("{} -h 127.0.0.1 -p {} -d postgres", bin("psql"), port?)),
        Environment::Redis => {
            let endpoint = if let Some(port) = port {
                format!("-h 127.0.0.1 -p {port}")
            } else {
                format!("-s {}", command::shell_quote(socket?))
            };
            Some(format!("{} {endpoint}", bin("redis-cli")))
        }
        _ => None,
    }
}

fn regular_file(path: &Path) -> Option<File> {
    fs::metadata(path).ok()?.is_file().then_some(())?;
    File::open(path).ok()
}

fn read_config(path: &Path) -> Option<String> {
    let file = regular_file(path)?;
    (file.metadata().ok()?.len() <= CONFIG_BYTES).then_some(())?;
    let mut source = String::new();
    file.take(CONFIG_BYTES).read_to_string(&mut source).ok()?;
    Some(source)
}

fn file_tail(file: &mut File, max: u64) -> Result<String> {
    let start = file.metadata()?.len().saturating_sub(max);
    file.seek(SeekFrom::Start(start))?;
    let mut output = Vec::new();
    file.take(max).read_to_end(&mut output)?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}
