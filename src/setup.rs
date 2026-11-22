use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::command::{self, PROXY_VARIABLES, shell_quote};
use crate::runtimes::home_directory;

pub const HOMEBREW_DOCS: &str = "https://docs.brew.sh/Installation";
pub const MISE_DOCS: &str = "https://mise.jdx.dev/installing-mise.html";
pub const HOMEBREW_MANUAL: &str = concat!(
    "/bin/bash -c \"$(/usr/bin/curl -fsSL ",
    "https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"",
);
pub const MISE_MANUAL: &str = "/usr/bin/curl -fsSL https://mise.run | /bin/sh";

pub fn install_homebrew_in_terminal(proxy: Option<&str>, log: &dyn Fn(&str)) -> Result<()> {
    run_terminal_launcher("Install Homebrew.command", &homebrew_launcher(proxy), log)
}

pub(crate) fn install_brew_package_in_terminal(
    path: &Path,
    args: &[&str],
    proxy: Option<&str>,
    log: &dyn Fn(&str),
) -> Result<()> {
    let path = path.to_str().context("无法读取 Homebrew 命令路径")?;
    let mut script = terminal_script_header(proxy);
    for variable in ["XDG_CONFIG_HOME", "HOMEBREW_XDG_CONFIG_HOME"] {
        if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
            let value = value.to_str().context("Homebrew 配置目录路径必须是 UTF-8 文本")?;
            script.push_str(&format!("export {variable}={}\n", shell_quote(value)));
        } else {
            script.push_str(&format!("unset {variable}\n"));
        }
    }
    script.push_str(
        "export HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_ANALYTICS=1\n",
    );
    match args.first().copied() {
        Some("upgrade") => script.push_str("export HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1\n"),
        Some("uninstall") => script.push_str("export HOMEBREW_NO_AUTOREMOVE=1\n"),
        _ => (),
    }
    script.push_str(&shell_quote(path));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push_str(
        r#"
result=$?
if [ "$result" -eq 0 ]; then
    printf '\n%s\n' '操作完成。可以回到 Usenv 查看结果。'
else
    printf '\n%s\n' '操作未完成，请查看上方输出。'
fi
exit "$result"
"#,
    );
    run_terminal_launcher("Continue Installation.command", &script, log)
}

fn run_terminal_launcher(file_name: &str, script: &str, log: &dyn Fn(&str)) -> Result<()> {
    let work = tempfile::Builder::new()
        .prefix("usenv-install-")
        .tempdir()
        .context("无法创建安装临时目录")?;
    let launcher = work.path().join(file_name);
    let pending = work.path().join("pending");
    fs::write(&pending, "")?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&launcher)
        .and_then(|mut file| file.write_all(script.as_bytes()))
        .context("无法准备终端安装命令")?;

    let mut open = Command::new("/usr/bin/open");
    open.args(["-a", "/System/Applications/Utilities/Terminal.app"])
        .arg(&launcher);
    command::output(open, "在终端打开安装", None)?;
    log("已打开 macOS 终端，请在那里查看输出并按提示确认。");

    let started = Instant::now();
    let result_path = work.path().join("result");
    let mut terminal_pid = None;
    let mut next_pid_check = started;
    let mut terminal_exited = false;
    loop {
        match fs::read_to_string(&result_path) {
            Ok(result) => {
                let status: u8 = result.trim().parse().context("终端未返回有效的安装结果")?;
                ensure!(status == 0, "终端中的安装未完成（退出状态 {status}）");
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("无法读取终端安装结果"),
        }
        if terminal_exited {
            bail!("终端安装已结束，请查看终端输出");
        }
        let now = Instant::now();
        if terminal_pid.is_none() {
            terminal_pid = fs::read_dir(work.path())?.filter_map(|entry| {
                entry.ok()?.file_name().to_str()?.strip_prefix("started-")?.parse::<u32>().ok()
            }).find(|pid| *pid > 0);
            if terminal_pid.is_none() && now.duration_since(started) >= Duration::from_secs(45) {
                // Removing the pending file and claiming it are mutually exclusive.
                match fs::remove_file(&pending) {
                    Ok(()) => bail!("终端没有开始安装，请重试"),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error).context("无法读取终端安装状态"),
                }
            }
        }
        if now >= next_pid_check && let Some(pid) = terminal_pid {
            next_pid_check = now + Duration::from_secs(1);
            terminal_exited = Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .output()
                .is_ok_and(|output| !output.status.success());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn terminal_script_header(proxy: Option<&str>) -> String {
    let mut script = String::from(
        "#!/bin/bash\n\
         set +x\n\
         work=${0%/*}\n\
         /bin/mv -f \"$work/pending\" \"$work/started-$$\" || exit 1\n\
         trap 'result=$?; printf \"%s\\n\" \"$result\" > \"$work/result.tmp\" && /bin/mv -f \"$work/result.tmp\" \"$work/result\"; /bin/rm -f \"$0\" \"$work/install.sh\"' EXIT\n\
         trap 'exit 1' HUP INT TERM\n\
         /bin/rm -f \"$0\"\n\
         unset NONINTERACTIVE CI\n",
    );
    if let Some(proxy) = proxy {
        let quoted = shell_quote(proxy);
        for key in PROXY_VARIABLES {
            script.push_str(&format!("export {key}={quoted}\n"));
        }
    }
    script
}

fn homebrew_launcher(proxy: Option<&str>) -> String {
    let mut script = terminal_script_header(proxy);
    script.push_str(
        r#"printf '%s\n' '正在下载 Homebrew 官方安装程序…'
/usr/bin/curl -q --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 20 --max-time 120 \
    -o "$work/install.sh" https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh || exit $?
lock_dir=$(/usr/bin/getconf DARWIN_USER_TEMP_DIR) || exit $?
/usr/bin/lockf -t 0 "${lock_dir%/}/usenv-homebrew-install.lock" /bin/bash "$work/install.sh"
result=$?
if [ "$result" -eq 0 ]; then
    printf '\n%s\n' 'Homebrew 安装完成。可以回到 Usenv 查看结果。'
else
    printf '\n%s\n' 'Homebrew 安装未完成，请查看上方输出。'
fi
exit "$result"
"#,
    );
    script
}

pub fn install_mise(proxy: Option<&str>, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let home = home_directory().context("无法确定当前用户的主目录")?;
    let work = tempfile::Builder::new()
        .prefix("usenv-mise-")
        .tempdir()
        .context("无法创建 Mise 安装临时目录")?;
    let installer = work.path().join("install.sh");
    let mut download = Command::new("/usr/bin/curl");
    download
        .args([
            "-q",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--fail",
            "--show-error",
            "--location",
            "--progress-bar",
            "--connect-timeout",
            "20",
            "--max-time",
            "120",
            "-o",
        ])
        .arg(&installer)
        .arg("https://mise.run");
    command::stream(download, "下载 Mise 官方安装程序", proxy, log)?;

    let mut install = Command::new("/bin/sh");
    install
        .arg(installer)
        .current_dir(&home)
        .env("MISE_INSTALL_PATH", home.join(".local/bin/mise"));
    command::stream(install, "安装 Mise", proxy, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_launcher_quotes_proxy_without_running_shell_syntax() -> Result<()> {
        let proxy = "http://user:p'$(exit 99)`exit 98`@127.0.0.1:7890";
        let launcher = homebrew_launcher(Some(proxy));
        let exports = launcher
            .lines()
            .filter(|line| line.starts_with("export "))
            .collect::<Vec<_>>()
            .join("\n");
        let mut shell = Command::new("/bin/bash");
        shell.args([
            "-c",
            &format!("{exports}\nprintf '%s\\n' \"$https_proxy\" \"$ALL_PROXY\""),
        ]);
        assert_eq!(
            command::output(shell, "proxy quoting", None)?,
            format!("{proxy}\n{proxy}\n")
        );
        Ok(())
    }
}
