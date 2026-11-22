use std::borrow::Cow;
use std::cell::Cell;
use std::fmt;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

pub const PROXY_VARIABLES: [&str; 6] = [
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
];

#[derive(Debug)]
pub struct NeedsTerminal {
    operation: String,
    status: ExitStatus,
}

impl fmt::Display for NeedsTerminal {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "{} 需要管理员授权，请在终端按提示确认（{}）",
            self.operation, self.status
        )
    }
}

impl std::error::Error for NeedsTerminal {}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn output(mut command: Command, operation: &str, proxy: Option<&str>) -> Result<String> {
    let proxy = apply_proxy(&mut command, proxy);
    let result = command
        .output()
        .with_context(|| format!("无法执行 {operation}"))?;
    if result.status.success() {
        return String::from_utf8(result.stdout)
            .with_context(|| format!("无法读取 {operation} 的输出"));
    }

    if proxy.is_some_and(proxy_needs_redaction) {
        bail!(
            "{operation} 执行失败（{}），代理使用了身份验证",
            result.status
        );
    }

    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let mut details = format!("{}\n{}", stdout.trim(), stderr.trim());
    if let Some(proxy) = proxy {
        details = details.replace(proxy, "[代理]");
    }
    let details = details.trim();
    let start = details
        .char_indices()
        .rev()
        .nth(3999)
        .map_or(0, |(index, _)| index);
    bail!(
        "{operation} 执行失败（{}）\n{}",
        result.status,
        &details[start..]
    );
}

pub fn stream(
    mut command: Command,
    operation: &str,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let proxy = apply_proxy(&mut command, proxy);
    let credentials = proxy.and_then(|proxy| {
        proxy_credentials(proxy).map(|credentials| {
            if credentials.contains(':') {
                credentials
            } else {
                proxy
            }
        })
    });
    let needs_terminal = Cell::new(false);
    let log_line = |line: &str| {
        if line.contains("sudo:")
            && (line.contains("a password is required")
                || line.contains("a terminal is required")
                || line.contains("no tty present"))
        {
            needs_terminal.set(true);
        }
        let line = match credentials.filter(|credentials| line.contains(*credentials)) {
            Some(credentials) => Cow::Owned(line.replace(credentials, "[代理凭据]")),
            None => Cow::Borrowed(line),
        };
        const PROXY_AUTH: &[u8] = b"Proxy-Authorization:";
        if let Some(start) = line
            .as_bytes()
            .windows(PROXY_AUTH.len())
            .position(|text| text.eq_ignore_ascii_case(PROXY_AUTH))
        {
            log(&format!("{} [代理凭据]", &line[..start + PROXY_AUTH.len()]));
        } else {
            log(&line);
        }
    };
    let display = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|arg| display_argument(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    log_line(&format!("$ {display}"));

    // A shared pipe interleaves stdout and stderr without either blocking the other.
    let (reader, writer) = io::pipe().context("无法打开安装输出管道")?;
    let stdout = writer.try_clone().context("无法打开标准输出管道")?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(writer)
        .spawn()
        .with_context(|| format!("无法执行 {operation}"))?;
    drop(command);

    let read_result = read_lines(reader, &log_line);
    if read_result.is_err() {
        let _ = child.kill();
    }
    let result = child.wait();
    read_result.with_context(|| format!("无法读取 {operation} 的输出"))?;
    let status = result.with_context(|| format!("无法获取 {operation} 的结果"))?;
    if !status.success() {
        if needs_terminal.get() {
            return Err(NeedsTerminal {
                operation: operation.to_owned(),
                status,
            }
            .into());
        }
        bail!("{operation} 执行失败（{status}）");
    }
    Ok(())
}

fn read_lines(mut reader: impl Read, log: &impl Fn(&str)) -> io::Result<()> {
    const MAX_LINE_BYTES: usize = 64 * 1024;
    let mut chunk = [0; 8192];
    let mut line = Vec::new();
    let mut overflow = false;
    let mut previous_cr = false;
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        for &byte in &chunk[..count] {
            match byte {
                b'\r' | b'\n' => {
                    if overflow {
                        log("[该行输出超过 64 KiB]");
                    } else if !line.is_empty() || (byte == b'\n' && !previous_cr) {
                        log(&String::from_utf8_lossy(&line));
                    }
                    line.clear();
                    overflow = false;
                }
                _ if line.len() < MAX_LINE_BYTES => line.push(byte),
                _ => overflow = true,
            }
            previous_cr = byte == b'\r';
        }
    }
    if overflow {
        log("[该行输出超过 64 KiB]");
    } else if !line.is_empty() {
        log(&String::from_utf8_lossy(&line));
    }
    Ok(())
}

fn display_argument(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._/:@%=+,".contains(&byte))
    {
        value.to_owned()
    } else {
        shell_quote(value)
    }
}

fn apply_proxy<'a>(command: &mut Command, proxy: Option<&'a str>) -> Option<&'a str> {
    let proxy = proxy.map(str::trim).filter(|value| !value.is_empty());
    if let Some(proxy) = proxy {
        for key in PROXY_VARIABLES {
            command.env(key, proxy);
        }
    }
    proxy
}

fn proxy_needs_redaction(proxy: &str) -> bool {
    proxy_credentials(proxy).is_some()
}

fn proxy_credentials(proxy: &str) -> Option<&str> {
    let address = proxy
        .split_once("://")
        .map_or(proxy, |(_, address)| address);
    address
        .split('/')
        .next()
        .and_then(|authority| authority.rsplit_once('@'))
        .map(|(credentials, _)| credentials)
}
