use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::brew::Environment;
use crate::command::shell_quote;

const DATA_DIRECTORY: &str = "Library/Application Support/usenv";
const DEFAULTS_FILE: &str = "defaults.zsh";
const DEFAULTS_HEADER: &str = "# usenv defaults v1: ";
pub const MIN_PARALLEL_INSTALLS: u8 = 1;
pub const MAX_PARALLEL_INSTALLS: u8 = 4;
const DEFAULT_PARALLEL_INSTALLS: u8 = 2;
const MAX_RECENT_PROJECTS: usize = 5;

const ZSH_HOOK: &str = concat!(
    "\n# >>> usenv default versions >>>\n",
    "if [[ -r \"$HOME/Library/Application Support/usenv/defaults.zsh\" ]]; then\n",
    "  builtin source \"$HOME/Library/Application Support/usenv/defaults.zsh\"\n",
    "fi\n",
    "# <<< usenv default versions <<<\n",
);

#[derive(Clone)]
pub struct Settings {
    pub proxy: String,
    pub quiet_login: bool,
    pub recent_projects: Vec<PathBuf>,
    pub parallel_installs: u8,
    pub theme: Theme,
    pub defaults: BTreeMap<String, String>,
    pub managed_services: BTreeSet<String>,
    hidden_environments: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        settings(Preferences::default(), BTreeMap::new())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Settings {
    pub fn environment_visible(&self, environment: Environment) -> bool {
        !self.hidden_environments.contains(environment.key())
    }
}

#[derive(Clone, Copy)]
pub enum ShellFile {
    Env,
    Profile,
    Rc,
}

impl ShellFile {
    pub const ALL: [Self; 3] = [Self::Env, Self::Profile, Self::Rc];

    pub fn name(self) -> &'static str {
        match self {
            Self::Env => ".zshenv",
            Self::Profile => ".zprofile",
            Self::Rc => ".zshrc",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SettingsStore {
    data_dir: PathBuf,
    zshrc: PathBuf,
}

#[derive(Default, Deserialize, Serialize)]
struct Preferences {
    #[serde(default)]
    quiet_login: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_projects: Vec<PathBuf>,
    #[serde(default)]
    proxy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parallel_installs: Option<u8>,
    #[serde(default)]
    theme: Theme,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    hidden_environments: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    managed_services: BTreeSet<String>,
}

#[derive(Deserialize, Serialize)]
struct RuntimeDefault {
    formula: String,
    bin: String,
}

type RuntimeDefaults = BTreeMap<String, RuntimeDefault>;

impl SettingsStore {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME").context("无法确定当前用户的主目录")?;
        let home = Path::new(&home);
        ensure!(home.is_absolute(), "当前用户的主目录必须是绝对路径");

        let dotdir = env::var_os("ZDOTDIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.to_owned());
        ensure!(dotdir.is_absolute(), "ZDOTDIR 必须是绝对路径");
        Ok(Self {
            data_dir: home.join(DATA_DIRECTORY),
            zshrc: dotdir.join(".zshrc"),
        })
    }

    pub fn load(&self) -> Result<Settings> {
        Ok(settings(self.read_preferences()?, self.read_defaults()?))
    }

    pub fn service_directory(&self) -> PathBuf {
        self.data_dir.join("services")
    }

    pub fn shell_file(&self, file: ShellFile) -> PathBuf {
        self.zshrc.with_file_name(file.name())
    }

    pub fn prepare_shell_file(&self, file: ShellFile) -> Result<PathBuf> {
        let path = self.shell_file(file);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => {
                return Err(error).with_context(|| format!("无法创建 {}", path.display()));
            }
        }
        Ok(path)
    }

    pub fn set_service_managed(&self, formula: &str, enabled: bool) -> Result<Settings> {
        self.update_preferences(|preferences| {
            if enabled {
                preferences.managed_services.insert(formula.into());
            } else {
                preferences.managed_services.remove(formula);
            }
        })
    }

    pub fn set_proxy(&self, proxy: &str) -> Result<Settings> {
        let proxy = proxy.trim();
        ensure!(
            !proxy.chars().any(char::is_control),
            "代理地址不能包含控制字符"
        );

        self.update_preferences(|preferences| preferences.proxy = proxy.to_owned())
    }

    pub fn set_theme(&self, theme: Theme) -> Result<Settings> {
        self.update_preferences(|preferences| preferences.theme = theme)
    }

    pub fn set_quiet_login(&self, enabled: bool) -> Result<Settings> {
        self.update_preferences(|preferences| preferences.quiet_login = enabled)
    }

    pub fn remember_project(&self, directory: PathBuf) -> Result<Settings> {
        ensure!(directory.is_absolute(), "项目目录必须是绝对路径");
        let exists = match directory.metadata() {
            Ok(metadata) => metadata.is_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error).context("无法读取项目目录"),
        };
        self.update_preferences(|preferences| {
            let projects = &mut preferences.recent_projects;
            projects.retain(|previous| previous != &directory);
            if exists {
                projects.insert(0, directory);
            }
            projects.truncate(MAX_RECENT_PROJECTS);
        })
    }

    pub fn set_parallel_installs(&self, limit: u8) -> Result<Settings> {
        ensure!(
            (MIN_PARALLEL_INSTALLS..=MAX_PARALLEL_INSTALLS).contains(&limit),
            "同时安装数量必须是 1 到 4"
        );
        self.update_preferences(|preferences| preferences.parallel_installs = Some(limit))
    }

    pub fn set_environment_visible(
        &self,
        environment: Environment,
        visible: bool,
    ) -> Result<Settings> {
        self.update_preferences(|preferences| {
            if visible {
                preferences.hidden_environments.remove(environment.key());
            } else {
                preferences
                    .hidden_environments
                    .insert(environment.key().into());
            }
        })
    }

    fn update_preferences(&self, update: impl FnOnce(&mut Preferences)) -> Result<Settings> {
        let _lock = self.lock()?;
        let defaults = self.read_defaults()?;
        let mut preferences = self.read_preferences()?;
        update(&mut preferences);
        self.write_preferences(&preferences)?;
        Ok(settings(preferences, defaults))
    }

    pub fn set_default(&self, family: &str, formula: &str, opt_bin: &Path) -> Result<Settings> {
        let path = opt_bin.to_str().context("Homebrew 路径必须是 UTF-8 文本")?;
        let environment = runtime(family)?;
        ensure!(
            Environment::from_formula(formula) == Some(environment),
            "{formula} 不是 {family} 的 Homebrew 公式"
        );
        ensure!(
            opt_bin.is_absolute()
                && opt_bin.ends_with(
                    Path::new("opt")
                        .join(formula)
                        .join(environment.brew_bin_dir())
                ),
            "默认版本需要使用 Homebrew 公式的 opt 工具路径"
        );
        ensure!(
            !path.contains(':') && !path.chars().any(char::is_control),
            "Homebrew 路径包含 PATH 不支持的字符"
        );
        for command in environment.runtime_commands() {
            let executable = opt_bin.join(command);
            let metadata = executable
                .metadata()
                .with_context(|| format!("找不到 {}", executable.display()))?;
            ensure!(
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
                "{} 不可执行",
                executable.display()
            );
        }

        let _lock = self.lock()?;
        let preferences = self.read_preferences()?;
        let mut defaults = self.read_defaults()?;
        defaults.insert(
            family.to_owned(),
            RuntimeDefault {
                formula: formula.to_owned(),
                bin: path.to_owned(),
            },
        );
        self.ensure_zsh_hook()?;
        self.write_defaults(&defaults)?;
        Ok(settings(preferences, defaults))
    }

    pub fn clear_default(&self, family: &str) -> Result<Settings> {
        runtime(family)?;
        let _lock = self.lock()?;
        let preferences = self.read_preferences()?;
        let mut defaults = self.read_defaults()?;
        if defaults.remove(family).is_some() {
            self.write_defaults(&defaults)?;
        }
        Ok(settings(preferences, defaults))
    }

    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("无法创建配置目录 {}", self.data_dir.display()))?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.data_dir.join(".settings.lock"))
            .context("无法打开 usenv 配置锁")?;
        file.lock().context("无法锁定 usenv 配置")?;
        Ok(file)
    }

    fn read_preferences(&self) -> Result<Preferences> {
        match read_if_present(&self.data_dir.join("settings.json"))? {
            Some(bytes) => serde_json::from_slice(&bytes).context("usenv 设置格式无效"),
            None => Ok(Preferences::default()),
        }
    }

    fn write_preferences(&self, preferences: &Preferences) -> Result<()> {
        atomic_write(
            &self.data_dir.join("settings.json"),
            &serde_json::to_vec_pretty(preferences)?,
        )
    }

    fn read_defaults(&self) -> Result<RuntimeDefaults> {
        let Some(bytes) = read_if_present(&self.data_dir.join(DEFAULTS_FILE))? else {
            return Ok(RuntimeDefaults::new());
        };
        let first_line = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        let json = first_line
            .strip_prefix(DEFAULTS_HEADER.as_bytes())
            .context("usenv 默认版本文件缺少配置头")?;
        serde_json::from_slice(json).context("usenv 默认版本配置格式无效")
    }

    fn write_defaults(&self, defaults: &RuntimeDefaults) -> Result<()> {
        // Store the selections and the shell commands together so they change in one rename.
        let mut script = format!("{DEFAULTS_HEADER}{}\n", serde_json::to_string(defaults)?);
        script.push_str("() {\n  emulate -L zsh\n  local -aU usenv_path\n");
        for environment in Environment::ALL
            .into_iter()
            .filter(Environment::supports_default)
        {
            let Some(selection) = defaults.get(environment.key()) else {
                continue;
            };
            let checks = environment
                .runtime_commands()
                .iter()
                .map(|name| format!("-x {}", shell_quote(&format!("{}/{name}", selection.bin))))
                .collect::<Vec<_>>()
                .join(" && ");
            script.push_str(&format!(
                "  if [[ {checks} ]]; then\n    usenv_path+=({})\n  fi\n",
                shell_quote(&selection.bin)
            ));
        }
        script.push_str("  if (( ${#usenv_path} )); then\n    usenv_path+=(\"${path[@]}\")\n    path=(\"${usenv_path[@]}\")\n  fi\n}\n");
        atomic_write(&self.data_dir.join(DEFAULTS_FILE), script.as_bytes())
    }

    fn ensure_zsh_hook(&self) -> Result<()> {
        let current = read_if_present(&self.zshrc)?.unwrap_or_default();
        if current
            .windows(ZSH_HOOK.len() - 1)
            .any(|part| part == &ZSH_HOOK.as_bytes()[1..])
        {
            return Ok(());
        }
        // Append to preserve existing contents, permissions, and dotfile symlinks.
        let mut rc = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.zshrc)
            .with_context(|| format!("无法打开 {}", self.zshrc.display()))?;
        rc.write_all(ZSH_HOOK.as_bytes())
            .with_context(|| format!("无法更新 {}", self.zshrc.display()))
    }
}

fn settings(preferences: Preferences, defaults: RuntimeDefaults) -> Settings {
    Settings {
        quiet_login: preferences.quiet_login,
        recent_projects: preferences.recent_projects,
        proxy: preferences.proxy,
        parallel_installs: preferences
            .parallel_installs
            .unwrap_or(DEFAULT_PARALLEL_INSTALLS)
            .clamp(MIN_PARALLEL_INSTALLS, MAX_PARALLEL_INSTALLS),
        theme: preferences.theme,
        hidden_environments: preferences.hidden_environments,
        managed_services: preferences.managed_services,
        defaults: defaults
            .into_iter()
            .map(|(family, selection)| (family, selection.formula))
            .collect(),
    }
}

fn runtime(family: &str) -> Result<Environment> {
    Environment::ALL
        .into_iter()
        .find(|environment| environment.supports_default() && environment.key() == family)
        .with_context(|| format!("{family} 不支持设置终端默认版本"))
}

fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>> {
    match path.metadata() {
        Ok(metadata) => ensure!(metadata.is_file(), "{} 不是普通文件", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("无法读取 {}", path.display())),
    }
    fs::read(path)
        .map(Some)
        .with_context(|| format!("无法读取 {}", path.display()))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("无法为 {} 创建临时文件", path.display()))?;
    let result = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("无法保存 {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn shell_file_entry_uses_zdotdir_and_preserves_existing_config() -> Result<()> {
        let home = TestHome::new()?;
        let dotdir = home.0.join("dot files");
        fs::create_dir(&dotdir)?;
        let store = SettingsStore {
            data_dir: home.0.join(DATA_DIRECTORY),
            zshrc: dotdir.join(".zshrc"),
        };
        fs::write(&store.zshrc, "export TEST=present\n")?;
        assert_eq!(store.prepare_shell_file(ShellFile::Rc)?, store.zshrc);
        assert_eq!(fs::read_to_string(&store.zshrc)?, "export TEST=present\n");
        let environment = store.prepare_shell_file(ShellFile::Env)?;
        assert_eq!(environment, dotdir.join(".zshenv"));
        assert_eq!(
            fs::metadata(&environment)?.permissions().mode() & 0o777,
            0o600
        );
        assert!(!store.shell_file(ShellFile::Profile).exists());
        Ok(())
    }

    #[test]
    fn zsh_selects_node_npm_and_npx_from_one_formula() -> Result<()> {
        let fixture = TestHome::new()?;
        let home = &fixture.0;

        let dotdir = home.join("dot files");
        fs::create_dir(&dotdir)?;
        let store = SettingsStore {
            data_dir: home.join(DATA_DIRECTORY),
            zshrc: dotdir.join(".zshrc"),
        };
        let previous = home.join("original bin");
        let selected = home.join("brew's prefix/opt/node@24/bin");
        let other = home.join("brew's prefix/opt/node@22/bin");
        create_commands(&previous)?;
        create_commands(&selected)?;
        create_commands(&other)?;
        let dotfile = home.join("dotfiles.zsh");
        let user_content = format!(
            "export PATH={}:$PATH\n",
            shell_quote(previous.to_str().unwrap())
        );
        fs::write(&dotfile, &user_content)?;
        symlink(&dotfile, &store.zshrc)?;

        store.set_default("node", "node@22", &other)?;
        assert_commands(home, &dotdir, &other)?;
        store.set_default("node", "node@24", &selected)?;
        assert_eq!(
            fs::read_to_string(&dotfile)?,
            format!("{user_content}{ZSH_HOOK}")
        );
        assert!(fs::symlink_metadata(&store.zshrc)?.is_symlink());
        assert_eq!(store.load()?.defaults["node"], "node@24");
        assert_commands(home, &dotdir, &selected)?;

        fs::remove_file(selected.join("npx"))?;
        assert_commands(home, &dotdir, &previous)?;
        create_commands(&selected)?;
        assert!(store.clear_default("node")?.defaults.is_empty());
        assert_commands(home, &dotdir, &previous)?;
        Ok(())
    }

    #[test]
    fn preferences_preserve_existing_settings_and_defaults() -> Result<()> {
        let fixture = TestHome::new()?;
        let store = SettingsStore {
            data_dir: fixture.0.join(DATA_DIRECTORY),
            zshrc: fixture.0.join(".zshrc"),
        };
        fs::create_dir_all(&store.data_dir)?;
        fs::write(
            store.data_dir.join("settings.json"),
            r#"{"proxy":"http://127.0.0.1:7890"}"#,
        )?;
        let legacy = store.load()?;
        assert_eq!(legacy.theme, Theme::System);
        assert!(!legacy.quiet_login);
        assert!(legacy.recent_projects.is_empty());
        assert!(
            Environment::ALL
                .into_iter()
                .all(|env| legacy.environment_visible(env))
        );

        let node = fixture.0.join("brew/opt/node@24/bin");
        create_commands(&node)?;
        let hidden = store.set_environment_visible(Environment::Node, false)?;
        assert!(!hidden.environment_visible(Environment::Node));
        assert_eq!(hidden.proxy, legacy.proxy);
        assert!(!store.zshrc.exists());

        store.set_default("node", "node@24", &node)?;
        store.set_theme(Theme::Dark)?;
        store.set_quiet_login(true)?;
        store.set_proxy("socks5://127.0.0.1:1080")?;
        let reloaded = store.load()?;
        assert_eq!(reloaded.theme, Theme::Dark);
        assert!(reloaded.quiet_login);
        assert!(!reloaded.environment_visible(Environment::Node));
        assert!(reloaded.environment_visible(Environment::Php));
        assert_eq!(reloaded.defaults["node"], "node@24");
        assert_eq!(reloaded.proxy, "socks5://127.0.0.1:1080");

        store.set_environment_visible(Environment::Node, true)?;
        assert!(store.load()?.environment_visible(Environment::Node));
        Ok(())
    }

    #[test]
    fn recent_projects_keep_the_last_five_distinct_directories() -> Result<()> {
        let fixture = TestHome::new()?;
        let store = SettingsStore {
            data_dir: fixture.0.join(DATA_DIRECTORY),
            zshrc: fixture.0.join(".zshrc"),
        };
        let projects: Vec<_> = (0..6).map(|i| fixture.0.join(format!("project {i}"))).collect();
        for project in &projects {
            fs::create_dir(project)?;
            store.remember_project(project.clone())?;
        }
        store.remember_project(projects[2].clone())?;
        let expected = [2, 5, 4, 3, 1].map(|i| projects[i].clone());
        assert_eq!(store.load()?.recent_projects, expected);
        fs::remove_dir(&projects[2])?;
        let settings = store.remember_project(projects[2].clone())?;
        assert_eq!(settings.recent_projects, expected[1..]);
        Ok(())
    }

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Result<Self> {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let home =
                env::temp_dir().join(format!("usenv-{}-{unique}-{sequence}", std::process::id()));
            fs::create_dir(&home)?;
            Ok(Self(home))
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_commands(path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        for command in Environment::Node.runtime_commands() {
            let path = path.join(command);
            fs::write(&path, b"#!/bin/sh\n")?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    fn assert_commands(home: &Path, dotdir: &Path, expected: &Path) -> Result<()> {
        let output = Command::new("/bin/zsh")
            .args(["-dlic", "command -v node; command -v npm; command -v npx"])
            .env_clear()
            .env("HOME", home)
            .env("ZDOTDIR", dotdir)
            .env("PATH", "/usr/bin:/bin")
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let paths: Vec<PathBuf> = String::from_utf8(output.stdout)?
            .lines()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            paths,
            [
                expected.join("node"),
                expected.join("npm"),
                expected.join("npx")
            ]
        );
        Ok(())
    }
}
