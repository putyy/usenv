use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use url::Url;

use crate::runtimes::home_directory;

const BEGIN: &str = "# >>> usenv Homebrew >>>";
const END: &str = "# <<< usenv Homebrew <<<";
const API: &str = "HOMEBREW_API_DOMAIN";
const BOTTLE: &str = "HOMEBREW_BOTTLE_DOMAIN";
const NO_AUTO_UPDATE: &str = "HOMEBREW_NO_AUTO_UPDATE";
const SYSTEM_CONFIG: &str = "/etc/homebrew/brew.env";

#[derive(Clone, Default, PartialEq, Eq)]
pub struct BrewPreferences {
    pub api_mirror: String,
    pub bottle_mirror: String,
    pub disable_auto_update: bool,
}

#[derive(Clone)]
pub struct BrewConfig {
    pub path: PathBuf,
    pub preferences: Result<BrewPreferences, String>,
}

impl BrewConfig {
    pub fn discover(prefix: &Path) -> Result<Self> {
        let path = config_path(prefix)?;
        let preferences = BrewPreferences::read(&path).map_err(|error| format!("{error:#}"));
        Ok(Self { path, preferences })
    }
}

pub fn config_path(prefix: &Path) -> Result<PathBuf> {
    let home = home_directory().context("无法确定当前用户的主目录")?;
    let xdg = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty());
    let xdg = xdg.or_else(|| {
        let mut brew_xdg = env::var_os("HOMEBREW_XDG_CONFIG_HOME");
        for path in [PathBuf::from(SYSTEM_CONFIG), prefix.join("etc/homebrew/brew.env")] {
            if let Ok(source) = fs::read_to_string(path)
                && let Some(directory) = value(&source, "HOMEBREW_XDG_CONFIG_HOME")
            {
                brew_xdg = Some(directory.into());
            }
        }
        brew_xdg.filter(|value| !value.is_empty())
    });
    let directory = xdg.map_or_else(|| home.join(".homebrew"), |path| PathBuf::from(path).join("homebrew"));
    ensure!(directory.is_absolute(), "Homebrew 的用户配置目录必须是绝对路径");
    Ok(directory.join("brew.env"))
}

impl BrewPreferences {
    pub fn read(path: &Path) -> Result<Self> {
        let source = read_config(path)?;
        let Some(range) = managed_block(&source)? else {
            return Ok(Self::default());
        };
        let block = &source[range];
        Ok(Self {
            api_mirror: value(block, API).unwrap_or_default().into(),
            bottle_mirror: value(block, BOTTLE).unwrap_or_default().into(),
            disable_auto_update: value(block, NO_AUTO_UPDATE) == Some("1"),
        })
    }

    pub fn save(self, path: &Path) -> Result<Self> {
        let preferences = Self {
            api_mirror: mirror_url(&self.api_mirror, "软件索引镜像")?,
            bottle_mirror: mirror_url(&self.bottle_mirror, "预编译包镜像")?,
            disable_auto_update: self.disable_auto_update,
        };
        let target = match path.symlink_metadata() {
            Ok(metadata) if metadata.is_symlink() => path.canonicalize()
                .with_context(|| format!("无法找到 {} 指向的配置文件", path.display()))?,
            Ok(_) => path.to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_owned(),
            Err(error) => return Err(error).with_context(|| format!("无法读取 {}", path.display())),
        };
        let source = read_config(&target)?;
        let mut updated = source.clone();
        if let Some(range) = managed_block(&source)? {
            let extra: String = source[range.clone()].split_inclusive('\n').filter(|line| {
                !matches!(line.trim_end_matches(['\r', '\n']), BEGIN | END)
                    && !line.split_once('=').is_some_and(|(key, _)| {
                        matches!(key, API | BOTTLE | NO_AUTO_UPDATE)
                    })
            }).collect();
            updated.replace_range(range, &extra);
        }
        if preferences != Self::default() {
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(&format!("{BEGIN}\n"));
            for (key, value) in [
                (API, preferences.api_mirror.as_str()),
                (BOTTLE, preferences.bottle_mirror.as_str()),
            ] {
                if !value.is_empty() {
                    updated.push_str(&format!("{key}={value}\n"));
                }
            }
            if preferences.disable_auto_update {
                updated.push_str(&format!("{NO_AUTO_UPDATE}=1\n"));
            }
            updated.push_str(&format!("{END}\n"));
        }
        if updated != source {
            write_config(&target, &source, &updated)?;
        }
        Ok(preferences)
    }
}

fn mirror_url(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let url = Url::parse(value).with_context(|| format!("{label}需要填写完整的 URL"))?;
    ensure!(
        value.len() <= 2048 && !value.chars().any(|character| character.is_whitespace() || character.is_control())
            && matches!(url.scheme(), "http" | "https")
            && value.contains("://") && url.has_host()
            && url.username().is_empty() && url.password().is_none()
            && url.query().is_none() && url.fragment().is_none(),
        "{label}需要填写 HTTP 或 HTTPS 镜像地址"
    );
    Ok(value.to_owned())
}

fn managed_block(source: &str) -> Result<Option<std::ops::Range<usize>>> {
    let mut begin = None;
    let mut end = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        match line.trim_end_matches(['\r', '\n']) {
            BEGIN => {
                ensure!(begin.is_none(), "brew.env 有重复的 Usenv 配置标记，请先打开文件检查");
                begin = Some(offset);
            }
            END => {
                ensure!(begin.is_some() && end.is_none(), "brew.env 的 Usenv 配置标记不完整");
                end = Some(offset + line.len());
            }
            _ => (),
        }
        offset += line.len();
    }
    ensure!(begin.is_some() == end.is_some(), "brew.env 的 Usenv 配置标记不完整");
    Ok(begin.zip(end).map(|(begin, end)| begin..end))
}

fn value<'a>(source: &'a str, key: &str) -> Option<&'a str> {
    source.lines().filter_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == key).then_some(value)
    }).last()
}

fn read_config(path: &Path) -> Result<String> {
    match fs::metadata(path) {
        Ok(metadata) => ensure!(metadata.is_file(), "{} 不是配置文件", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error).with_context(|| format!("无法读取 {}", path.display())),
    }
    fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))
}

fn write_config(path: &Path, previous: &str, updated: &str) -> Result<()> {
    let directory = path.parent().context("无效的 brew.env 路径")?;
    fs::create_dir_all(directory)?;
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    temp.write_all(updated.as_bytes())?;
    let existing = fs::metadata(path).ok();
    if let Some(metadata) = &existing {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.as_file().sync_all()?;
    ensure!(read_config(path)? == previous, "brew.env 已在别处修改，请重新读取后保存");
    if existing.is_some() {
        temp.persist(path)?;
    } else {
        temp.persist_noclobber(path)?;
    }
    Ok(())
}
