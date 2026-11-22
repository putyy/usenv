use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

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
    Available { version: String, page: String, dmg: Option<String> },
    Failed(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    state: String,
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
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    };
    let dmg = [arch, "universal"].into_iter().find_map(|arch| {
        [format!("Usenv-macos-{arch}.dmg"), format!("Usenv-dev-macos-{arch}.dmg")]
            .into_iter()
            .find_map(|name| {
                let url = format!("{RELEASES}/download/{tag}/{name}");
                release.assets.iter().any(|asset| {
                    asset.state == "uploaded"
                        && asset.name == name
                        && asset.browser_download_url == url
                }).then_some(url)
            })
    });
    Ok(UpdateStatus::Available {
        version: tag.trim_start_matches('v').to_owned(),
        page: format!("{RELEASES}/tag/{tag}"),
        dmg,
    })
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
