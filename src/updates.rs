use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::command;

pub const REPOSITORY: &str = "https://github.com/putyy/usenv";
pub const RELEASES: &str = "https://github.com/putyy/usenv/releases";
const API: &str = "https://api.github.com/repos/putyy/usenv/releases?per_page=30";

#[derive(Clone, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    NoRelease,
    Current,
    Available(AvailableUpdate),
    InProgress(AvailableUpdate, UpdatePhase),
    InstallFailed(AvailableUpdate, String),
    Failed(String),
}

#[derive(Clone)]
pub struct AvailableUpdate {
    pub version: String,
    pub notes: String,
    pub page: String,
    pub dmg: Option<Download>,
}

#[derive(Clone)]
pub struct Download {
    url: String,
    digest: String,
    pub size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    Downloading { received: u64, total: u64 },
    Verifying,
    Waiting,
    Installing,
}

impl UpdatePhase {
    pub fn label(self) -> String {
        match self {
            Self::Downloading { received, total } => {
                let percent = u128::from(received.min(total)) * 100 / u128::from(total);
                format!("正在下载更新 · {percent}%")
            }
            Self::Verifying => "正在校验更新…".into(),
            Self::Waiting => "更新已就绪，等待当前任务完成…".into(),
            Self::Installing => "正在安装更新并重新启动 Usenv…".into(),
        }
    }
}

impl UpdateStatus {
    pub fn offer(&self) -> Option<&AvailableUpdate> {
        match self {
            Self::Available(release) | Self::InstallFailed(release, _) => Some(release),
            _ => None,
        }
    }

    pub fn release(&self) -> Option<&AvailableUpdate> {
        match self {
            Self::InProgress(release, _) => Some(release),
            _ => self.offer(),
        }
    }

    pub fn phase(&self) -> Option<UpdatePhase> {
        match self {
            Self::InProgress(_, phase) => Some(*phase),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    state: String,
    size: u64,
    digest: Option<String>,
}

pub fn check(proxy: Option<&str>) -> UpdateStatus {
    check_release(proxy).unwrap_or_else(|error| UpdateStatus::Failed(format!("{error:#}")))
}

fn check_release(proxy: Option<&str>) -> Result<UpdateStatus> {
    let mut curl = Command::new("/usr/bin/curl");
    curl.args([
        "--disable", "--fail", "--silent", "--show-error", "--location",
        "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "5",
        "--max-time", "15", "--max-filesize", "2097152", "--header",
        "Accept: application/vnd.github+json", "--user-agent", "Usenv", API,
    ]);
    let output = command::output(curl, "检查 GitHub Releases", proxy)?;
    ensure!(output.len() <= 2 * 1024 * 1024, "GitHub 返回的发布清单过大");
    let releases: Vec<Release> = serde_json::from_str(&output).context("无法读取 GitHub 发布清单")?;
    let current = stable_version(env!("CARGO_PKG_VERSION")).context("无法识别当前应用版本")?;
    let Some((version, release)) = releases.into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| Some((stable_version(&release.tag_name)?, release)))
        .max_by_key(|(version, _)| *version)
    else {
        return Ok(UpdateStatus::NoRelease);
    };
    if version <= current {
        return Ok(UpdateStatus::Current);
    }

    let tag = release.tag_name;
    let dmg = [macos_arch(), "universal"].into_iter().find_map(|arch| {
        [format!("Usenv-macos-{arch}.dmg"), format!("Usenv-dev-macos-{arch}.dmg")]
            .into_iter()
            .find_map(|name| {
                let url = format!("{RELEASES}/download/{tag}/{name}");
                let asset = release.assets.iter().find(|asset| {
                    asset.state == "uploaded" && asset.name == name && asset.browser_download_url == url
                })?;
                let digest = asset.digest.as_deref()?.strip_prefix("sha256:")?;
                (asset.size > 0 && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| Download { url, digest: digest.to_ascii_lowercase(), size: asset.size })
            })
    });
    Ok(UpdateStatus::Available(AvailableUpdate {
        version: tag.trim_start_matches('v').to_owned(),
        notes: release.body.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "此版本没有提供更新说明。".into()),
        page: format!("{RELEASES}/tag/{tag}"),
        dmg,
    }))
}

fn macos_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    }
}

fn installed_bundle() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let bundle = exe.ancestors().nth(3).context("无法定位 Usenv.app")?;
    ensure!(
        bundle.extension().is_some_and(|ext| ext == "app") && exe == bundle.join("Contents/MacOS/usenv"),
        "请从已安装的 Usenv.app 启动后再更新"
    );
    Ok(bundle.to_owned())
}

fn check_bundle(bundle: &Path, version: &str) -> Result<()> {
    let plist = bundle.join("Contents/Info.plist");
    for (key, expected) in [
        ("CFBundleIdentifier", "io.github.putyy.usenv"),
        ("CFBundleExecutable", "usenv"),
        ("CFBundleShortVersionString", version),
    ] {
        let mut plutil = Command::new("/usr/bin/plutil");
        plutil.args(["-extract", key, "raw", "-o", "-", "--"]).arg(&plist);
        let value = command::output(plutil, "读取应用信息", None)?;
        ensure!(value.trim() == expected, "应用信息不匹配（{key}），已中止更新");
    }
    Ok(())
}

struct MountedImage(PathBuf);

impl Drop for MountedImage {
    fn drop(&mut self) {
        let detach = |force: bool| {
            let mut command = Command::new("/usr/bin/hdiutil");
            command.arg("detach");
            if force {
                command.arg("-force");
            }
            command.arg(&self.0).output().is_ok_and(|out| out.status.success())
        };
        if !detach(false) {
            detach(true);
        }
    }
}

pub struct PreparedUpdate {
    staging: TempDir,
    bundle: PathBuf,
    // Serializes updates by other running instances of this installation.
    _lock: File,
}

pub fn prepare(
    release: &AvailableUpdate,
    proxy: Option<&str>,
    report: impl Fn(UpdatePhase),
) -> Result<PreparedUpdate> {
    let asset = release.dmg.as_ref().context("没有匹配当前架构且可校验的安装包")?;
    let bundle = installed_bundle()?;
    let lock = File::open(bundle.join("Contents/MacOS/usenv"))?;
    lock.try_lock().context("另一份 Usenv 正在更新此应用")?;
    check_bundle(&bundle, env!("CARGO_PKG_VERSION"))?;
    let parent = bundle.parent().context("无法定位应用的安装目录")?;
    let mut temporary = tempfile::Builder::new();
    temporary.prefix(".usenv-update-").permissions(fs::Permissions::from_mode(0o700));
    let staging = temporary.tempdir_in(parent)
        .context("安装目录不可写。请将 Usenv.app 移至可写的应用程序目录后重新打开")?;
    let scratch = temporary.tempdir()?;
    let dmg = scratch.path().join("Usenv.dmg");
    let mut curl = Command::new("/usr/bin/curl");
    curl.args(["--disable", "--fail", "--silent", "--show-error", "--location",
        "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "15",
        "--max-time", "900", "--max-filesize"])
        .arg(asset.size.to_string()).arg("--output").arg(&dmg).arg(&asset.url);
    std::thread::scope(|scope| -> Result<()> {
        let download = scope.spawn(move || command::output(curl, "下载 Usenv 更新", proxy));
        while !download.is_finished() {
            if let Ok(file) = fs::metadata(&dmg) {
                report(UpdatePhase::Downloading { received: file.len().min(asset.size), total: asset.size });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        download.join().map_err(|_| anyhow::anyhow!("下载线程意外退出"))??;
        Ok(())
    })?;
    report(UpdatePhase::Verifying);
    let mut file = File::open(&dmg)?;
    let mut hash = Sha256::new();
    ensure!(io::copy(&mut file, &mut hash)? == asset.size
        && format!("{:x}", hash.finalize()) == asset.digest, "安装包与发布的 SHA-256 或大小不符，请重新下载");

    let mount = scratch.path().join("volume");
    fs::create_dir(&mount)?;
    let mut attach = Command::new("/usr/bin/hdiutil");
    attach.args(["attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint"])
        .arg(&mount).arg(&dmg);
    command::output(attach, "挂载更新安装包", None)?;
    let mounted = MountedImage(mount);
    let staged = staging.path().join("Usenv.app");
    let mut copy = Command::new("/usr/bin/ditto");
    copy.arg(mounted.0.join("Usenv.app")).arg(&staged);
    command::output(copy, "复制新版 Usenv", None)?;
    check_bundle(&staged, &release.version)?;
    let mut verify = Command::new("/usr/bin/codesign");
    verify.args(["--verify", "--deep", "--strict", "--architecture", macos_arch()]).arg(&staged);
    command::output(verify, "校验新版应用签名", None)?;
    Ok(PreparedUpdate { staging, bundle, _lock: lock })
}

impl PreparedUpdate {
    pub fn install(self) -> Result<()> {
        check_bundle(&self.bundle, env!("CARGO_PKG_VERSION"))?;
        let staged = self.staging.path().join("Usenv.app");
        swap_bundles(&self.bundle, &staged).context("无法替换当前应用，请检查安装目录的写入权限")?;
        let mut open = Command::new("/usr/bin/open");
        open.arg("-n").arg(&self.bundle);
        if let Err(error) = command::output(open, "启动新版 Usenv", None) {
            if let Err(rollback) = swap_bundles(&self.bundle, &staged) {
                let backup = self.staging.keep().join("Usenv.app");
                anyhow::bail!("{error:#}；恢复旧版失败（{rollback}），旧版保存在 {}", backup.display());
            }
            return Err(error.context("已恢复原来的 Usenv"));
        }
        Ok(())
    }
}

fn swap_bundles(installed: &Path, staged: &Path) -> io::Result<()> {
    let installed = CString::new(installed.as_os_str().as_bytes())?;
    let staged = CString::new(staged.as_os_str().as_bytes())?;
    // Both paths are on the same volume. The original bundle remains intact on failure.
    if unsafe { libc::renamex_np(installed.as_ptr(), staged.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn stable_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.');
    let mut next = || {
        let part = parts.next()?;
        (!part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && (part.len() == 1 || !part.starts_with('0')))
            .then(|| part.parse::<u64>().ok())
            .flatten()
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}
