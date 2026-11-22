use std::collections::BTreeMap;
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::config::{self, ConfigKind, ServicePaths};
use crate::instance::Instance;
use crate::language_tools::DeveloperTool;
use crate::service::{Service, ServiceAction, ServiceBackend};
use crate::setup;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Node,
    Go,
    Rust,
    Flutter,
    Php,
    Composer,
    Python,
    Java,
    Nginx,
    Mysql,
    Mariadb,
    Redis,
    Postgresql,
    Meilisearch,
    Elasticsearch,
}

impl Environment {
    pub const SIDEBAR: [Self; 14] = [
        Self::Node,
        Self::Go,
        Self::Rust,
        Self::Flutter,
        Self::Php,
        Self::Python,
        Self::Java,
        Self::Nginx,
        Self::Mysql,
        Self::Mariadb,
        Self::Redis,
        Self::Postgresql,
        Self::Meilisearch,
        Self::Elasticsearch,
    ];

    pub const ALL: [Self; 15] = [
        Self::Node,
        Self::Go,
        Self::Rust,
        Self::Flutter,
        Self::Php,
        Self::Composer,
        Self::Python,
        Self::Java,
        Self::Nginx,
        Self::Mysql,
        Self::Mariadb,
        Self::Redis,
        Self::Postgresql,
        Self::Meilisearch,
        Self::Elasticsearch,
    ];

    pub fn key(&self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Go => "go",
            Self::Rust => "rust",
            Self::Flutter => "flutter",
            Self::Php => "php",
            Self::Composer => "composer",
            Self::Python => "python",
            Self::Java => "java",
            Self::Nginx => "nginx",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Redis => "redis",
            Self::Postgresql => "postgresql",
            Self::Meilisearch => "meilisearch",
            Self::Elasticsearch => "elasticsearch",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Node => "Node.js",
            Self::Go => "Go",
            Self::Rust => "Rust",
            Self::Flutter => "Flutter",
            Self::Php => "PHP",
            Self::Composer => "Composer",
            Self::Python => "Python",
            Self::Java => "Java",
            Self::Nginx => "Nginx",
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
            Self::Redis => "Redis",
            Self::Postgresql => "PostgreSQL",
            Self::Meilisearch => "Meilisearch",
            Self::Elasticsearch => "Elasticsearch",
        }
    }

    pub fn runtime(&self) -> bool {
        matches!(
            self,
            Self::Node
                | Self::Go
                | Self::Rust
                | Self::Flutter
                | Self::Php
                | Self::Composer
                | Self::Python
                | Self::Java
        )
    }

    pub fn supports_default(&self) -> bool {
        self.runtime() && !matches!(self, Self::Flutter | Self::Java)
    }

    pub(crate) fn brew_bin_dir(&self) -> &'static str {
        if *self == Self::Python {
            "libexec/bin"
        } else {
            "bin"
        }
    }

    fn brew_formula(&self) -> Option<&'static str> {
        match self {
            Self::Elasticsearch => None,
            Self::Java => Some("openjdk"),
            _ => Some(self.key()),
        }
    }

    pub(crate) fn supports_instance(&self) -> bool {
        matches!(
            self,
            Self::Php
                | Self::Nginx
                | Self::Mysql
                | Self::Mariadb
                | Self::Redis
                | Self::Postgresql
                | Self::Meilisearch
        )
    }

    pub fn service_label(&self) -> Option<&'static str> {
        match self {
            Self::Php => Some("PHP-FPM"),
            Self::Nginx
            | Self::Mysql
            | Self::Mariadb
            | Self::Redis
            | Self::Postgresql
            | Self::Meilisearch
            | Self::Elasticsearch => Some(self.label()),
            _ => None,
        }
    }

    pub fn from_formula(name: &str) -> Option<Self> {
        let base = match name.split_once('@') {
            Some((base, suffix))
                if suffix.split('.').all(|part| {
                    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
                }) =>
            {
                base
            }
            Some(_) => return None,
            None => name,
        };
        Self::ALL
            .into_iter()
            .find(|environment| environment.brew_formula() == Some(base))
    }

    pub(crate) fn runtime_commands(&self) -> &[&str] {
        match self {
            Self::Node => &["node", "npm", "npx"],
            Self::Go => &["go", "gofmt"],
            Self::Rust => &["rustc", "cargo"],
            Self::Flutter => &["flutter", "dart"],
            Self::Php => &["php"],
            Self::Composer => &["composer"],
            Self::Python => &["python", "python3", "pip", "pip3"],
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Formula {
    pub name: String,
    pub environment: Environment,
    pub version: Option<String>,
    pub installed_versions: Vec<String>,
    pub install_path: Option<PathBuf>,
    pub installable: bool,
}

pub struct Inventory {
    pub formulae: Vec<Formula>,
    pub developer_tools: BTreeMap<String, String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PackageChange {
    Upgrade,
    Uninstall,
}

impl PackageChange {
    pub fn label(self) -> &'static str {
        match self {
            Self::Upgrade => "升级",
            Self::Uninstall => "卸载",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade",
            Self::Uninstall => "uninstall",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Brew {
    path: PathBuf,
    prefix: PathBuf,
    catalog: Arc<Mutex<BrewCatalog>>,
}

impl Brew {
    pub(crate) fn find_path() -> Option<PathBuf> {
        let standard_paths = if cfg!(target_arch = "aarch64") {
            ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        } else {
            ["/usr/local/bin/brew", "/opt/homebrew/bin/brew"]
        };
        let env_path = env::var_os("PATH").unwrap_or_default();
        env::split_paths(&env_path)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("brew"))
            .chain(standard_paths.into_iter().map(PathBuf::from))
            .find(|path| is_executable(path))
    }

    pub fn detect() -> Result<Option<Self>> {
        let Some(path) = Self::find_path() else {
            return Ok(None);
        };

        let mut brew = Self {
            path,
            prefix: PathBuf::new(),
            catalog: Arc::default(),
        };
        brew.prefix = PathBuf::from(brew.output(&["--prefix"], None)?.trim());
        ensure!(brew.prefix.is_absolute(), "Homebrew 返回了无效的安装路径");
        if let Some(saved) = cache::read::<SavedCatalog>(CATALOG_FILE)
            && saved.brew == brew.path
            && saved.prefix == brew.prefix
        {
            brew.catalog = Arc::new(Mutex::new(saved.catalog));
        }
        Ok(Some(brew))
    }

    pub(crate) fn same_installation(&self, other: &Self) -> bool {
        self.path == other.path && self.prefix == other.prefix
    }

    pub(crate) fn restore_catalog_for_display(path: &Path, formulae: &mut Vec<Formula>) {
        if let Some(saved) = cache::read::<SavedCatalog>(CATALOG_FILE)
            && saved.brew == path
        {
            saved.catalog.apply(formulae);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    pub fn version(&self) -> Result<String> {
        let output = self.output(&["--version"], None)?;
        output.lines().next().map(str::to_owned).context("Homebrew 未返回版本")
    }

    pub fn update(&self, proxy: Option<&str>, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
        let mut command = self.command(&["update"]);
        command.env("NONINTERACTIVE", "1");
        crate::command::stream(command, "brew update", proxy, log)
    }

    pub fn bin_path(&self, formula: &str) -> Result<PathBuf> {
        let environment = Environment::from_formula(formula)
            .filter(Environment::supports_default)
            .with_context(|| format!("{formula} 无法设置为终端默认版本"))?;
        let bin = self.opt_path(formula)?.join(environment.brew_bin_dir());
        for command in environment.runtime_commands() {
            ensure!(
                is_executable(&bin.join(command)),
                "{formula} 缺少可执行的 {command}"
            );
        }
        Ok(bin)
    }

    pub fn config_path(&self, formula: &str, kind: ConfigKind) -> Result<PathBuf> {
        let environment = Environment::from_formula(formula)
            .with_context(|| format!("不支持的公式 {formula}"))?;
        match kind {
            ConfigKind::FpmIni => self
                .service_paths(formula)?
                .php_ini
                .context("无法识别 PHP-FPM 的 php.ini"),
            ConfigKind::PhpIni => {
                ensure!(environment == Environment::Php, "只有 PHP 可以打开 php.ini");
                config::php_ini(&self.bin_path(formula)?)
            }
            ConfigKind::Service => {
                ensure!(
                    environment.service_label().is_some(),
                    "{formula} 没有可管理的服务"
                );
                let service = self.service_state(formula)?;
                let plist = service
                    .loaded_file
                    .or(service.file)
                    .with_context(|| format!("找不到 {formula} 的 Homebrew 服务启动项"))?;
                config::service_file(
                    self.prefix(),
                    &self.opt_path(formula)?,
                    formula,
                    environment,
                    &plist,
                )
            }
        }
    }

    pub fn service_paths(&self, formula: &str) -> Result<ServicePaths> {
        self.service_paths_at(formula, &self.opt_path(formula)?)
    }

    pub(crate) fn service_plist(&self, formula: &str) -> Result<PathBuf> {
        let service = self.service_state(formula)?;
        service
            .loaded_file
            .or(service.file)
            .with_context(|| format!("找不到 {formula} 的 Homebrew 服务启动项"))
    }

    pub(crate) fn service_paths_at(&self, formula: &str, opt: &Path) -> Result<ServicePaths> {
        let environment = Environment::from_formula(formula).context("无法识别服务")?;
        let plist = self.service_plist(formula)?;
        config::service_paths(
            self.prefix(),
            opt,
            formula,
            environment,
            &plist,
        )
    }

    pub fn opt_path(&self, formula: &str) -> Result<PathBuf> {
        ensure!(
            Environment::from_formula(formula).is_some(),
            "不支持的公式 {formula}"
        );
        let output = self.output(&["--prefix", "--installed", formula], None)?;
        let path = PathBuf::from(output.trim());
        ensure!(
            path.is_absolute() && path.is_dir(),
            "Homebrew 未返回 {formula} 的可用安装目录"
        );
        Ok(path)
    }

    pub fn inventory(&self) -> Result<Inventory> {
        let installed = self.output(&["list", "--formula", "--versions"], None)?;
        let mut formulae = merge_inventory(&installed, &[]);
        self.set_formula_paths(&mut formulae);
        if let Some(flutter) = self.installed_flutter()? {
            formulae.push(flutter);
        }
        self.use_cached_catalog(&mut formulae);
        Ok(Inventory {
            formulae,
            developer_tools: developer_tool_versions(&installed),
        })
    }

    pub(crate) fn use_cached_catalog(&self, formulae: &mut Vec<Formula>) {
        self.catalog.lock().unwrap().apply(formulae);
    }

    pub(crate) fn catalog_needs_refresh(&self) -> bool {
        let catalog = self.catalog.lock().unwrap();
        !catalog_fresh(&catalog.formulae) || !catalog_fresh(&catalog.flutter)
    }

    pub(crate) fn fetch_catalog(&self, proxy: Option<&str>, force: bool) -> CatalogUpdate {
        let (formulae, flutter) = {
            let catalog = self.catalog.lock().unwrap();
            (force || !catalog_fresh(&catalog.formulae), force || !catalog_fresh(&catalog.flutter))
        };
        CatalogUpdate {
            formulae: formulae.then(|| self.formula_catalog(proxy)),
            flutter: flutter.then(|| self.flutter_catalog(proxy)).flatten(),
        }
    }

    pub(crate) fn apply_catalog(&self, update: CatalogUpdate, formulae: &mut Vec<Formula>) -> Option<String> {
        let mut catalog = self.catalog.lock().unwrap();
        let mut changed = false;
        let mut warning = None;
        if let Some(result) = update.formulae {
            match result {
                Ok(entries) => {
                    catalog.formulae = Some(CatalogEntry::new(entries));
                    changed = true;
                }
                Err(error) => warning = Some(format!("可安装版本加载失败\n{error:#}")),
            }
        }
        if let Some(flutter) = update.flutter {
            catalog.flutter = Some(CatalogEntry::new(flutter));
            changed = true;
        }
        if changed {
            let _ = cache::write(CATALOG_FILE, &SavedCatalog {
                brew: self.path.clone(), prefix: self.prefix.clone(), catalog: catalog.clone(),
            });
        }
        catalog.apply(formulae);
        warning
    }

    fn set_formula_paths(&self, formulae: &mut [Formula]) {
        let installed: Vec<_> = formulae
            .iter_mut()
            .filter(|formula| !formula.installed_versions.is_empty())
            .collect();
        if installed.is_empty() {
            return;
        }
        let mut args = vec!["--prefix", "--installed"];
        args.extend(installed.iter().map(|formula| formula.name.as_str()));
        let Ok(output) = self.output(&args, None) else {
            return;
        };
        let paths = output.lines().map(|line| Path::new(line.trim()));
        if paths.clone().count() != installed.len() {
            return;
        }
        for (formula, path) in installed.into_iter().zip(paths) {
            if path.is_absolute() && path.is_dir() {
                formula.install_path = Some(path.to_owned());
            }
        }
    }

    fn formula_catalog(&self, proxy: Option<&str>) -> Result<Vec<CatalogFormula>> {
        let names = Environment::ALL
            .into_iter()
            .filter(|environment| *environment != Environment::Flutter)
            .filter_map(|environment| environment.brew_formula())
            .collect::<Vec<_>>()
            .join("|");
        let pattern = format!("/^({names})(@[0-9]+(\\.[0-9]+)*)?$/");
        let result = self.output(&["search", "--formula", &pattern], proxy)?;
        let names: Vec<_> = result
            .lines()
            .map(str::trim)
            .map(|name| name.strip_prefix("homebrew/core/").unwrap_or(name))
            .filter(|name| Environment::from_formula(name).is_some())
            .map(|name| format!("homebrew/core/{name}"))
            .collect();
        ensure!(!names.is_empty(), "Homebrew 暂未返回可用的公式");
        let mut args = vec!["info", "--formula", "--json=v2"];
        args.extend(names.iter().map(String::as_str));
        let info = self.output(&args, proxy)?;
        let catalog: Catalog = serde_json::from_str(&info).context("无法读取 Homebrew 公式信息")?;
        Ok(catalog.formulae)
    }

    fn flutter_catalog(&self, proxy: Option<&str>) -> Option<CatalogCask> {
        let output = self.output(&["info", "--cask", "--json=v2", "flutter"], proxy).ok()?;
        let catalog: CaskCatalog = serde_json::from_str(&output).ok()?;
        catalog.casks.into_iter().find(|cask| cask.token == "flutter")
    }

    fn installed_flutter(&self) -> Result<Option<Formula>> {
        let output = self.output(&["list", "--cask", "--versions"], None)
            .context("无法读取 Homebrew cask 安装列表")?;
        let installed: Vec<String> = output.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("flutter")).then(|| fields.map(str::to_owned).collect())
        }).unwrap_or_default();
        if installed.is_empty() {
            return Ok(None);
        }
        let install_path = self.output(&["--caskroom", "flutter"], None)
                .ok()
                .map(|output| PathBuf::from(output.trim()))
                .filter(|path| path.is_absolute() && path.is_dir());
        Ok(Some(Formula {
            name: "flutter".into(),
            environment: Environment::Flutter,
            version: None,
            installed_versions: installed,
            install_path,
            installable: false,
        }))
    }

    pub fn install(
        &self,
        formula: &str,
        proxy: Option<&str>,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        self.validate_install(formula, false)?;
        self.install_package(formula, proxy, log)
    }

    pub fn install_developer_tool(
        &self,
        formula: &str,
        proxy: Option<&str>,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        self.validate_install(formula, true)?;
        self.install_package(formula, proxy, log)
    }

    pub fn change_package(
        &self,
        formula: &str,
        change: PackageChange,
        proxy: Option<&str>,
        in_terminal: bool,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        let environment = Environment::from_formula(formula).context("不支持的 Homebrew 公式")?;
        let (kind, qualified) = install_target(formula);
        let installed = self.output(&["list", kind, "--versions", formula], None)?;
        let count = installed.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()?.rsplit('/').next()? == formula).then(|| fields.count())
        }).unwrap_or_default();
        ensure!(count != 0, "{formula} 已不在 Homebrew 安装列表中，请刷新");
        if change == PackageChange::Upgrade {
            self.check_database_conflict(environment)?;
        } else {
            ensure!(count == 1, "{formula} 有多个已安装的补丁版本，请在终端检查该公式后再卸载");
        }
        if environment.service_label().is_some() {
            let info = self.service_state(formula)?;
            ensure!(info.manageable(), "{formula} 由系统级 Homebrew 服务管理");
            ensure!(
                !info.running && !info.loaded && !info.auto_start(),
                "请先停止 {formula} 并关闭登录启动"
            );
        }
        let args = [change.command(), kind, &qualified];
        if in_terminal {
            return setup::install_brew_package_in_terminal(&self.path, &args, proxy, log);
        }
        let mut command = self.command(&args);
        command.envs([
            ("NONINTERACTIVE", "1"),
            ("HOMEBREW_NO_INSTALL_CLEANUP", "1"),
            ("HOMEBREW_NO_AUTOREMOVE", "1"),
        ]);
        if change == PackageChange::Upgrade {
            command.env("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", "1");
        }
        crate::command::stream(
            command,
            &format!("brew {} {kind} {qualified}", change.command()),
            proxy,
            log,
        )
    }

    pub fn install_in_terminal(
        &self,
        formula: &str,
        proxy: Option<&str>,
        developer_tool: bool,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        self.validate_install(formula, developer_tool)?;
        let (kind, qualified) = install_target(formula);
        setup::install_brew_package_in_terminal(&self.path, &["install", kind, &qualified], proxy, log)
    }

    pub(crate) fn install_package(
        &self,
        formula: &str,
        proxy: Option<&str>,
        log: &(dyn Fn(&str) + Sync),
    ) -> Result<()> {
        let (kind, qualified) = install_target(formula);
        let args = ["install", kind, &qualified];
        crate::command::stream(
            self.command(&args),
            &format!("brew install {kind} {qualified}"),
            proxy,
            log,
        )
    }

    fn validate_install(&self, formula: &str, developer_tool: bool) -> Result<()> {
        if developer_tool {
            ensure!(
                DeveloperTool::ALL.into_iter().any(|tool| tool.formula() == formula),
                "不支持的开发工具 {formula}"
            );
            return Ok(());
        }
        let environment = Environment::from_formula(formula)
            .with_context(|| format!("不支持的公式 {formula}"))?;
        self.check_database_conflict(environment)
    }

    fn check_database_conflict(&self, environment: Environment) -> Result<()> {
        let other = match environment {
            Environment::Mysql => Environment::Mariadb,
            Environment::Mariadb => Environment::Mysql,
            _ => return Ok(()),
        };
        let installed = self.output(&["list", "--formula", "--versions"], None)?;
        ensure!(
            !installed.lines().any(|line| {
                line.split_whitespace()
                    .next()
                    .and_then(Environment::from_formula)
                    == Some(other)
            }),
            "Homebrew 已安装 {}，与 {} 共用默认数据目录 var/mysql",
            other.label(),
            environment.label()
        );
        let data = self.prefix.join("var/mysql");
        if environment == Environment::Mariadb && data.is_dir() {
            ensure!(
                data.read_dir()?.next().is_none()
                    || (data.join("aria_log_control").is_file()
                        && !data.join("mysql.ibd").is_file()),
                "{} 中已有数据，无法确认是 MariaDB 数据目录",
                data.display()
            );
        } else if environment == Environment::Mysql {
            ensure!(
                !data.join("aria_log_control").exists(),
                "{} 中已有 MariaDB 数据",
                data.display()
            );
        }
        Ok(())
    }

    pub fn services(&self) -> Result<Vec<Service>> {
        let mut services: Vec<_> = self
            .service_info("--all")?
            .into_iter()
            .filter_map(|info| {
                let environment = Environment::from_formula(&info.name)?;
                environment.service_label()?;
                Some(Service {
                    backend: ServiceBackend::Homebrew,
                    running: info.running,
                    loaded: info.loaded,
                    auto_start: info.auto_start(),
                    manageable: info.manageable(),
                    formula: info.name,
                    environment,
                    status: info.status,
                })
            })
            .collect();
        services.sort_by(|left, right| {
            left.environment
                .cmp(&right.environment)
                .then(left.formula.cmp(&right.formula))
        });
        Ok(services)
    }

    pub fn prepare_instance(&self, formula: &str, instance: Option<&Instance>) -> Result<()> {
        let info = self.service_state(formula)?;
        ensure!(info.manageable(), "{formula} 由系统级 Homebrew 服务管理");
        ensure!(
            !info.running && !info.loaded && !info.auto_start(),
            "请先停止 {formula} 并关闭登录启动，再切换配置模式"
        );
        if let Some(instance) = instance {
            let opt = self.opt_path(formula)?;
            instance.prepare(&opt)?;
            instance.write_job(&opt, &info.label(formula))?;
        }
        Ok(())
    }

    pub fn service_action(
        &self,
        formula: &str,
        action: ServiceAction,
        instance: Option<&Instance>,
    ) -> Result<()> {
        ensure!(
            Environment::from_formula(formula)
                .and_then(|environment| environment.service_label())
                .is_some(),
            "不支持的服务 {formula}"
        );
        let opt = self.opt_path(formula)?;
        let info = self.service_state(formula)?;
        ensure!(info.manageable(), "{formula} 由系统级 Homebrew 服务管理");
        let auto_start = info.auto_start();
        let needs_start = match action {
            ServiceAction::Start => !info.running,
            ServiceAction::Restart => true,
            ServiceAction::Stop => false,
            ServiceAction::SetAutoStart(enabled) => {
                enabled != auto_start && (enabled || info.running)
            }
        };
        if needs_start
            && instance.is_none()
            && let Some(environment) = Environment::from_formula(formula)
        {
            self.check_database_conflict(environment)?;
        }
        let file = if let Some(instance) = instance.filter(|_| needs_start) {
            Some(instance.write_job(&opt, &info.label(formula))?)
        } else {
            None
        };
        let stop = |keep| {
            if let (Some(instance), Some(pid)) = (instance, info.pid) {
                instance.fast_stop_postgresql(&opt, pid)?;
            }
            self.stop_service(formula, keep)
        };
        let start = |register| self.start_service(formula, register, file.as_deref());

        match action {
            ServiceAction::Start if info.running => Ok(()),
            ServiceAction::Start => {
                if info.loaded {
                    stop(auto_start)?;
                }
                start(auto_start)
            }
            ServiceAction::Stop if !info.running && !info.loaded => Ok(()),
            ServiceAction::Stop => stop(auto_start),
            ServiceAction::Restart => {
                if info.running || info.loaded {
                    stop(auto_start)?;
                }
                start(auto_start)
            }
            ServiceAction::SetAutoStart(enabled) if enabled == auto_start => Ok(()),
            ServiceAction::SetAutoStart(true) => {
                if info.loaded || info.running {
                    stop(false)?;
                }
                if let Err(error) = start(true) {
                    stop(false).with_context(|| {
                        format!("登录自启设置失败，且无法清理服务注册\n{error:#}")
                    })?;
                    if info.running {
                        start(false).with_context(|| {
                            format!("登录自启设置失败，且无法恢复服务\n{error:#}")
                        })?;
                    }
                    return Err(error);
                }
                if !info.running {
                    stop(true)?;
                }
                Ok(())
            }
            ServiceAction::SetAutoStart(false) => {
                stop(false)?;
                if info.running {
                    start(false)?;
                }
                Ok(())
            }
        }
    }

    fn service_info(&self, name: &str) -> Result<Vec<ServiceInfo>> {
        let result = self.output(&["services", "info", "--json", name], None)?;
        if name == "--all" && result.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&result).context("无法读取 Homebrew 服务状态")
    }

    fn service_state(&self, formula: &str) -> Result<ServiceInfo> {
        self.service_info(formula)?
            .into_iter()
            .find(|info| info.name == formula)
            .with_context(|| format!("未检测到服务 {formula}"))
    }

    fn start_service(&self, formula: &str, register: bool, file: Option<&Path>) -> Result<()> {
        let subcommand = if register { "start" } else { "run" };
        let mut args = vec!["services", subcommand, formula];
        let file_arg;
        if let Some(file) = file {
            let path = file.to_str().context("服务启动项路径必须是 UTF-8 文本")?;
            file_arg = format!("--file={path}");
            args.push(&file_arg);
        }
        self.output(&args, None)?;
        let info = self.service_state(formula)?;
        if info.status == "error" {
            if let Some(exit_code) = info.exit_code {
                bail!("{formula} 启动后退出（退出码 {exit_code}）");
            }
            bail!("{formula} 启动后退出");
        }
        if file.is_some() {
            ensure!(info.loaded || info.running, "{formula} 的独立启动项未载入");
        }
        if register {
            ensure!(info.auto_start(), "{formula} 未注册为登录自启服务");
        }
        Ok(())
    }

    fn stop_service(&self, formula: &str, keep: bool) -> Result<()> {
        let mut args = vec!["services", "stop"];
        if keep {
            args.push("--keep");
        }
        args.push(formula);
        self.output(&args, None)?;
        let info = self.service_state(formula)?;
        ensure!(!info.running && !info.loaded, "{formula} 未能停止");
        ensure!(keep || !info.auto_start(), "{formula} 未能取消登录自启");
        Ok(())
    }

    fn output(&self, args: &[&str], proxy: Option<&str>) -> Result<String> {
        crate::command::output(self.command(args), &format!("brew {}", args.join(" ")), proxy)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.path);
        command.args(args).envs([
            ("HOMEBREW_NO_ENV_HINTS", "1"),
            ("HOMEBREW_NO_ANALYTICS", "1"),
        ]);
        match args.first().copied() {
            Some("install" | "upgrade") => {
                command.envs([
                    ("NONINTERACTIVE", "1"),
                    ("HOMEBREW_NO_INSTALL_CLEANUP", "1"),
                ]);
            }
            Some("search" | "info") => {
                command.envs([
                    ("HOMEBREW_NO_AUTO_UPDATE", "1"),
                    ("HOMEBREW_FORCE_API_AUTO_UPDATE", "1"),
                ]);
            }
            _ => {
                command.env("HOMEBREW_NO_AUTO_UPDATE", "1");
            }
        }
        command
    }
}

fn developer_tool_versions(installed: &str) -> BTreeMap<String, String> {
    installed
        .lines()
        .filter_map(|line| {
            let (name, versions) = line.trim().split_once(char::is_whitespace)?;
            DeveloperTool::ALL.into_iter().any(|tool| tool.formula() == name)
                .then(|| (name.to_owned(), versions.trim().to_owned()))
        })
        .collect()
}

fn install_target(formula: &str) -> (&'static str, String) {
    if formula == "flutter" {
        ("--cask", "homebrew/cask/flutter".into())
    } else {
        ("--formula", format!("homebrew/core/{formula}"))
    }
}

pub(crate) fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[derive(Deserialize)]
struct Catalog {
    formulae: Vec<CatalogFormula>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogFormula {
    name: String,
    versions: CatalogVersions,
    disabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogVersions {
    stable: Option<String>,
}

#[derive(Deserialize)]
struct CaskCatalog {
    casks: Vec<CatalogCask>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogCask {
    token: String,
    version: String,
    disabled: bool,
}

const CATALOG_FILE: &str = "homebrew-catalog-v1.json";
const CATALOG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CatalogEntry<T> {
    value: T,
    updated: SystemTime,
}

impl<T> CatalogEntry<T> {
    fn new(value: T) -> Self {
        Self { value, updated: SystemTime::now() }
    }
}

fn catalog_fresh<T>(entry: &Option<CatalogEntry<T>>) -> bool {
    entry.as_ref().is_some_and(|entry| {
        entry.updated.elapsed().is_ok_and(|age| age < CATALOG_MAX_AGE)
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct BrewCatalog {
    formulae: Option<CatalogEntry<Vec<CatalogFormula>>>,
    flutter: Option<CatalogEntry<CatalogCask>>,
}

impl BrewCatalog {
    fn apply(&self, formulae: &mut Vec<Formula>) {
        let catalog = self.formulae.as_ref().map_or(&[][..], |entry| entry.value.as_slice());
        let flutter = self.flutter.as_ref().map(|entry| &entry.value);
        let installed = std::mem::take(formulae).into_iter().filter(|formula| !formula.installed_versions.is_empty());
        *formulae = merge_catalog(installed, catalog, flutter);
    }
}

#[derive(Serialize, Deserialize)]
struct SavedCatalog {
    brew: PathBuf,
    prefix: PathBuf,
    catalog: BrewCatalog,
}

pub(crate) struct CatalogUpdate {
    formulae: Option<Result<Vec<CatalogFormula>>>,
    flutter: Option<CatalogCask>,
}

fn merge_inventory(installed: &str, catalog: &[CatalogFormula]) -> Vec<Formula> {
    let installed = installed.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        Some(Formula {
            name: name.to_owned(),
            environment: Environment::from_formula(name)?,
            version: None,
            installed_versions: fields.map(str::to_owned).collect(),
            install_path: None,
            installable: false,
        })
    });
    merge_catalog(installed, catalog, None)
}

fn merge_catalog(
    installed: impl Iterator<Item = Formula>,
    catalog: &[CatalogFormula],
    flutter: Option<&CatalogCask>,
) -> Vec<Formula> {
    let mut formulae: BTreeMap<_, _> = installed.map(|mut formula| {
        formula.version = None;
        formula.installable = false;
        (formula.name.clone(), formula)
    }).collect();
    for formula in catalog {
        let Some(environment) = Environment::from_formula(&formula.name) else {
            continue;
        };
        let installable = !formula.disabled && formula.versions.stable.is_some();
        if let Some(existing) = formulae.get_mut(&formula.name) {
            existing.version = formula.versions.stable.clone();
            existing.installable = installable;
        } else if installable {
            formulae.insert(
                formula.name.clone(),
                Formula {
                    name: formula.name.clone(),
                    environment,
                    version: formula.versions.stable.clone(),
                    installed_versions: Vec::new(),
                    install_path: None,
                    installable,
                },
            );
        }
    }
    if let Some(flutter) = flutter {
        if let Some(installed) = formulae.get_mut("flutter") {
            installed.version = Some(flutter.version.clone());
            installed.installable = !flutter.disabled;
        } else if !flutter.disabled {
            formulae.insert("flutter".into(), Formula {
                name: "flutter".into(),
                environment: Environment::Flutter,
                version: Some(flutter.version.clone()),
                installed_versions: Vec::new(),
                install_path: None,
                installable: true,
            });
        }
    }
    let mut formulae: Vec<_> = formulae.into_values().collect();
    formulae.sort_by(|left, right| {
        left.environment
            .cmp(&right.environment)
            .then_with(|| formula_version(&right.name).cmp(&formula_version(&left.name)))
    });
    formulae
}

fn formula_version(name: &str) -> (bool, Vec<u32>) {
    let Some((_, version)) = name.split_once('@') else {
        return (true, Vec::new());
    };
    (
        false,
        version
            .split('.')
            .filter_map(|part| part.parse().ok())
            .collect(),
    )
}

#[derive(Deserialize)]
struct ServiceInfo {
    name: String,
    pid: Option<u32>,
    service_name: Option<String>,
    running: bool,
    loaded: bool,
    status: String,
    exit_code: Option<i64>,
    registered: Option<bool>,
    user: Option<String>,
    file: Option<PathBuf>,
    loaded_file: Option<PathBuf>,
}

impl ServiceInfo {
    fn label(&self, formula: &str) -> String {
        self.service_name
            .clone()
            .unwrap_or_else(|| format!("homebrew.mxcl.{formula}"))
    }

    fn manageable(&self) -> bool {
        self.user.as_deref() != Some("root")
            && ![self.file.as_deref(), self.loaded_file.as_deref()]
                .into_iter()
                .flatten()
                .any(|path| {
                    path.starts_with("/Library/LaunchDaemons")
                        || path.starts_with("/Library/LaunchAgents")
                })
    }

    fn auto_start(&self) -> bool {
        self.manageable()
            && self.registered.unwrap_or_else(|| {
                env::var_os("HOME")
                    .zip(self.file.as_ref())
                    .is_some_and(|(home, file)| {
                        file.starts_with(PathBuf::from(home).join("Library/LaunchAgents"))
                    })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn failed_cask_listing_does_not_report_flutter_as_absent() {
        let fixture = Fixture::new("cask-list-failure", "exit 1");
        assert!(fixture.brew().installed_flutter().is_err());
    }

    #[test]
    fn refuses_mariadb_install_when_homebrew_already_has_mysql() {
        let fixture = Fixture::new(
            "mariadb-conflict",
            r#"
            [ "$*" = 'list --formula --versions' ] || exit 2
            printf 'mysql@8.4 8.4.9\n'
            "#,
        );
        let message = fixture
            .brew()
            .install("mariadb@11.8", None, &|_| {})
            .unwrap_err()
            .to_string();
        assert!(message.contains("MySQL"));
        assert!(message.contains("var/mysql"));
    }

    #[test]
    fn inventory_keeps_installed_versions_and_filters_disabled_catalog_entries() {
        let installed = "node@20 20.19.5\ngo 1.25.5 1.26.1\nfnm 1.39.0\n";
        let catalog: Catalog = serde_json::from_str(
            r#"{"formulae":[
                {"name":"node@20","versions":{"stable":"20.20.0"},"disabled":true},
                {"name":"node@18","versions":{"stable":"18.20.0"},"disabled":true},
                {"name":"node@24","versions":{"stable":"24.11.1"},"disabled":false}
            ]}"#,
        )
        .unwrap();
        let formulae = merge_inventory(installed, &catalog.formulae);
        assert_eq!(
            formulae
                .iter()
                .map(|formula| formula.name.as_str())
                .collect::<Vec<_>>(),
            ["node@24", "node@20", "go"]
        );
        assert!(formulae[0].installable);
        assert!(!formulae[1].installable);
        assert_eq!(formulae[1].installed_versions, ["20.19.5"]);
        assert_eq!(formulae[2].installed_versions, ["1.25.5", "1.26.1"]);
    }

    #[test]
    fn uses_package_paths_reported_by_homebrew() -> Result<()> {
        let fixture = Fixture::new("install-paths", "exit 1");
        let brew = fixture.brew();
        let go = fixture.0.join("packages/go");
        let redis = fixture.0.join("packages/redis");
        fs::create_dir_all(go.join("bin"))?;
        fs::create_dir_all(&redis)?;
        for command in ["go", "gofmt"] {
            let binary = go.join("bin").join(command);
            fs::write(&binary, "")?;
            fs::set_permissions(binary, fs::Permissions::from_mode(0o700))?;
        }
        let mut formulae = merge_inventory("redis 8.6.2\ngo 1.26.1\n", &[]);
        brew.set_formula_paths(&mut formulae);
        assert_eq!(formulae[0].install_path.as_ref(), Some(&go));
        assert_eq!(formulae[1].install_path.as_ref(), Some(&redis));
        assert_eq!(brew.bin_path("go")?, go.join("bin"));
        Ok(())
    }

    #[test]
    fn service_actions_preserve_running_and_login_preferences() {
        let fixture = Fixture::new(
            "services",
            r#"
            usenv_state="$0.state"
            [ -f "$usenv_state" ] || printf 'false false\n' > "$usenv_state"
            read -r usenv_running usenv_registered < "$usenv_state"
            case "$2" in
                info) printf '[{"name":"redis","running":%s,"loaded":%s,"status":"none","registered":%s}]\n' \
                    "$usenv_running" "$usenv_running" "$usenv_registered" ;;
                start)
                    [ "$usenv_running" != true ] || exit 0
                    printf 'true true\n' > "$usenv_state" ;;
                run) printf 'true %s\n' "$usenv_registered" > "$usenv_state" ;;
                stop)
                    [ "$3" = --keep ] || usenv_registered=false
                    printf 'false %s\n' "$usenv_registered" > "$usenv_state" ;;
                *) exit 1 ;;
            esac
        "#,
        );
        let brew = fixture.brew();
        fs::create_dir_all(brew.prefix().join("packages/redis")).unwrap();
        for (action, running, auto_start) in [
            (ServiceAction::SetAutoStart(true), false, true),
            (ServiceAction::Start, true, true),
            (ServiceAction::Stop, false, true),
            (ServiceAction::Restart, true, true),
            (ServiceAction::SetAutoStart(false), true, false),
            (ServiceAction::Restart, true, false),
            (ServiceAction::SetAutoStart(true), true, true),
        ] {
            brew.service_action("redis", action, None).unwrap();
            let state = brew.service_state("redis").unwrap();
            assert_eq!(
                (state.running, state.auto_start()),
                (running, auto_start),
                "{action:?}"
            );
        }
    }

    #[test]
    fn authenticated_proxy_reaches_install_without_leaking_credentials_in_errors() {
        use std::sync::Mutex;

        let fixture = Fixture::new(
            "proxy",
            r#"
            [ "$1 $2 $3" = 'install --formula homebrew/core/node@24' ] || exit 2
            [ "$http_proxy" = 'http://alice:secret@127.0.0.1:1080' ] || exit 3
            [ "$https_proxy" = "$http_proxy" ] && [ "$all_proxy" = "$http_proxy" ] || exit 4
            printf 'Downloading\r50%%\r100%%\n'
            printf 'proxy authentication failed: %s\n' "$http_proxy" >&2
            printf '> Proxy-Authorization: Basic c2VjcmV0\n' >&2
            exit 5
        "#,
        );
        let lines = Mutex::new(Vec::new());
        let error = fixture
            .brew()
            .install("node@24", Some("http://alice:secret@127.0.0.1:1080"), &|line| {
                lines.lock().unwrap().push(line.to_owned());
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("5"));
        assert!(!error.contains("alice") && !error.contains("secret"));
        let lines = lines.into_inner().unwrap();
        assert!(lines[0].ends_with(" install --formula homebrew/core/node@24"));
        assert_eq!(&lines[1..4], ["Downloading", "50%", "100%"]);
        assert!(lines[4].starts_with("proxy authentication failed"));
        assert!(!lines[4].contains("secret"));
        assert_eq!(lines[5], "> Proxy-Authorization: [代理凭据]");
    }

    #[test]
    fn independent_service_uses_its_own_plist_and_preserves_edits() -> Result<()> {
        let fixture = Fixture::new(
            "managed-service",
            r#"
            dir="${0%/*}"
            case "$2" in
                info)
                    running=false; [ ! -f "$dir/started" ] || running=true
                    printf '[{"name":"redis","running":%s,"loaded":%s,"status":"none","registered":false}]\n' \
                        "$running" "$running" ;;
                run)
                    [ "$3" = redis ] && [ "$#" -eq 4 ] && [ -f "${4#--file=}" ] || exit 2
                    printf '%s' "${4#--file=}" > "$dir/started" ;;
                stop) /bin/rm "$dir/started" ;;
                *) exit 1 ;;
            esac
            "#,
        );
        let brew = fixture.brew();
        let opt = brew.prefix().join("packages/redis");
        fs::create_dir_all(opt.join("bin"))?;
        fs::write(opt.join("bin/redis-server"), "")?;
        let instance = Instance::new(&fixture.0.join("usenv services"), "redis")?;
        brew.prepare_instance("redis", Some(&instance))?;
        fs::write(instance.config(), "port 6381\n")?;
        let data = instance.root().join("data/dump.rdb");
        fs::write(&data, "existing data")?;

        brew.service_action("redis", ServiceAction::Start, Some(&instance))?;
        let plist = instance.root().join("service.plist");
        assert_eq!(
            fs::read_to_string(fixture.0.join("started"))?,
            plist.to_string_lossy()
        );
        assert_eq!(
            config::service_file(brew.prefix(), &opt, "redis", Environment::Redis, &plist)?,
            instance.config()
        );
        assert_eq!(
            config::service_paths(brew.prefix(), &opt, "redis", Environment::Redis, &plist)?
                .executable,
            opt.join("bin/redis-server")
        );
        assert!(brew.prepare_instance("redis", None).is_err());
        brew.service_action("redis", ServiceAction::Stop, Some(&instance))?;
        brew.prepare_instance("redis", None)?;
        brew.prepare_instance("redis", Some(&instance))?;
        assert_eq!(fs::read_to_string(instance.config())?, "port 6381\n");
        assert_eq!(fs::read_to_string(data)?, "existing data");
        Ok(())
    }

    #[test]
    fn config_uses_loaded_service_plist_before_formula_default() -> Result<()> {
        let fixture = Fixture::new(
            "service-config",
            r#"
            [ "$1 $2 $3 $4" = 'services info --json redis' ] || exit 2
            usenv_dir="${0%/*}"
            printf '[{"name":"redis","running":true,"loaded":true,"status":"started","file":"%s/default.plist","loaded_file":"%s/loaded.plist"}]\n' "$usenv_dir" "$usenv_dir"
            "#,
        );
        fs::create_dir_all(fixture.0.join("packages/redis"))?;
        fs::write(
            fixture.0.join("loaded.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict><key>ProgramArguments</key><array>
              <string>/opt/homebrew/opt/redis/bin/redis-server</string>
              <string>/custom profile/redis.conf</string>
            </array></dict></plist>"#,
        )?;
        assert_eq!(
            fixture.brew().config_path("redis", ConfigKind::Service)?,
            Path::new("/custom profile/redis.conf")
        );
        Ok(())
    }

    #[test]
    fn independent_fpm_uses_a_private_ini_without_overwriting_it() -> Result<()> {
        let fixture = Fixture::new("fpm-ini", "exit 1");
        let opt = fixture.0.join("packages/php@8.4");
        fs::create_dir_all(opt.join("bin"))?;
        fs::create_dir_all(opt.join("sbin"))?;
        fs::write(opt.join("sbin/php-fpm"), "")?;
        let source = fixture.0.join("brew php.ini");
        fs::write(&source, "memory_limit = 128M\n")?;
        let php = opt.join("bin/php");
        fs::write(
            &php,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'Loaded Configuration File: {}'\n",
                source.display()
            ),
        )?;
        fs::set_permissions(php, fs::Permissions::from_mode(0o700))?;
        let instance = Instance::new(&fixture.0.join("usenv services"), "php@8.4")?;
        instance.prepare(&opt)?;
        let ini = instance.paths(&opt).php_ini.context("missing ini")?;
        assert_eq!(fs::read(&ini)?, fs::read(&source)?);
        fs::write(&ini, "memory_limit = 256M\n")?;
        let plist = instance.write_job(&opt, "homebrew.mxcl.php@8.4")?;
        let paths = config::service_paths(
            fixture.brew().prefix(),
            &opt,
            "php@8.4",
            Environment::Php,
            &plist,
        )?;
        assert_eq!(paths.php_ini.as_ref(), Some(&ini));
        assert_eq!(fs::read_to_string(&ini)?, "memory_limit = 256M\n");
        assert_eq!(fs::read_to_string(source)?, "memory_limit = 128M\n");
        Ok(())
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str, script: &str) -> Self {
            let dir = env::temp_dir().join(format!("usenv-brew-{}-{name}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            let fixture = Self(dir);
            let brew = fixture.brew();
            let prefix = r#"#!/bin/sh
            if [ "$1 $2" = '--prefix --installed' ]; then
                shift 2
                for formula in "$@"; do
                    path="${0%/*}/packages/$formula"
                    [ -d "$path" ] || exit 1
                    printf '%s\n' "$path"
                done
                exit 0
            fi
            "#;
            fs::write(brew.path(), format!("{prefix}\n{script}")).unwrap();
            fs::set_permissions(brew.path(), fs::Permissions::from_mode(0o700)).unwrap();
            fixture
        }

        fn brew(&self) -> Brew {
            Brew {
                path: self.0.join("brew"),
                prefix: self.0.clone(),
                catalog: Arc::default(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
