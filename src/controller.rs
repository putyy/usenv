use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::brew::{Brew, CatalogUpdate, Environment, Formula, PackageChange};
use crate::service::{Service, ServiceAction, ServiceBackend};
use crate::brew_preferences::{self, BrewConfig, BrewPreferences};
use crate::cache;
use crate::config::{self, ConfigKind, ServicePaths};
use crate::diagnostics::{self, CurrentJava, ServiceReport};
use crate::elasticsearch::{Elasticsearch, launch_domain};
use crate::instance::Instance;
use crate::language_tools::{self, NpmInfo, RustComponent, RustComponents, ToolRuntime};
use crate::login;
use crate::php_extensions::{self, Extension, ExtensionInfo, Operation as ExtensionOperation};
use crate::runtimes::{Mise, MiseTool};
use crate::settings::{MAX_PARALLEL_INSTALLS, MIN_PARALLEL_INSTALLS, Settings, SettingsStore, ShellFile, Theme};
use crate::setup;
use crate::sites::{self, Site};
use crate::updates::{self, UpdateStatus};
pub use crate::language_tools::DeveloperTool;

#[derive(Clone, Default)]
pub struct Snapshot {
    pub generation: u64,
    pub managers_checked: bool,
    pub brew_path: Option<PathBuf>,
    pub brew_detection_failed: bool,
    pub brew_inventory_error: Option<String>,
    pub mise_inventory_errors: BTreeMap<Environment, String>,
    pub brew_details: Option<BrewDetails>,
    pub mise: Option<Mise>,
    pub mise_version: Option<String>,
    pub mise_configs: Option<Vec<PathBuf>>,
    pub formulae: Vec<Formula>,
    pub mise_tools: Vec<MiseTool>,
    pub services: Vec<Service>,
    pub settings: Settings,
    pub service_paths: Option<(String, ServicePaths)>,
    cached_service_paths: BTreeMap<(String, bool), CachedServicePaths>,
    pub app_login: login::LoginStatus,
    pub busy: Option<String>,
    pub service_operation: Option<(String, ServiceAction)>,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub output_tasks: VecDeque<OutputTask>,
    pub runtime_details: Option<(RuntimeTarget, RuntimeDetails)>,
    pub developer_tools: BTreeMap<String, String>,
    pub terminal_report: Option<(Environment, String)>,
    pub current_java: CurrentJavaStatus,
    pub service_report: Option<(String, ServiceReport)>,
    pub update: UpdateStatus,
    pub dismissed_update_version: Option<String>,
    pub site: Option<Site>,
    pending_visibility: BTreeMap<Environment, (u64, bool)>,
    pending_theme: Option<(u64, Theme)>,
    pending_parallel_installs: Option<(u64, u8)>,
}

#[derive(Clone, Default)]
pub enum CurrentJavaStatus {
    #[default]
    Idle,
    Checking(u64),
    Ready(Option<CurrentJava>),
    Failed(String),
}

#[derive(Clone)]
pub struct BrewDetails {
    pub version: Option<String>,
    pub prefix: PathBuf,
    pub config: Result<BrewConfig, String>,
}

impl BrewDetails {
    fn read(brew: &Brew) -> Self {
        Self {
            version: brew.version().ok(),
            prefix: brew.prefix().to_owned(),
            config: BrewConfig::discover(brew.prefix()).map_err(|error| format!("{error:#}")),
        }
    }
}

#[derive(Clone)]
struct CachedServicePaths {
    paths: ServicePaths,
    config_modified: Option<SystemTime>,
}

impl CachedServicePaths {
    fn config_modified(paths: &ServicePaths) -> Option<SystemTime> {
        paths.config.as_ref()?.metadata().ok()?.modified().ok()
    }
}

const INVENTORY_FILE: &str = "installed-inventory-v1.json";

#[derive(Default, Deserialize, Serialize)]
struct SavedInventory {
    brew: Option<SavedBrewInventory>,
    mise: Option<SavedMiseInventory>,
}

#[derive(Deserialize, Serialize)]
struct SavedBrewInventory {
    path: PathBuf,
    formulae: Vec<Formula>,
    developer_tools: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
struct SavedMiseInventory {
    path: PathBuf,
    tools: Vec<MiseTool>,
}

impl SavedInventory {
    fn restore(&self, snapshot: &mut Snapshot) {
        if let Some(saved) = &self.brew
            && snapshot.brew_path.as_ref() == Some(&saved.path)
        {
            snapshot.formulae = saved.formulae.clone();
            snapshot.developer_tools = saved.developer_tools.clone();
        }
        if let Some(saved) = &self.mise
            && snapshot.mise.as_ref().map(Mise::path) == Some(saved.path.as_path())
        {
            snapshot.mise_tools = saved.tools.clone();
        }
    }

    fn update(&mut self, snapshot: &Snapshot) {
        if snapshot.brew_inventory_error.is_none() {
            self.brew = snapshot.brew_path.clone().map(|path| SavedBrewInventory {
                path,
                formulae: snapshot.formulae
                    .iter()
                    .filter(|formula| !formula.installed_versions.is_empty())
                    .cloned()
                    .map(|mut formula| {
                        formula.version = None;
                        formula.installable = false;
                        formula
                    })
                    .collect(),
                developer_tools: snapshot.developer_tools.clone(),
            });
        }
        if let Some(mise) = &snapshot.mise {
            let previous = self.mise.as_ref().filter(|saved| saved.path == mise.path());
            let mut tools: Vec<_> = snapshot.mise_tools.iter()
                .filter(|tool| !snapshot.mise_inventory_errors.contains_key(&tool.environment))
                .cloned()
                .collect();
            if let Some(previous) = previous {
                tools.extend(previous.tools.iter()
                    .filter(|tool| snapshot.mise_inventory_errors.contains_key(&tool.environment))
                    .cloned());
            }
            self.mise = Some(SavedMiseInventory { path: mise.path().to_owned(), tools });
        } else {
            self.mise = None;
        }
    }

    fn save(&self) {
        let _ = cache::write(INVENTORY_FILE, self);
    }
}

#[derive(Clone)]
pub struct OutputTask {
    pub id: u64,
    pub title: String,
    pub status: TaskStatus,
    pub lines: Arc<VecDeque<Arc<str>>>,
    pub omitted_lines: usize,
    pub terminal_install: Option<TerminalInstall>,
    pub command_line_tools_required: bool,
    scope: Option<InstallScope>,
    action: Action,
    refresh: Option<RefreshQuery>,
}

impl OutputTask {
    fn new(id: u64, title: String, action: Action, scope: Option<InstallScope>, status: TaskStatus) -> Self {
        Self {
            id, title, action, scope, status,
            lines: Arc::default(),
            omitted_lines: 0,
            terminal_install: None,
            command_line_tools_required: false,
            refresh: None,
        }
    }

    fn append(&mut self, line: &str) {
        const MAX_LINES: usize = 1200;
        const MAX_LINE_BYTES: usize = 8192;
        let line = if line.len() > MAX_LINE_BYTES {
            let mut end = MAX_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            Arc::<str>::from(format!("{}…", &line[..end]))
        } else {
            Arc::from(line)
        };
        let lines = Arc::make_mut(&mut self.lines);
        if lines.len() == MAX_LINES {
            lines.pop_front();
            self.omitted_lines += 1;
        }
        lines.push_back(line);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Finished,
    Failed,
    RefreshFailed,
    Cancelled,
}

impl TaskStatus {
    fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstallScope {
    Brew,
    MiseSetup,
    Mise(Environment),
    NpmBrew,
    NpmMise,
}

impl InstallScope {
    fn conflicts(self, other: Self) -> bool {
        use InstallScope::*;
        match (self, other) {
            (Brew, NpmBrew) | (NpmBrew, Brew) | (NpmBrew, NpmMise) | (NpmMise, NpmBrew) => true,
            (MiseSetup, Mise(_) | NpmMise) | (Mise(_) | NpmMise, MiseSetup) => true,
            (Mise(Environment::Node), NpmMise) | (NpmMise, Mise(Environment::Node)) => true,
            _ => self == other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeTarget {
    Php(String),
    Npm(ToolRuntime),
    Rust(ToolRuntime),
}

#[derive(Clone, Debug)]
enum RefreshQuery {
    BrewInventory,
    BrewSetup,
    BrewUpdate,
    MiseInventory,
    MiseSetup,
    Runtime(RuntimeTarget),
}

impl std::fmt::Display for RefreshQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("操作已完成，重新读取状态时出错")
    }
}

impl RefreshQuery {
    fn run(self, brew: &mut Option<Brew>, snapshot: &mut Snapshot) -> Result<()> {
        self.read(brew, snapshot).context(self)
    }

    fn read(&self, brew: &mut Option<Brew>, snapshot: &mut Snapshot) -> Result<()> {
        match self {
            Self::BrewInventory => refresh_brew(brew.as_ref().context("未检测到 Homebrew")?, snapshot),
            Self::BrewSetup | Self::BrewUpdate => {
                if matches!(self, Self::BrewSetup) {
                    *brew = None;
                    *brew = Some(Brew::detect()?.context("安装结束后未检测到 Homebrew")?);
                }
                let current = brew.as_ref().context("未检测到 Homebrew")?;
                snapshot.brew_path = Some(current.path().to_owned());
                snapshot.brew_detection_failed = false;
                snapshot.brew_details = Some(BrewDetails::read(current));
                refresh_brew(current, snapshot)?;
                refresh_brew_catalog(current, snapshot, matches!(self, Self::BrewUpdate));
                Ok(())
            }
            Self::MiseInventory => refresh_mise(snapshot),
            Self::MiseSetup => {
                snapshot.mise = Mise::detect();
                snapshot.mise_version = None;
                let mise = snapshot.mise.as_ref().context("安装结束后未检测到 Mise")?;
                snapshot.mise_version = Some(mise.version()?);
                refresh_mise(snapshot)
            }
            Self::Runtime(target) => inspect_runtime(brew.as_ref(), snapshot, target.clone()),
        }
    }
}

#[derive(Clone)]
pub enum RuntimeDetails {
    Php(Vec<ExtensionInfo>),
    Npm(NpmInfo),
    Rust(RustComponents),
}

#[derive(Clone)]
pub enum TerminalInstall {
    Formula(String),
    DeveloperTool(DeveloperTool),
    PackageChange(String, PackageChange),
}

impl TerminalInstall {
    fn label(&self) -> &str {
        match self {
            Self::Formula(formula) | Self::PackageChange(formula, _) => formula,
            Self::DeveloperTool(tool) => tool.label(),
        }
    }
}

impl Snapshot {
    fn service(&self, formula: &str) -> Option<&Service> {
        self.services.iter().find(|service| service.formula == formula)
    }

    fn finish_java_check(&mut self, id: u64, result: Result<Option<CurrentJava>, String>) {
        if matches!(self.current_java, CurrentJavaStatus::Checking(current) if current == id) {
            self.current_java = match result {
                Ok(java) => CurrentJavaStatus::Ready(java),
                Err(error) => CurrentJavaStatus::Failed(error),
            };
        }
    }

    pub fn brew_available(&self) -> bool {
        self.brew_path.is_some() && !self.brew_detection_failed
    }

    fn mark_mise_inventory_stale(&mut self, reason: String) {
        self.mise_inventory_errors = Environment::ALL.into_iter()
            .filter(|env| Mise::supports(*env))
            .map(|environment| (environment, reason.clone()))
            .collect();
    }

    fn mise_tools_for_operation(&self, environment: Environment) -> Result<std::borrow::Cow<'_, [MiseTool]>> {
        if self.mise_inventory_errors.contains_key(&environment) {
            let mise = self.mise.as_ref().context("请先刷新以检测 Mise")?;
            return Ok(std::borrow::Cow::Owned(mise.installed()
                .context("无法确认 Mise 安装清单，请刷新后重试")?));
        }
        Ok(std::borrow::Cow::Borrowed(&self.mise_tools))
    }

    pub fn brew_preferences(&self) -> Option<&BrewPreferences> {
        self.brew_details.as_ref()?.config.as_ref().ok()?.preferences.as_ref().ok()
    }

    fn paths_key(&self, formula: &str) -> (String, bool) {
        (formula.to_owned(), self.settings.managed_services.contains(formula))
    }

    fn toggle_cached_paths(&mut self, formula: &str) -> bool {
        if self.service_paths.as_ref().is_some_and(|(selected, _)| selected == formula) {
            self.service_paths = None;
            return true;
        }
        if let Some(cached) = self.cached_service_paths.get(&self.paths_key(formula))
            && cached.config_modified == CachedServicePaths::config_modified(&cached.paths)
        {
            self.service_paths = Some((formula.to_owned(), cached.paths.clone()));
            return true;
        }
        false
    }

    fn invalidate_paths(&mut self, formula: &str) {
        self.cached_service_paths.retain(|(cached, _), _| cached != formula);
        if self.service_paths.as_ref().is_some_and(|(selected, _)| selected == formula) {
            self.service_paths = None;
        }
        if self.service_report.as_ref().is_some_and(|(selected, _)| selected == formula) {
            self.service_report = None;
        }
    }

    fn replace_brew_formulae(&mut self, formulae: Vec<Formula>) {
        let changed: Vec<_> = self.formulae.iter().filter(|old| {
            !old.installed_versions.is_empty() && !formulae.iter().any(|new| {
                old.name == new.name && old.installed_versions == new.installed_versions
                    && old.install_path == new.install_path
            })
        }).map(|formula| formula.name.clone()).collect();
        for formula in changed {
            self.invalidate_paths(&formula);
        }
        self.formulae = formulae;
    }

    pub fn environment_visible(&self, environment: Environment) -> bool {
        self.pending_visibility.get(&environment).map_or_else(
            || self.settings.environment_visible(environment),
            |(_, visible)| *visible,
        )
    }

    pub fn theme(&self) -> Theme {
        self.pending_theme
            .map_or(self.settings.theme, |(_, theme)| theme)
    }

    pub fn parallel_installs(&self) -> u8 {
        self.pending_parallel_installs.map_or(self.settings.parallel_installs, |(_, limit)| limit)
    }

    pub fn installation_counts(&self) -> (usize, usize) {
        self.output_tasks.iter().filter(|task| task.scope.is_some()).fold((0, 0), |(running, queued), task| match task.status {
            TaskStatus::Running => (running + 1, queued),
            TaskStatus::Queued => (running, queued + 1),
            _ => (running, queued),
        })
    }

    pub fn active_install(&self, action: &Action) -> Option<TaskStatus> {
        let scope = action.install_scope()?;
        let title = action.log_title()?;
        self.output_tasks.iter().find(|task| {
            task.scope == Some(scope) && task.title == title && task.status.active()
        }).map(|task| task.status)
    }

    pub fn action_available(&self, action: &Action) -> bool {
        if self.brew_detection_failed
            && matches!(action.install_scope().or_else(|| action.foreground_scope(self)),
                Some(InstallScope::Brew | InstallScope::NpmBrew))
        {
            return false;
        }
        if let Action::Service(formula, operation) = action
            && !self.service(formula).is_some_and(|service| service.can_perform(*operation))
        {
            return false;
        }
        if matches!(action, Action::CheckUpdate) {
            return !matches!(self.update, UpdateStatus::Checking);
        }
        if matches!(action, Action::ReadCurrentJava) {
            return !matches!(self.current_java, CurrentJavaStatus::Checking(_));
        }
        if matches!(action, Action::CheckTerminal(..)) {
            return !self.output_tasks.iter().any(|task| task.scope.is_none() && task.status.active());
        }
        if action.install_scope().is_some() {
            return self.active_install(action).is_none();
        }
        self.busy.is_none() && action.foreground_scope(self).is_none_or(|scope| {
            !self.output_tasks.iter().any(|task| {
                task.status == TaskStatus::Running && task.scope.is_some_and(|running| running.conflicts(scope))
            })
        })
    }
}

#[derive(Clone)]
pub enum Action {
    Refresh,
    OpenHomebrewInstaller,
    UpdateHomebrew,
    OpenBrewConfig,
    SaveBrewPreferences(BrewPreferences),
    OpenTerminalInstall(TerminalInstall),
    InstallCommandLineTools(u64),
    InstallMiseManager,
    OpenMiseGlobalConfig,
    ToggleMiseConfigs,
    OpenMiseConfig(PathBuf),
    Install(String),
    InstallMise(Environment, String),
    ChangeBrew(String, PackageChange),
    UninstallMise(Environment, String, String),
    CheckTerminal(Environment, Option<PathBuf>),
    ReadCurrentJava,
    DiagnoseService(String),
    CheckUpdate,
    CreateSite { nginx: String, php: String, root: PathBuf, port: u16 },
    OpenSite,
    InspectRuntime(RuntimeTarget),
    PhpExtension(String, Extension, ExtensionOperation),
    OpenPhpExtensionConfig(String, Extension),
    NpmChange(ToolRuntime, String, bool),
    InstallRustComponent(ToolRuntime, RustComponent),
    InstallDeveloperTool(DeveloperTool),
    SetMiseGlobal(Environment, String, String),
    SetDefault(String),
    ClearDefault(String),
    Service(String, ServiceAction),
    OpenConfig(String, ConfigKind),
    EditShellFile(ShellFile),
    ShowPaths(String),
    SetManaged(String, bool),
    OpenInstance(String),
    OpenCredentials(String),
    SetProxy(String),
    AppLogin(bool),
    SetQuietLogin(bool),
}

impl Action {
    fn install_scope(&self) -> Option<InstallScope> {
        match self {
            Self::OpenHomebrewInstaller | Self::UpdateHomebrew | Self::OpenTerminalInstall(_) | Self::Install(_)
            | Self::ChangeBrew(..)
            | Self::InstallDeveloperTool(_) | Self::PhpExtension(_, _, ExtensionOperation::Install(_)) => Some(InstallScope::Brew),
            Self::InstallMiseManager => Some(InstallScope::MiseSetup),
            Self::InstallMise(environment, _) | Self::UninstallMise(environment, ..) => {
                Some(InstallScope::Mise(*environment))
            }
            Self::NpmChange(ToolRuntime::Brew(_), _, _) => Some(InstallScope::NpmBrew),
            Self::NpmChange(ToolRuntime::Mise { .. }, _, _) => Some(InstallScope::NpmMise),
            Self::InstallRustComponent(ToolRuntime::Mise { .. }, _) => Some(InstallScope::Mise(Environment::Rust)),
            Self::InstallRustComponent(ToolRuntime::Brew(_), _) => Some(InstallScope::Brew),
            _ => None,
        }
    }

    fn foreground_scope(&self, snapshot: &Snapshot) -> Option<InstallScope> {
        match self {
            Self::SetMiseGlobal(..) => Some(InstallScope::MiseSetup),
            Self::Service(formula, _) | Self::DiagnoseService(formula) => {
                snapshot.service(formula).map(|service| match service.backend {
                    ServiceBackend::Homebrew => InstallScope::Brew,
                    ServiceBackend::MiseLaunchd => InstallScope::Mise(service.environment),
                })
            }
            Self::SetDefault(_) | Self::SetManaged(..) | Self::SaveBrewPreferences(_)
            | Self::PhpExtension(..) => {
                Some(InstallScope::Brew)
            }
            Self::CreateSite { .. } => Some(InstallScope::Brew),
            _ => None,
        }
    }

    fn log_title(&self) -> Option<String> {
        match self {
            Self::OpenHomebrewInstaller => Some("安装 Homebrew".into()),
            Self::UpdateHomebrew => Some("更新 Homebrew".into()),
            Self::OpenTerminalInstall(TerminalInstall::PackageChange(formula, change)) => {
                Some(format!("在终端{}{formula}", change.label()))
            }
            Self::OpenTerminalInstall(install) => Some(format!("终端安装 {}", install.label())),
            Self::InstallMiseManager => Some("安装 Mise".into()),
            Self::Install(formula) => Some(format!("安装 {formula}")),
            Self::InstallMise(environment, version) => {
                Some(format!("安装 {} {version}", environment.label()))
            }
            Self::ChangeBrew(formula, change) => Some(format!("{} {formula}", change.label())),
            Self::UninstallMise(_, name, version) => Some(format!("卸载 {name} {version}（Mise）")),
            Self::PhpExtension(formula, extension, ExtensionOperation::Install(_)) => {
                Some(format!("为 {formula} 安装 {}", extension.label()))
            }
            Self::NpmChange(target, package, install) => {
                let verb = if *install { "安装" } else { "卸载" };
                Some(format!("{} · npm {verb} {package}", runtime_install_label(target)))
            }
            Self::InstallRustComponent(target, component) => Some(format!(
                "{} · 安装 {}", runtime_install_label(target), component.label()
            )),
            Self::InstallDeveloperTool(tool) => Some(format!("安装 {}", tool.label())),
            _ => None,
        }
    }

    fn progress(&self) -> String {
        match self {
            Self::Refresh => "正在读取开发环境…".into(),
            Self::OpenHomebrewInstaller => "正在打开 Homebrew 安装…".into(),
            Self::UpdateHomebrew => "正在更新 Homebrew…".into(),
            Self::OpenBrewConfig => "正在打开 Homebrew 配置…".into(),
            Self::SaveBrewPreferences(_) => "正在保存 Homebrew 配置…".into(),
            Self::OpenTerminalInstall(_) => "正在打开终端安装…".into(),
            Self::InstallCommandLineTools(_) => "正在打开 macOS 安装提示…".into(),
            Self::InstallMiseManager => "正在下载安装 Mise…".into(),
            Self::OpenMiseGlobalConfig | Self::OpenMiseConfig(_) => "正在打开 Mise 配置…".into(),
            Self::ToggleMiseConfigs => "正在读取 Mise 配置文件…".into(),
            Self::Install(formula) => format!("正在安装 {formula}…"),
            Self::InstallMise(environment, version) => {
                format!("Mise 正在安装 {} {version}…", environment.label())
            }
            Self::ChangeBrew(..) | Self::UninstallMise(..) => {
                format!("正在{}…", self.log_title().unwrap_or_default())
            }
            Self::CheckTerminal(environment, _) => format!("正在检查终端中的 {}…", environment.label()),
            Self::ReadCurrentJava => "正在读取终端中的 Java…".into(),
            Self::DiagnoseService(formula) => format!("正在读取 {formula} 服务详情…"),
            Self::CheckUpdate => "正在检查应用更新…".into(),
            Self::CreateSite { .. } => "正在生成本地站点配置…".into(),
            Self::OpenSite => "正在打开站点配置…".into(),
            Self::InspectRuntime(_) => "正在读取工具和扩展…".into(),
            Self::PhpExtension(_, extension, _) => format!("正在更新 {} 扩展…", extension.label()),
            Self::OpenPhpExtensionConfig(_, extension) => {
                format!("正在打开 {} 配置…", extension.label())
            }
            Self::NpmChange(_, _, _)
            | Self::InstallRustComponent(_, _)
            | Self::InstallDeveloperTool(_) => format!("正在{}…", self.log_title().unwrap_or_default()),
            Self::SetMiseGlobal(..) => "正在更新 Mise 全局版本…".into(),
            Self::SetDefault(_) | Self::ClearDefault(_) => "正在更新终端默认版本…".into(),
            Self::Service(formula, _) => format!("正在更新 {formula} 服务…"),
            Self::OpenConfig(formula, _) => format!("正在打开 {formula} 配置…"),
            Self::EditShellFile(file) => format!("正在打开 {}…", file.name()),
            Self::ShowPaths(formula) => format!("正在读取 {formula} 路径…"),
            Self::SetManaged(formula, true) => format!("正在为 {formula} 准备独立实例…"),
            Self::SetManaged(formula, false) => format!("正在恢复 {formula} 的 Homebrew 配置…"),
            Self::OpenInstance(formula) => format!("正在打开 {formula} 目录…"),
            Self::OpenCredentials(formula) => format!("正在打开 {formula} 初始凭据…"),
            Self::SetProxy(_) => "正在保存代理…".into(),
            Self::AppLogin(_) => "正在更新登录项…".into(),
            Self::SetQuietLogin(_) => "正在保存登录启动偏好…".into(),
        }
    }
}

fn runtime_install_label(runtime: &ToolRuntime) -> String {
    match runtime {
        ToolRuntime::Brew(formula) => formula.clone(),
        ToolRuntime::Mise { name, version } => format!("{name} {version}"),
    }
}

#[derive(Clone, Copy)]
pub enum Preference {
    Visibility(Environment, bool),
    Theme(Theme),
    ParallelInstalls(u8),
}

enum Request {
    Action(Action),
    RememberProject(PathBuf, u64),
    StartupCatalog(Brew, CatalogUpdate),
    QueueInstall(QueuedInstall),
    FinishInstall(Box<FinishedInstall>),
    Preference {
        preference: Preference,
        revision: u64,
    },
}

struct QueuedInstall {
    id: u64,
    action: Action,
    scope: InstallScope,
    sender: mpsc::Sender<Request>,
}

struct FinishedInstall {
    id: u64,
    action: Action,
    brew: Option<Brew>,
    snapshot: Snapshot,
    result: Result<()>,
}

#[derive(Clone)]
pub struct Controller {
    state: Arc<Mutex<Snapshot>>,
    sender: mpsc::Sender<Request>,
}

impl Controller {
    pub fn new() -> Self {
        let mut snapshot = Snapshot {
            settings: load_settings().unwrap_or_default(),
            brew_path: Brew::find_path(),
            mise: Mise::detect(),
            ..Snapshot::default()
        };
        let saved = cache::read::<SavedInventory>(INVENTORY_FILE).unwrap_or_default();
        saved.restore(&mut snapshot);
        if let Some(path) = &snapshot.brew_path {
            Brew::restore_catalog_for_display(path, &mut snapshot.formulae);
        }
        let state = Arc::new(Mutex::new(snapshot));
        let (sender, receiver) = mpsc::channel();
        let worker_state = state.clone();
        let startup_sender = sender.clone();
        std::thread::spawn(move || run_worker(worker_state, receiver, startup_sender, saved));
        Self { state, sender }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().clone()
    }

    pub fn dismiss_update_notice(&self, version: &str) {
        let mut state = self.state.lock().unwrap();
        state.dismissed_update_version = Some(version.to_owned());
        state.generation += 1;
    }

    pub fn snapshot_if_changed(&self, generation: u64) -> Option<Snapshot> {
        let state = self.state.lock().unwrap();
        (state.generation != generation).then(|| state.clone())
    }

    pub fn submit(&self, action: Action) {
        let mut state = self.state.lock().unwrap();
        if !state.action_available(&action) {
            return;
        }
        state.generation += 1;
        if matches!(&action, Action::CheckUpdate) {
            state.update = UpdateStatus::Checking;
            let address = state.settings.proxy.clone();
            let update_state = self.state.clone();
            if let Err(error) = std::thread::Builder::new().spawn(move || {
                let proxy = (!address.is_empty()).then_some(address.as_str());
                let result = updates::check(proxy);
                let mut snapshot = update_state.lock().unwrap();
                snapshot.update = result;
                snapshot.generation += 1;
            }) {
                state.update = UpdateStatus::Failed(format!("无法启动更新检查：{error}"));
            }
            return;
        }
        if let Action::ShowPaths(formula) = &action
            && state.toggle_cached_paths(formula)
        {
            state.notice = None;
            state.error = None;
            return;
        }
        if matches!(&action, Action::ReadCurrentJava) {
            let id = state.generation;
            state.current_java = CurrentJavaStatus::Checking(id);
            let worker_state = self.state.clone();
            if let Err(error) = std::thread::Builder::new().spawn(move || {
                let result = diagnostics::current_java().map_err(|error| format!("{error:#}"));
                let mut state = worker_state.lock().unwrap();
                state.finish_java_check(id, result);
                state.generation += 1;
            }) {
                state.finish_java_check(id, Err(format!("无法开始读取 Java：{error}")));
            }
            return;
        }
        if let Action::CheckTerminal(environment, directory) = &action {
            let (environment, directory) = (*environment, directory.clone());
            if state.busy.is_none() {
                state.notice = None;
                state.error = None;
            }
            state.terminal_report = None;
            let id = state.generation;
            let check_current_java = environment == Environment::Java && directory.is_none();
            if check_current_java {
                state.current_java = CurrentJavaStatus::Checking(id);
            }
            let mut title = format!("检查终端 · {}", environment.label());
            if let Some(project) = directory.as_deref().and_then(Path::file_name) {
                title.push_str(" · ");
                title.push_str(&project.to_string_lossy());
            }
            state.output_tasks.push_back(OutputTask::new(id, title, action, None, TaskStatus::Running));
            let mut defaults = Vec::new();
            if let Some(formula) = state.settings.defaults.get(environment.key()) {
                defaults.push(format!("Homebrew 终端默认  {formula}"));
            }
            for tool in state.mise_tools.iter().filter(|tool| tool.environment == environment && tool.active == Some(true)) {
                defaults.push(format!("Mise 全局  {} {}", tool.name, tool.version));
            }
            let worker_state = self.state.clone();
            let project = directory.clone();
            if let Err(error) = std::thread::Builder::new().spawn(move || {
                run_terminal_check(&worker_state, id, environment, directory, defaults);
            }) {
                if check_current_java {
                    state.finish_java_check(id, Err(format!("无法启动终端检测：{error}")));
                }
                fail_task(&mut state, id, format!("无法启动终端检测：{error}"));
            } else if let Some(directory) = project
                && self.sender.send(Request::RememberProject(directory, id)).is_err()
            {
                state.error = Some("后台操作已停止，无法保存最近检查的项目。".into());
            }
            return;
        }
        if let (Some(scope), Some(title)) = (action.install_scope(), action.log_title()) {
            if state.busy.is_none() {
                state.notice = None;
                state.error = None;
            }
            let id = state.generation;
            state.output_tasks.push_back(OutputTask::new(
                id, title, action.clone(), Some(scope), TaskStatus::Queued,
            ));
            let request = Request::QueueInstall(QueuedInstall {
                id,
                action,
                scope,
                sender: self.sender.clone(),
            });
            if self.sender.send(request).is_err() {
                fail_task(&mut state, id, "安装队列已停止，请重新打开应用。".into());
            }
            return;
        }
        state.notice = None;
        state.error = None;
        if matches!(&action, Action::Refresh) {
            state.terminal_report = None;
            state.current_java = CurrentJavaStatus::Idle;
        }
        state.busy = Some(action.progress());
        state.service_operation = match &action {
            Action::Service(formula, operation) => Some((formula.clone(), *operation)),
            _ => None,
        };
        if self.sender.send(Request::Action(action)).is_err() {
            state.busy = None;
            state.service_operation = None;
            state.error = Some("后台操作已停止，请重新打开应用。".into());
        }
    }

    pub fn cancel_install(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(task) = state.output_tasks.iter_mut()
            .find(|task| task.id == id && task.status == TaskStatus::Queued)
        {
            task.status = TaskStatus::Cancelled;
            task.append("已取消排队");
            prune_task_history(&mut state, id);
            state.generation += 1;
        }
    }

    pub fn retry_task(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.output_tasks.iter().position(|task| task.id == id) else {
            return;
        };
        let task = &state.output_tasks[index];
        let action = task.action.clone();
        if task.status == TaskStatus::RefreshFailed {
            let Some(scope) = task.scope else {
                return;
            };
            if state.active_install(&action).is_some() {
                return;
            }
            let task = &mut state.output_tasks[index];
            task.status = TaskStatus::Queued;
            task.append("已排队重新读取状态");
            state.generation += 1;
            let job = QueuedInstall { id, scope, action, sender: self.sender.clone() };
            if self.sender.send(Request::QueueInstall(job)).is_err() {
                fail_task(&mut state, id, "后台任务已停止，请重新打开应用。".into());
            }
        } else if task.status == TaskStatus::Failed {
            drop(state);
            self.submit(action);
        }
    }

    pub fn set_preference(&self, preference: Preference) {
        let mut state = self.state.lock().unwrap();
        if let Preference::ParallelInstalls(limit) = preference
            && (!(MIN_PARALLEL_INSTALLS..=MAX_PARALLEL_INSTALLS).contains(&limit)
                || state.parallel_installs() == limit)
        {
            return;
        }
        state.generation += 1;
        let revision = state.generation;
        if self
            .sender
            .send(Request::Preference {
                preference,
                revision,
            })
            .is_ok()
        {
            match preference {
                Preference::Visibility(environment, visible) => {
                    state
                        .pending_visibility
                        .insert(environment, (revision, visible));
                }
                Preference::Theme(theme) => state.pending_theme = Some((revision, theme)),
                Preference::ParallelInstalls(limit) => {
                    state.pending_parallel_installs = Some((revision, limit));
                }
            }
        } else {
            state.error = Some("后台操作已停止，请重新打开应用。".into());
        }
    }
}

fn run_terminal_check(
    state: &Mutex<Snapshot>,
    id: u64,
    environment: Environment,
    directory: Option<PathBuf>,
    mut defaults: Vec<String>,
) {
    let log = |line: &str| append_task_line(state, id, line);
    for line in &defaults {
        log(line);
    }
    let result = diagnostics::terminal(environment, directory.as_deref(), &log);
    let mut snapshot = state.lock().unwrap();
    if environment == Environment::Java && directory.is_none() {
        let java = result.as_ref().map(|report| report.java.clone())
            .map_err(|error| format!("{error:#}"));
        snapshot.finish_java_check(id, java);
    }
    match result {
        Ok(report) => {
            defaults.push(report.summary);
            snapshot.terminal_report = Some((environment, defaults.join("\n")));
            if let Some(task) = snapshot.output_tasks.iter_mut().find(|task| task.id == id) {
                task.status = TaskStatus::Finished;
                task.append("终端检测完成");
            }
            prune_task_history(&mut snapshot, id);
            snapshot.generation += 1;
        }
        Err(error) => fail_task(&mut snapshot, id, format!("{error:#}")),
    }
}

fn run_worker(
    state: Arc<Mutex<Snapshot>>,
    receiver: mpsc::Receiver<Request>,
    startup_sender: mpsc::Sender<Request>,
    mut saved: SavedInventory,
) {
    let mut brew = None;
    let mut startup_catalog_pending = startup_inventory(&state, &mut brew, startup_sender, &mut saved);
    let mut queued = VecDeque::new();
    let mut running = BTreeMap::new();
    loop {
        match receiver.recv_timeout(Duration::from_secs(15)) {
            Ok(Request::Action(action)) => {
                let mut snapshot = state.lock().unwrap().clone();
                let refresh_catalog = matches!(
                    &action, Action::Refresh | Action::SetProxy(_) | Action::SaveBrewPreferences(_)
                );
                let save_inventory = matches!(&action, Action::Refresh | Action::SetMiseGlobal(..));
                let java_changed = matches!(&action, Action::SetMiseGlobal(Environment::Java, ..));
                if refresh_catalog {
                    startup_catalog_pending = false;
                }
                let tools_task = match &action {
                    Action::InstallCommandLineTools(id) => Some(*id),
                    _ => None,
                };
                let result = execute(action, &mut brew, &mut snapshot, &|_| {});
                if save_inventory {
                    saved.update(&snapshot);
                }
                let tools_task = tools_task.filter(|_| result.is_ok());
                if let Err(error) = &result {
                    snapshot.error = Some(format!("{error:#}"));
                }
                snapshot.busy = None;
                snapshot.service_operation = None;
                let mut state = state.lock().unwrap();
                snapshot.generation = state.generation + 1;
                snapshot.pending_visibility = std::mem::take(&mut state.pending_visibility);
                snapshot.pending_theme = state.pending_theme;
                snapshot.pending_parallel_installs = state.pending_parallel_installs;
                snapshot.output_tasks = std::mem::take(&mut state.output_tasks);
                snapshot.terminal_report = state.terminal_report.clone();
                snapshot.current_java = if java_changed && result.is_ok() {
                    CurrentJavaStatus::Idle
                } else {
                    state.current_java.clone()
                };
                snapshot.update = state.update.clone();
                snapshot.dismissed_update_version = state.dismissed_update_version.clone();
                if let Some(id) = tools_task
                    && let Some(task) = snapshot.output_tasks.iter_mut().find(|task| task.id == id)
                {
                    task.command_line_tools_required = false;
                }
                *state = snapshot;
                drop(state);
                if save_inventory {
                    saved.save();
                }
            }
            Ok(Request::StartupCatalog(source, update)) => {
                if startup_catalog_pending
                    && let Some(current) = brew.as_ref().filter(|brew| brew.same_installation(&source))
                {
                    let mut formulae = state.lock().unwrap().formulae.clone();
                    let warning = current.apply_catalog(update, &mut formulae);
                    let mut snapshot = state.lock().unwrap();
                    snapshot.formulae = formulae;
                    if snapshot.error.is_none() && snapshot.busy.is_none() {
                        snapshot.error = warning;
                    }
                    snapshot.generation += 1;
                }
                startup_catalog_pending = false;
            }
            Ok(Request::QueueInstall(job)) => queued.push_back(job),
            Ok(Request::RememberProject(directory, id)) => {
                match SettingsStore::discover().and_then(|store| store.remember_project(directory)) {
                    Ok(settings) => {
                        let mut snapshot = state.lock().unwrap();
                        snapshot.settings = settings;
                        snapshot.generation += 1;
                    }
                    Err(error) => append_task_line(&state, id, &format!("无法保存最近检查的项目：{error:#}")),
                }
            }
            Ok(Request::FinishInstall(job)) => {
                running.remove(&job.id);
                let mut snapshot = state.lock().unwrap();
                let inventory_changed = finish_install(&mut snapshot, &mut brew, *job);
                if inventory_changed {
                    saved.update(&snapshot);
                }
                drop(snapshot);
                if inventory_changed {
                    saved.save();
                }
            }
            Ok(Request::Preference {
                preference,
                revision,
            }) => {
                let result = SettingsStore::discover().and_then(|store| match preference {
                    Preference::Visibility(environment, visible) => {
                        store.set_environment_visible(environment, visible)
                    }
                    Preference::Theme(theme) => store.set_theme(theme),
                    Preference::ParallelInstalls(limit) => store.set_parallel_installs(limit),
                });
                let mut state = state.lock().unwrap();
                if let Ok(settings) = result.as_ref() {
                    state.settings = settings.clone();
                }
                let completed = match preference {
                    Preference::Visibility(environment, _)
                        if state
                            .pending_visibility
                            .get(&environment)
                            .is_some_and(|pending| pending.0 == revision) =>
                    {
                        state.pending_visibility.remove(&environment);
                        true
                    }
                    Preference::Theme(_)
                        if state
                            .pending_theme
                            .is_some_and(|pending| pending.0 == revision) =>
                    {
                        state.pending_theme = None;
                        true
                    }
                    Preference::ParallelInstalls(_)
                        if state.pending_parallel_installs.is_some_and(|pending| pending.0 == revision) =>
                    {
                        state.pending_parallel_installs = None;
                        true
                    }
                    _ => false,
                };
                if completed {
                    if let Err(error) = result {
                        let setting = match preference {
                            Preference::Visibility(..) => "侧栏设置",
                            Preference::Theme(_) => "主题设置",
                            Preference::ParallelInstalls(_) => "同时安装数量",
                        };
                        state.error = Some(format!("无法保存{setting}：{error:#}"));
                    }
                    state.generation += 1;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let login = login::status();
                let services = brew.as_ref().and_then(|brew: &Brew| brew.services().ok());
                let tools = state.lock().unwrap().mise_tools.clone();
                let elastic = elasticsearch_services(&tools);
                let mut snapshot = state.lock().unwrap();
                let mut service_changed = replace_services(&mut snapshot, elastic, ServiceBackend::MiseLaunchd);
                if let Some(services) = services {
                    service_changed |= replace_services(&mut snapshot, services, ServiceBackend::Homebrew);
                }
                let login_changed = snapshot.app_login != login;
                snapshot.app_login = login;
                if service_changed || login_changed {
                    snapshot.generation += 1;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        start_installs(&state, &brew, &mut queued, &mut running, &mut startup_catalog_pending);
    }
}

fn startup_inventory(
    state: &Arc<Mutex<Snapshot>>,
    brew: &mut Option<Brew>,
    sender: mpsc::Sender<Request>,
    saved: &mut SavedInventory,
) -> bool {
    let mut detected = state.lock().unwrap().clone();
    let result = scan_local(brew, &mut detected);
    saved.update(&detected);
    let address = detected.settings.proxy.clone();
    {
        let mut snapshot = state.lock().unwrap();
        snapshot.managers_checked = detected.managers_checked;
        snapshot.brew_path = detected.brew_path;
        snapshot.brew_detection_failed = detected.brew_detection_failed;
        snapshot.brew_inventory_error = detected.brew_inventory_error;
        snapshot.mise_inventory_errors = detected.mise_inventory_errors;
        snapshot.brew_details = detected.brew_details;
        snapshot.mise = detected.mise;
        snapshot.mise_version = detected.mise_version;
        snapshot.replace_brew_formulae(detected.formulae);
        snapshot.mise_tools = detected.mise_tools;
        snapshot.developer_tools = detected.developer_tools;
        snapshot.services = detected.services;
        snapshot.app_login = detected.app_login;
        if snapshot.busy.is_none() {
            snapshot.error = result.err().map(|error| format!("{error:#}")).or(detected.error);
        }
        snapshot.generation += 1;
    }
    saved.save();
    let Some(source) = brew.as_ref().filter(|brew| brew.catalog_needs_refresh()).cloned() else {
        return false;
    };
    std::thread::Builder::new().spawn(move || {
        let proxy = (!address.is_empty()).then_some(address.as_str());
        let update = source.fetch_catalog(proxy, false);
        let _ = sender.send(Request::StartupCatalog(source, update));
    }).is_ok()
}

fn runnable_install(
    queued: &VecDeque<QueuedInstall>,
    running: &BTreeMap<u64, InstallScope>,
    limit: u8,
) -> Option<usize> {
    if running.len() >= usize::from(limit) {
        return None;
    }
    queued.iter().position(|job| !running.values().any(|scope| job.scope.conflicts(*scope)))
}

fn start_installs(
    state: &Arc<Mutex<Snapshot>>,
    brew: &Option<Brew>,
    queued: &mut VecDeque<QueuedInstall>,
    running: &mut BTreeMap<u64, InstallScope>,
    startup_catalog_pending: &mut bool,
) {
    while !queued.is_empty() {
        let mut current = state.lock().unwrap();
        if current.busy.is_some() {
            return;
        }
        let limit = current.settings.parallel_installs.min(current.parallel_installs());
        let Some(index) = runnable_install(queued, running, limit) else {
            return;
        };
        let Some(QueuedInstall { id, action, scope, sender }) = queued.remove(index) else {
            return;
        };
        let Some(task) = current.output_tasks.iter_mut()
            .find(|task| task.id == id && task.status == TaskStatus::Queued) else {
            continue;
        };
        task.status = TaskStatus::Running;
        let refresh = task.refresh.clone();
        if matches!(&action, Action::UpdateHomebrew) {
            *startup_catalog_pending = false;
        }
        current.generation += 1;
        let mut snapshot = current.clone();
        drop(current);
        snapshot.output_tasks.clear();
        snapshot.notice = None;
        snapshot.error = None;

        let mut brew = brew.clone();
        let worker_state = state.clone();
        running.insert(id, scope);
        let worker = std::thread::Builder::new().spawn(move || {
            let log = |line: &str| append_task_line(&worker_state, id, line);
            let result = match refresh {
                Some(query) => {
                    let result = query.run(&mut brew, &mut snapshot);
                    if result.is_ok() {
                        snapshot.notice = Some("已重新读取状态".into());
                    }
                    result
                }
                None => execute(action.clone(), &mut brew, &mut snapshot, &log),
            };
            let _ = sender.send(Request::FinishInstall(Box::new(FinishedInstall {
                id, action, brew, snapshot, result,
            })));
        });
        if let Err(error) = worker {
            running.remove(&id);
            fail_task(&mut state.lock().unwrap(), id, format!("无法启动安装任务：{error}"));
        }
    }
}

fn finish_install(state: &mut Snapshot, brew: &mut Option<Brew>, job: FinishedInstall) -> bool {
    let FinishedInstall { id, action, brew: job_brew, snapshot, result } = job;
    let refresh_failed = result.as_ref().is_err_and(|error| error.is::<RefreshQuery>());
    if result.is_ok() || refresh_failed {
        let java_changed = match &action {
            Action::InstallMise(Environment::Java, _) | Action::UninstallMise(Environment::Java, ..) => true,
            Action::Install(formula) | Action::ChangeBrew(formula, _)
            | Action::OpenTerminalInstall(TerminalInstall::Formula(formula)
                | TerminalInstall::PackageChange(formula, _)) => {
                Environment::from_formula(formula) == Some(Environment::Java)
            }
            _ => false,
        };
        if java_changed {
            state.current_java = CurrentJavaStatus::Idle;
        }
    }
    if let Some(error) = result.as_ref().err()
        && let Some(query) = error.downcast_ref::<RefreshQuery>()
    {
        let reason = format!("{error:#}");
        match query {
            RefreshQuery::BrewInventory | RefreshQuery::BrewSetup | RefreshQuery::BrewUpdate => {
                state.brew_inventory_error = Some(reason);
            }
            RefreshQuery::MiseInventory => {
                if let Action::InstallMise(environment, _) | Action::UninstallMise(environment, ..) = &action {
                    state.mise_inventory_errors.insert(*environment, reason);
                }
            }
            RefreshQuery::MiseSetup => {
                state.mark_mise_inventory_stale(reason);
            }
            RefreshQuery::Runtime(_) => {}
        }
    }
    let mut inventory_changed = false;
    match &action {
        Action::OpenHomebrewInstaller | Action::UpdateHomebrew | Action::Install(_) | Action::ChangeBrew(..) | Action::InstallDeveloperTool(_)
        | Action::OpenTerminalInstall(_) if result.is_ok() || snapshot.notice.is_some() => {
            if matches!(&action, Action::OpenHomebrewInstaller) && job_brew.is_some() {
                *brew = job_brew;
                state.brew_path = snapshot.brew_path.clone();
                state.brew_detection_failed = snapshot.brew_detection_failed;
                state.brew_details = snapshot.brew_details.clone();
            } else if matches!(&action, Action::UpdateHomebrew) && job_brew.is_some() {
                state.brew_details = snapshot.brew_details.clone();
            }
            if let Action::Install(formula)
                | Action::OpenTerminalInstall(TerminalInstall::Formula(formula)) = &action
            {
                state.invalidate_paths(formula);
            }
            if let Action::ChangeBrew(formula, _)
                | Action::OpenTerminalInstall(TerminalInstall::PackageChange(formula, _)) = &action
            {
                state.invalidate_paths(formula);
                if state.runtime_details.as_ref().is_some_and(|(target, _)| match target {
                    RuntimeTarget::Php(name)
                    | RuntimeTarget::Npm(ToolRuntime::Brew(name))
                    | RuntimeTarget::Rust(ToolRuntime::Brew(name)) => name == formula,
                    _ => false,
                }) {
                    state.runtime_details = None;
                }
            }
            inventory_changed = result.is_ok() || (!refresh_failed && snapshot.notice.is_some() && matches!(
                &action, Action::ChangeBrew(..) | Action::OpenTerminalInstall(TerminalInstall::PackageChange(..))
            ));
            if inventory_changed {
                let mut formulae = snapshot.formulae.clone();
                if let Some(brew) = brew.as_ref() {
                    brew.use_cached_catalog(&mut formulae);
                }
                state.replace_brew_formulae(formulae);
                state.developer_tools = snapshot.developer_tools.clone();
                replace_services(state, snapshot.services.iter().filter(|service| {
                    service.backend == ServiceBackend::Homebrew
                }).cloned().collect(), ServiceBackend::Homebrew);
                state.brew_inventory_error = None;
            }
        }
        Action::InstallMiseManager if result.is_ok() || snapshot.notice.is_some() => {
            if let Some(mise) = &snapshot.mise {
                state.mise = Some(mise.clone());
                state.mise_version = snapshot.mise_version.clone();
                state.mise_configs = None;
            }
            inventory_changed = result.is_ok();
            if inventory_changed {
                state.mise_tools = snapshot.mise_tools.clone();
                replace_services(state, snapshot.services.iter().filter(|service| {
                    service.backend == ServiceBackend::MiseLaunchd
                }).cloned().collect(), ServiceBackend::MiseLaunchd);
                state.mise_inventory_errors.clear();
            }
        }
        Action::InstallMise(environment, _) | Action::UninstallMise(environment, ..)
            if result.is_ok() =>
        {
            state.mise_inventory_errors.remove(environment);
            state.mise_tools.retain(|tool| tool.environment != *environment);
            state.mise_tools.extend(snapshot.mise_tools.iter().filter(|tool| {
                tool.environment == *environment
            }).cloned());
            if let Action::UninstallMise(_, name, version) = &action
                && state.runtime_details.as_ref().is_some_and(|(target, _)| match target {
                    RuntimeTarget::Npm(ToolRuntime::Mise { name: current, version: selected })
                    | RuntimeTarget::Rust(ToolRuntime::Mise { name: current, version: selected }) => {
                        current == name && selected == version
                    }
                    _ => false,
                })
            {
                state.runtime_details = None;
            }
            let belongs = |service: &Service| {
                service.backend == ServiceBackend::MiseLaunchd && service.environment == *environment
            };
            let old_names: Vec<_> = state.services.iter().filter(|s| belongs(s))
                .map(|service| service.formula.clone()).collect();
            for name in old_names {
                state.invalidate_paths(&name);
            }
            let services = snapshot.services.iter().filter(|s| belongs(s)).cloned().collect();
            replace_services_matching(state, services, belongs);
            inventory_changed = true;
        }
        Action::PhpExtension(formula, _, _) if !refresh_failed => merge_runtime_details(
            state, &snapshot, RuntimeTarget::Php(formula.clone()),
        ),
        Action::NpmChange(target, _, _) if !refresh_failed => merge_runtime_details(
            state, &snapshot, RuntimeTarget::Npm(target.clone()),
        ),
        Action::InstallRustComponent(target, _) if !refresh_failed => merge_runtime_details(
            state, &snapshot, RuntimeTarget::Rust(target.clone()),
        ),
        _ => (),
    }

    if let Some(task) = state.output_tasks.iter_mut().find(|task| task.id == id) {
        match result {
            Ok(()) => {
                task.status = TaskStatus::Finished;
                task.append(snapshot.notice.as_deref().unwrap_or("已完成"));
                if let Some(warning) = snapshot.error {
                    for line in warning.lines() {
                        task.append(line);
                    }
                }
            }
            Err(error) => {
                if let Some(query) = error.downcast_ref::<RefreshQuery>() {
                    task.refresh = Some(query.clone());
                    if let Some(notice) = &snapshot.notice {
                        task.append(notice);
                    }
                } else if error.is::<crate::command::NeedsTerminal>() {
                    task.terminal_install = match &action {
                        Action::Install(formula) => Some(TerminalInstall::Formula(formula.clone())),
                        Action::InstallDeveloperTool(tool) => Some(TerminalInstall::DeveloperTool(*tool)),
                        Action::ChangeBrew(formula, change) => {
                            Some(TerminalInstall::PackageChange(formula.clone(), *change))
                        }
                        _ => None,
                    };
                }
                task.command_line_tools_required = error.is::<php_extensions::CommandLineToolsRequired>();
                task_failed(task, format!("{error:#}"));
            }
        }
    }
    prune_task_history(state, id);
    state.generation += 1;
    inventory_changed
}

fn merge_runtime_details(state: &mut Snapshot, source: &Snapshot, target: RuntimeTarget) {
    if state.runtime_details.as_ref().is_some_and(|(current, _)| *current == target) {
        state.runtime_details = source.runtime_details.clone();
    }
}

fn append_task_line(state: &Mutex<Snapshot>, id: u64, line: &str) {
    let mut state = state.lock().unwrap();
    if let Some(task) = state.output_tasks.iter_mut().find(|task| task.id == id) {
        task.append(line);
        state.generation += 1;
    }
}

fn fail_task(state: &mut Snapshot, id: u64, error: String) {
    if let Some(task) = state.output_tasks.iter_mut().find(|task| task.id == id) {
        task_failed(task, error);
    }
    prune_task_history(state, id);
    state.generation += 1;
}

fn task_failed(task: &mut OutputTask, error: String) {
    task.status = if task.refresh.is_some() {
        TaskStatus::RefreshFailed
    } else {
        TaskStatus::Failed
    };
    for line in error.lines() {
        task.append(line);
    }
}

fn prune_task_history(state: &mut Snapshot, latest: u64) {
    const KEEP_FINISHED: usize = 12;
    let completed = state.output_tasks.iter().filter(|task| !task.status.active()).count();
    if completed > KEEP_FINISHED
        && let Some(index) = state.output_tasks.iter().position(|task| {
            task.id != latest && !task.status.active()
        })
    {
        state.output_tasks.remove(index);
    }
}

fn replace_services(snapshot: &mut Snapshot, services: Vec<Service>, backend: ServiceBackend) -> bool {
    replace_services_matching(snapshot, services, |service| service.backend == backend)
}

fn replace_services_matching(
    snapshot: &mut Snapshot,
    services: Vec<Service>,
    belongs_to_group: impl Fn(&Service) -> bool,
) -> bool {
    if services.iter().eq(snapshot.services.iter().filter(|service| belongs_to_group(service))) {
        return false;
    }
    let changed_paths: Vec<_> = snapshot.services.iter().filter(|service| {
        belongs_to_group(service) && !services.iter().any(|current| {
            current.formula == service.formula && current.loaded == service.loaded
                && current.manageable == service.manageable
        })
    }).map(|service| service.formula.clone()).collect();
    for formula in changed_paths {
        snapshot.invalidate_paths(&formula);
    }
    snapshot
        .services
        .retain(|service| !belongs_to_group(service));
    snapshot.services.extend(services);
    true
}

fn execute(
    action: Action,
    brew: &mut Option<Brew>,
    snapshot: &mut Snapshot,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    match action {
        Action::Refresh => {
            snapshot.service_paths = None;
            snapshot.cached_service_paths.clear();
            snapshot.service_report = None;
            let settings = load_settings();
            if let Ok(settings) = &settings {
                snapshot.settings = settings.clone();
            }
            snapshot.runtime_details = None;
            let local = scan_local(brew, snapshot);
            if let Some(brew) = brew.as_ref() {
                refresh_brew_catalog(brew, snapshot, true);
            }
            local?;
            settings?;
        }
        Action::OpenHomebrewInstaller => {
            if Brew::detect()?.is_none() {
                setup::install_homebrew_in_terminal(proxy(&snapshot.settings), log)?;
            }
            snapshot.notice = Some("Homebrew 安装步骤已完成".into());
            RefreshQuery::BrewSetup.run(brew, snapshot)?;
        }
        Action::UpdateHomebrew => {
            brew.as_ref().context("请先检测 Homebrew")?.update(proxy(&snapshot.settings), log)?;
            snapshot.notice = Some("Homebrew 更新完成".into());
            RefreshQuery::BrewUpdate.run(brew, snapshot)?;
        }
        Action::OpenBrewConfig => {
            let brew = brew.as_ref().context("请先检测 Homebrew")?;
            config::open_or_create(&brew_preferences::config_path(brew.prefix())?)?;
        }
        Action::SaveBrewPreferences(preferences) => {
            let brew = brew.as_ref().context("请先检测 Homebrew")?;
            let path = brew_preferences::config_path(brew.prefix())?;
            let preferences = preferences.save(&path)?;
            let details = snapshot.brew_details.get_or_insert_with(|| BrewDetails::read(brew));
            details.config = Ok(BrewConfig {
                path,
                preferences: Ok(preferences),
            });
            snapshot.notice = Some("已保存 Homebrew 配置".into());
            refresh_brew_catalog(brew, snapshot, true);
        }
        Action::OpenTerminalInstall(install) => {
            let brew = brew.as_ref().context("请先检测 Homebrew")?;
            match install {
                TerminalInstall::PackageChange(formula, change) => {
                    change_brew_package(brew, snapshot, &formula, change, true, log)?;
                }
                install => {
                    let (formula, developer_tool) = match &install {
                        TerminalInstall::Formula(formula) => (formula.as_str(), false),
                        TerminalInstall::DeveloperTool(tool) => (tool.formula(), true),
                        TerminalInstall::PackageChange(..) => unreachable!(),
                    };
                    brew.install_in_terminal(formula, proxy(&snapshot.settings), developer_tool, log)?;
                    snapshot.notice = Some(format!("已在终端安装 {formula}"));
                    refresh_brew(brew, snapshot).context(RefreshQuery::BrewInventory)?;
                }
            }
        }
        Action::InstallCommandLineTools(_) => {
            let mut command = Command::new("/usr/bin/xcode-select");
            command.arg("--install");
            crate::command::output(command, "打开 Xcode 命令行工具安装", None)?;
            snapshot.notice = Some("请在 macOS 提示中完成安装，再重试 PHP 扩展安装。".into());
        }
        Action::InstallMiseManager => {
            if Mise::detect().is_none() {
                setup::install_mise(proxy(&snapshot.settings), log)?;
            }
            snapshot.notice = Some("Mise 安装步骤已完成".into());
            RefreshQuery::MiseSetup.run(brew, snapshot)?;
        }
        Action::OpenMiseGlobalConfig => {
            let mise = snapshot.mise.as_ref().context("请先检测 Mise")?;
            config::open_or_create(&mise.global_config_path())?;
        }
        Action::ToggleMiseConfigs => {
            let mise = snapshot.mise.as_ref().context("请先检测 Mise")?;
            if snapshot.mise_configs.take().is_none() {
                snapshot.mise_configs = Some(mise.loaded_configs()?);
            }
        }
        Action::OpenMiseConfig(path) => {
            ensure!(
                snapshot.mise_configs.as_ref().is_some_and(|paths| paths.contains(&path)),
                "请重新读取 Mise 配置列表"
            );
            config::open(&path)?;
        }
        Action::Install(formula) => {
            let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
            ensure!(
                snapshot
                    .formulae
                    .iter()
                    .any(|entry| entry.name == formula && entry.installable),
                "该公式目前不可安装，请刷新版本列表"
            );
            brew.install(&formula, proxy(&snapshot.settings), log)?;
            snapshot.notice = Some(format!("已安装 {formula}"));
            refresh_brew(brew, snapshot).context(RefreshQuery::BrewInventory)?;
        }
        Action::InstallMise(environment, version) => {
            let mise = snapshot.mise.as_ref().context("请先刷新以检测 Mise")?;
            mise.install(environment, &version, proxy(&snapshot.settings), log)?;
            snapshot.notice = Some(format!("Mise 已安装 {} {version}", environment.label()));
            refresh_mise(snapshot).context(RefreshQuery::MiseInventory)?;
        }
        Action::ChangeBrew(formula, change) => {
            let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
            change_brew_package(brew, snapshot, &formula, change, false, log)?;
        }
        Action::UninstallMise(environment, name, version) => {
            if environment == Environment::Elasticsearch {
                let tools = snapshot.mise_tools_for_operation(environment)?;
                let service = elasticsearch_instance(&tools, &format!("elasticsearch@{version}"))?
                    .service(&launch_domain()?);
                ensure!(
                    service.can_uninstall(),
                    "请确认 Elasticsearch 已停止并关闭登录启动后再卸载"
                );
            }
            let mise = snapshot.mise.as_ref().context("请先刷新以检测 Mise")?;
            mise.uninstall(environment, &name, &version, log)?;
            snapshot.notice = Some(format!("Mise 已卸载 {name} {version}"));
            refresh_mise(snapshot).context(RefreshQuery::MiseInventory)?;
        }
        Action::CheckTerminal(..) | Action::ReadCurrentJava => (),
        Action::DiagnoseService(formula) => {
            snapshot.service_report = None;
            let service = snapshot.service(&formula).context("未找到该服务，请刷新")?;
            let (paths, root, plist) = match service.backend {
                ServiceBackend::MiseLaunchd => {
                    let instance = elasticsearch_instance(&snapshot.mise_tools, &formula)?;
                    (instance.paths(), Some(instance.root().to_owned()), None)
                }
                ServiceBackend::Homebrew => {
                    let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
                    let paths = brew_service_paths(brew, snapshot, &formula)?;
                    if snapshot.settings.managed_services.contains(&formula) {
                        (paths, Some(managed_instance(&formula)?.root().to_owned()), None)
                    } else {
                        (paths, None, brew.service_plist(&formula).ok())
                    }
                }
            };
            let report = diagnostics::service(service.environment, &paths, root.as_deref(), plist.as_deref())?;
            snapshot.service_report = Some((formula, report));
        }
        Action::CheckUpdate => (),
        Action::CreateSite { nginx, php, root, port } => {
            ensure!(
                Environment::from_formula(&nginx) == Some(Environment::Nginx)
                    && Environment::from_formula(&php) == Some(Environment::Php),
                "请选择已安装的 Nginx 和 PHP-FPM"
            );
            let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
            let nginx_paths = brew_service_paths(brew, snapshot, &nginx)?;
            let php_paths = brew_service_paths(brew, snapshot, &php)?;
            let nginx_config = nginx_paths.config.as_deref().context("找不到 Nginx 主配置")?;
            let php_config = php_paths.config.as_deref().context("找不到 PHP-FPM 配置")?;
            let nginx_prefix = snapshot.settings.managed_services.contains(&nginx)
                .then(|| managed_instance(&nginx))
                .transpose()?;
            let site = sites::create(
                &nginx_paths.executable, nginx_config, nginx_prefix.as_ref().map(Instance::root),
                php_config, &root, port,
            )?;
            snapshot.notice = Some(format!("已生成 {} 的站点配置", site.url));
            snapshot.site = Some(site);
        }
        Action::OpenSite => {
            let site = snapshot.site.as_ref().context("请先生成站点配置")?;
            config::open(&site.config)?;
        }
        Action::InspectRuntime(target) => inspect_runtime(brew.as_ref(), snapshot, target)?,
        Action::PhpExtension(formula, extension, operation) => {
            let brew = brew.as_ref().context("请先检测 Homebrew")?;
            let previous_config = php_extension_config(snapshot, &formula, extension).map(Path::to_owned);
            let result = php_extensions::apply(
                brew,
                &formula,
                extension,
                operation,
                proxy(&snapshot.settings),
                log,
            );
            let inspection = inspect_runtime(
                Some(brew),
                snapshot,
                RuntimeTarget::Php(formula.clone()),
            );
            result?;
            snapshot.notice = Some(format!("已更新 {} 扩展", extension.label()));
            inspection.context(RefreshQuery::Runtime(RuntimeTarget::Php(formula.clone())))?;
            let config = php_extension_config(snapshot, &formula, extension).or(previous_config.as_deref());
            let fpm_running = snapshot.services.iter().any(|service| {
                service.formula == formula && service.running
            });
            let suffix = if fpm_running && let Some(config) = config {
                let ini = if snapshot.settings.managed_services.contains(&formula) {
                    managed_instance(&formula).ok().map(|instance| instance.root().join("php.ini"))
                } else {
                    None
                };
                match php_extensions::fpm_uses_config(brew, &formula, config, ini.as_deref()) {
                    Ok(true) => "。重启 PHP-FPM 后生效",
                    Ok(false) => "",
                    Err(_) => "。若 PHP-FPM 使用此配置，请重启服务",
                }
            } else {
                ""
            };
            snapshot.notice = Some(format!("已更新 {} 扩展{suffix}", extension.label()));
        }
        Action::OpenPhpExtensionConfig(formula, extension) => {
            let brew = brew.as_ref().context("请先检测 Homebrew")?;
            let path = php_extensions::inspect(brew, &formula)?
                .into_iter()
                .find(|entry| entry.extension == extension)
                .and_then(|entry| entry.config)
                .context("找不到此扩展的配置文件")?;
            config::open(&path)?;
        }
        Action::NpmChange(target, package, install) => {
            let result = language_tools::npm_change(
                brew.as_ref(),
                snapshot.mise.as_ref(),
                &snapshot.mise_tools,
                &target,
                &package,
                install,
                proxy(&snapshot.settings),
                log,
            );
            let target = RuntimeTarget::Npm(target);
            let inspection = inspect_runtime(brew.as_ref(), snapshot, target.clone());
            result?;
            let verb = if install { "已安装" } else { "已卸载" };
            snapshot.notice = Some(format!("{verb} {package}"));
            inspection.context(RefreshQuery::Runtime(target))?;
        }
        Action::InstallRustComponent(target, component) => {
            let result = language_tools::install_rust_component(
                brew.as_ref(),
                snapshot.mise.as_ref(),
                &snapshot.mise_tools,
                &target,
                component,
                proxy(&snapshot.settings),
                log,
            );
            let target = RuntimeTarget::Rust(target);
            let inspection = inspect_runtime(brew.as_ref(), snapshot, target.clone());
            result?;
            snapshot.notice = Some(format!("已安装 {}", component.label()));
            inspection.context(RefreshQuery::Runtime(target))?;
        }
        Action::InstallDeveloperTool(tool) => {
            let brew = brew.as_ref().context("请先检测 Homebrew")?;
            brew.install_developer_tool(tool.formula(), proxy(&snapshot.settings), log)?;
            snapshot.notice = Some(format!("已安装 {}", tool.label()));
            refresh_brew(brew, snapshot).context(RefreshQuery::BrewInventory)?;
        }
        Action::SetMiseGlobal(environment, name, version) => {
            let mise = snapshot.mise.as_ref().context("请先刷新以检测 Mise")?;
            mise.set_global(environment, &name, &version)?;
            snapshot.mise_configs = None;
            refresh_mise(snapshot).context("全局版本已设定，刷新 Mise 环境时出错")?;
            snapshot.notice = Some(format!(
                "已将 {} {version} 设为 Mise 全局版本",
                environment.label()
            ));
        }
        Action::SetDefault(formula) => {
            let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
            let env = Environment::from_formula(&formula).context("无法识别运行环境")?;
            let bin = brew.bin_path(&formula)?;
            snapshot.settings =
                SettingsStore::discover()?.set_default(env.key(), &formula, &bin)?;
            snapshot.notice = Some(format!("已设为默认版本，在新开的 zsh 终端使用 {formula}"));
        }
        Action::ClearDefault(family) => {
            snapshot.settings = SettingsStore::discover()?.clear_default(&family)?;
            snapshot.notice = Some("已清除默认版本，新开的 zsh 将使用原有 PATH。".into());
        }
        Action::Service(formula, action) => {
            let service = snapshot.service(&formula).context("未找到该服务，请刷新")?;
            let (backend, environment) = (service.backend, service.environment);
            snapshot.invalidate_paths(&formula);
            match backend {
                ServiceBackend::MiseLaunchd => {
                    let tools = snapshot.mise_tools_for_operation(environment)?;
                    let instance = elasticsearch_instance(&tools, &formula)?;
                    let operation = instance.operate(&launch_domain()?, action);
                    let services = elasticsearch_services(&tools);
                    replace_services(snapshot, services, backend);
                    operation?;
                }
                ServiceBackend::Homebrew => {
                    let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
                    let instance = snapshot.settings.managed_services.contains(&formula)
                        .then(|| managed_instance(&formula)).transpose()?;
                    let operation = brew.service_action(&formula, action, instance.as_ref());
                    let refreshed = brew.services().map(|services| replace_services(snapshot, services, backend));
                    operation?;
                    refreshed?;
                }
            }
            snapshot.notice = Some(format!("已更新 {formula} 服务"));
        }
        Action::OpenConfig(formula, ConfigKind::Service)
            if snapshot.service(&formula).is_some_and(|service| service.backend == ServiceBackend::MiseLaunchd) =>
        {
            config::open(&elasticsearch_instance(&snapshot.mise_tools, &formula)?.config_file()?)?;
        }
        Action::OpenConfig(formula, kind) => {
            let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
            let path = if !matches!(kind, ConfigKind::PhpIni)
                && snapshot.settings.managed_services.contains(&formula)
            {
                let instance = managed_instance(&formula)?;
                match kind {
                    ConfigKind::FpmIni => instance.prepare_php_ini(&brew.opt_path(&formula)?)?,
                    _ => instance.config(),
                }
            } else {
                brew.config_path(&formula, kind)?
            };
            config::open(&path)?;
        }
        Action::ShowPaths(formula) => {
            let service = snapshot.service(&formula).context("未找到该服务，请刷新")?;
            let paths = match service.backend {
                ServiceBackend::MiseLaunchd => elasticsearch_instance(&snapshot.mise_tools, &formula)?.paths(),
                ServiceBackend::Homebrew => {
                    let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
                    brew_service_paths(brew, snapshot, &formula)?
                }
            };
            let key = snapshot.paths_key(&formula);
            snapshot.cached_service_paths.insert(key, CachedServicePaths {
                config_modified: CachedServicePaths::config_modified(&paths),
                paths: paths.clone(),
            });
            snapshot.service_paths = Some((formula, paths));
        }
        Action::SetManaged(formula, enabled) => {
            let brew = brew.as_ref().context("请先刷新以检测 Homebrew")?;
            let store = SettingsStore::discover()?;
            let instance = Instance::new(&store.service_directory(), &formula)?;
            brew.prepare_instance(&formula, enabled.then_some(&instance))?;
            snapshot.settings = store.set_service_managed(&formula, enabled)?;
            snapshot.invalidate_paths(&formula);
            snapshot.notice = Some(if enabled {
                format!("{formula} 启动时将使用独立实例的配置和目录")
            } else {
                format!("{formula} 已恢复使用 Homebrew 配置，独立实例的文件已保留")
            });
        }
        Action::OpenInstance(formula) => {
            config::open_directory(managed_instance(&formula)?.root())?;
        }
        Action::OpenCredentials(formula) => {
            let path = managed_instance(&formula)?
                .credentials()
                .context("该服务没有初始凭据")?;
            config::open(&path)?;
        }
        Action::EditShellFile(file) => {
            config::open(&SettingsStore::discover()?.prepare_shell_file(file)?)?;
        }
        Action::SetProxy(address) => {
            snapshot.settings = SettingsStore::discover()?.set_proxy(&address)?;
            snapshot.notice = Some("已保存安装代理".into());
            if let Some(brew) = brew.as_ref() {
                refresh_brew_catalog(brew, snapshot, true);
            }
        }
        Action::AppLogin(enabled) => {
            let operation = login::set_enabled(enabled);
            snapshot.app_login = login::status();
            operation?;
            snapshot.notice = Some(
                if enabled {
                    "已开启登陆时启动"
                } else {
                    "已关闭登陆时启动"
                }
                .into(),
            );
        }
        Action::SetQuietLogin(enabled) => {
            snapshot.settings = SettingsStore::discover()?.set_quiet_login(enabled)?;
        }
    }
    Ok(())
}

fn change_brew_package(
    brew: &Brew,
    snapshot: &mut Snapshot,
    formula: &str,
    change: PackageChange,
    in_terminal: bool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    if change == PackageChange::Uninstall {
        ensure!(
            !load_settings()?.defaults.values().any(|selected| selected == formula),
            "请先清除或切换 {formula} 的终端默认版本"
        );
    }
    let before = snapshot.formulae.iter().find(|entry| entry.name == formula)
        .map(|entry| entry.installed_versions.clone());
    let result = brew.change_package(formula, change, proxy(&snapshot.settings), in_terminal, log);
    let refreshed = refresh_brew(brew, snapshot);
    if refreshed.is_ok() {
        let changed = snapshot.formulae.iter().find(|entry| entry.name == formula)
            .map(|entry| &entry.installed_versions) != before.as_ref();
        snapshot.notice = Some(if result.is_err() {
            "已重新读取 Homebrew 安装列表".into()
        } else if change == PackageChange::Upgrade && !changed {
            format!("{formula} 的已安装版本未变化，请查看 Homebrew 输出")
        } else {
            format!("已{} {formula}", change.label())
        });
    }
    result?;
    if refreshed.is_err() {
        snapshot.notice = Some(format!("{formula} 的{}命令已完成", change.label()));
    }
    refreshed.context(RefreshQuery::BrewInventory)
}

fn php_extension_config<'a>(
    snapshot: &'a Snapshot,
    formula: &str,
    extension: Extension,
) -> Option<&'a Path> {
    let (target, details) = snapshot.runtime_details.as_ref()?;
    if !matches!(target, RuntimeTarget::Php(selected) if selected == formula) {
        return None;
    }
    let RuntimeDetails::Php(entries) = details else {
        return None;
    };
    entries
        .iter()
        .find(|entry| entry.extension == extension)?
        .config
        .as_deref()
}

fn inspect_runtime(brew: Option<&Brew>, snapshot: &mut Snapshot, target: RuntimeTarget) -> Result<()> {
    snapshot.runtime_details = None;
    let details = match &target {
        RuntimeTarget::Php(formula) => RuntimeDetails::Php(php_extensions::inspect(
            brew.context("请先检测 Homebrew")?,
            formula,
        )?),
        RuntimeTarget::Npm(tool) => RuntimeDetails::Npm(language_tools::npm_info(
            brew,
            snapshot.mise.as_ref(),
            &snapshot.mise_tools,
            tool,
        )?),
        RuntimeTarget::Rust(tool) => RuntimeDetails::Rust(language_tools::rust_components(
            brew,
            snapshot.mise.as_ref(),
            &snapshot.mise_tools,
            tool,
        )?),
    };
    snapshot.runtime_details = Some((target, details));
    Ok(())
}

fn managed_instance(formula: &str) -> Result<Instance> {
    Instance::new(&SettingsStore::discover()?.service_directory(), formula)
}

fn brew_service_paths(brew: &Brew, snapshot: &Snapshot, formula: &str) -> Result<ServicePaths> {
    let opt = snapshot.formulae.iter().find(|entry| entry.name == formula)
        .and_then(|entry| entry.install_path.as_ref())
        .filter(|path| path.is_absolute() && path.is_dir())
        .map_or_else(|| brew.opt_path(formula), |path| Ok(path.clone()))?;
    if snapshot.settings.managed_services.contains(formula) {
        Ok(managed_instance(formula)?.paths(&opt))
    } else {
        brew.service_paths_at(formula, &opt)
    }
}

fn load_settings() -> Result<Settings> {
    SettingsStore::discover()?.load()
}

fn proxy(settings: &Settings) -> Option<&str> {
    (!settings.proxy.is_empty()).then_some(settings.proxy.as_str())
}

fn scan_local(brew: &mut Option<Brew>, snapshot: &mut Snapshot) -> Result<()> {
    snapshot.app_login = login::status();
    snapshot.managers_checked = true;
    let mise = Mise::detect();
    if snapshot.mise.as_ref().map(Mise::path) != mise.as_ref().map(Mise::path) {
        snapshot.mise_tools.clear();
        replace_services(snapshot, Vec::new(), ServiceBackend::MiseLaunchd);
    }
    snapshot.mise = mise;
    snapshot.mise_version = snapshot.mise.as_ref().and_then(|mise| mise.version().ok());
    snapshot.mise_configs = None;
    let mise_result = refresh_mise(snapshot).context("无法读取 Mise 已安装版本");

    snapshot.brew_detection_failed = false;
    snapshot.brew_details = None;
    let brew_result = match Brew::detect() {
        Ok(Some(detected)) => {
            if !brew.as_ref().is_some_and(|current| current.same_installation(&detected)) {
                if snapshot.brew_path.as_deref().is_some_and(|path| path != detected.path()) {
                    snapshot.replace_brew_formulae(Vec::new());
                    snapshot.developer_tools.clear();
                    replace_services(snapshot, Vec::new(), ServiceBackend::Homebrew);
                }
                *brew = Some(detected.clone());
            }
            snapshot.brew_path = Some(detected.path().to_owned());
            let brew = brew.as_ref().unwrap_or(&detected);
            snapshot.brew_details = Some(BrewDetails::read(brew));
            refresh_brew(brew, snapshot)
        }
        Ok(None) => {
            *brew = None;
            snapshot.brew_path = None;
            snapshot.brew_inventory_error = None;
            snapshot.replace_brew_formulae(Vec::new());
            snapshot.developer_tools.clear();
            replace_services(snapshot, Vec::new(), ServiceBackend::Homebrew);
            Ok(())
        }
        Err(error) => {
            *brew = None;
            snapshot.brew_detection_failed = true;
            snapshot.brew_inventory_error = Some(format!("{error:#}"));
            Err(error)
        }
    };
    match (brew_result, mise_result) {
        (Err(brew), Err(mise)) => Err(anyhow::anyhow!("{brew:#}\n{mise:#}")),
        (brew, mise) => brew.and(mise),
    }
}

fn refresh_brew(brew: &Brew, snapshot: &mut Snapshot) -> Result<()> {
    let result = brew.inventory();
    snapshot.brew_inventory_error = result.as_ref().err().map(|error| format!("{error:#}"));
    let inventory = result?;
    snapshot.replace_brew_formulae(inventory.formulae);
    snapshot.developer_tools = inventory.developer_tools;
    match brew.services().context("无法读取 Homebrew 服务状态") {
        Ok(services) => { replace_services(snapshot, services, ServiceBackend::Homebrew); }
        Err(error) => { snapshot.error = Some(format!("{error:#}")); }
    }
    Ok(())
}

fn refresh_brew_catalog(brew: &Brew, snapshot: &mut Snapshot, force: bool) {
    let update = brew.fetch_catalog(proxy(&snapshot.settings), force);
    if let Some(warning) = brew.apply_catalog(update, &mut snapshot.formulae) {
        snapshot.error.get_or_insert(warning);
    }
}

fn refresh_mise(snapshot: &mut Snapshot) -> Result<()> {
    let result = snapshot.mise.as_ref().map(Mise::installed).transpose();
    snapshot.mise_inventory_errors.clear();
    if let Err(error) = &result {
        snapshot.mark_mise_inventory_stale(format!("{error:#}"));
    }
    snapshot.mise_tools = result?.unwrap_or_default();
    let services = elasticsearch_services(&snapshot.mise_tools);
    replace_services(snapshot, services, ServiceBackend::MiseLaunchd);
    Ok(())
}

fn elasticsearch_services(tools: &[MiseTool]) -> Vec<Service> {
    if !tools
        .iter()
        .any(|tool| tool.environment == Environment::Elasticsearch)
    {
        return Vec::new();
    }
    let Ok(store) = SettingsStore::discover() else {
        return Vec::new();
    };
    let Ok(domain) = launch_domain() else {
        return Vec::new();
    };
    let directory = store.service_directory();
    let mut versions = std::collections::BTreeSet::new();
    tools
        .iter()
        .filter_map(|tool| Elasticsearch::new(&directory, tool))
        .filter(|instance| versions.insert(instance.service_name()))
        .map(|instance| instance.service(&domain))
        .collect()
}

fn elasticsearch_instance(tools: &[MiseTool], formula: &str) -> Result<Elasticsearch> {
    let directory = SettingsStore::discover()?.service_directory();
    tools.iter()
        .filter_map(|tool| Elasticsearch::new(&directory, tool))
        .find(|instance| instance.service_name() == formula)
        .context("找不到由 Mise 安装的 Elasticsearch 版本")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_mise_scan_preserves_its_cache_while_brew_updates() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mise = Mise::for_test(directory.path());
        let mut saved = SavedInventory {
            brew: None,
            mise: Some(SavedMiseInventory {
                path: mise.path().to_owned(),
                tools: vec![MiseTool {
                    name: "node".into(), environment: Environment::Node, version: "22".into(),
                    install_path: None, executable: None, cargo_executable: None, active: Some(true),
                }],
            }),
        };
        let mut snapshot = Snapshot {
            brew_path: Some(directory.path().join("brew")),
            mise: Some(mise),
            ..Snapshot::default()
        };
        saved.restore(&mut snapshot);
        snapshot.developer_tools.insert("ruff".into(), "1".into());
        assert!(refresh_mise(&mut snapshot).is_err());
        assert_eq!(snapshot.mise_tools[0].version, "22");
        assert!(snapshot.mise_inventory_errors.contains_key(&Environment::Node));
        saved.update(&snapshot);
        assert_eq!(saved.brew.as_ref().unwrap().developer_tools.get("ruff").unwrap(), "1");
        assert_eq!(saved.mise.as_ref().unwrap().tools[0].version, "22");

        snapshot.mise = None;
        refresh_mise(&mut snapshot)?;
        saved.update(&snapshot);
        assert!(snapshot.mise_inventory_errors.is_empty());
        assert!(snapshot.mise_tools.is_empty());
        assert!(saved.mise.is_none());
        Ok(())
    }

    #[test]
    fn failed_install_refresh_keeps_newer_brew_inventory() {
        let mut live = Snapshot::default();
        live.developer_tools.insert("ruff".into(), "2".into());
        let mut old = Snapshot {
            notice: Some("已更新 Homebrew".into()),
            ..Snapshot::default()
        };
        old.developer_tools.insert("ruff".into(), "1".into());
        let job = FinishedInstall {
            id: 1,
            action: Action::UpdateHomebrew,
            brew: None,
            snapshot: old,
            result: Err(anyhow::anyhow!("brew list failed").context(RefreshQuery::BrewInventory)),
        };
        assert!(!finish_install(&mut live, &mut None, job));
        assert_eq!(live.developer_tools.get("ruff").unwrap(), "2");
        assert!(live.brew_inventory_error.is_some());
    }

    #[test]
    fn service_operations_and_refresh_use_the_service_backend() {
        let mut snapshot = Snapshot::default();
        let local = Service {
            formula: "redis-local".into(),
            environment: Environment::Redis,
            backend: ServiceBackend::MiseLaunchd,
            running: false,
            loaded: true,
            auto_start: false,
            manageable: true,
            status: "none".into(),
        };
        let brew = Service {
            formula: "redis".into(), backend: ServiceBackend::Homebrew, ..local.clone()
        };
        snapshot.services = vec![brew.clone(), local.clone()];
        let action = |service: &Service, operation| Action::Service(service.formula.clone(), operation);
        assert_eq!(local.toggle_action(), ServiceAction::Stop);
        assert!(snapshot.action_available(&action(&local, ServiceAction::Stop)));
        assert!(!snapshot.action_available(&action(&local, ServiceAction::Restart)));
        assert!(!snapshot.action_available(&action(&local, ServiceAction::SetAutoStart(true))));
        assert!(snapshot.action_available(&action(&brew, ServiceAction::SetAutoStart(true))));
        assert!(matches!(action(&local, ServiceAction::Stop).foreground_scope(&snapshot),
            Some(InstallScope::Mise(Environment::Redis))));
        replace_services(&mut snapshot, Vec::new(), ServiceBackend::Homebrew);
        assert_eq!(snapshot.services, vec![local]);
    }

    #[test]
    fn reopening_a_cached_path_does_not_submit_a_command() {
        let mut snapshot = Snapshot::default();
        for formula in ["mysql", "redis"] {
            snapshot.cached_service_paths.insert((formula.into(), false), CachedServicePaths {
                paths: ServicePaths {
                    executable: PathBuf::from(format!("/opt/homebrew/bin/{formula}")),
                    config: None,
                    data: None,
                    php_ini: None,
                    initial_credentials: false,
                },
                config_modified: None,
            });
        }
        let (sender, receiver) = mpsc::channel();
        let controller = Controller { state: Arc::new(Mutex::new(snapshot)), sender };
        for formula in ["mysql", "redis", "mysql"] {
            controller.submit(Action::ShowPaths(formula.into()));
            let snapshot = controller.snapshot();
            assert_eq!(snapshot.service_paths.as_ref().map(|(shown, _)| shown.as_str()), Some(formula));
        }
        controller.submit(Action::ShowPaths("mysql".into()));
        assert!(controller.snapshot().service_paths.is_none());
        assert!(matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)));

        controller.state.lock().unwrap().invalidate_paths("mysql");
        controller.submit(Action::ShowPaths("mysql".into()));
        assert!(matches!(receiver.try_recv(), Ok(Request::Action(Action::ShowPaths(_)))));
    }

    #[test]
    fn retry_after_mise_uninstall_only_reads_the_inventory() -> Result<()> {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let mise = Mise::for_test(directory.path());
        fs::write(mise.path(), r#"#!/bin/sh
echo "$*" >> calls
case "$*" in
    'ls --prunable --json') echo '{"go":[{"version":"1.24"}]}' ;;
    'ls --global --installed --json') echo '{}' ;;
    'uninstall go@1.24') exit 0 ;;
    'ls --installed --json') test -e allow-inventory && echo '{}' ;;
    *) exit 2 ;;
esac
"#)?;
        fs::set_permissions(mise.path(), fs::Permissions::from_mode(0o700))?;
        let (sender, receiver) = mpsc::channel();
        let controller = Controller {
            state: Arc::new(Mutex::new(Snapshot { mise: Some(mise), ..Snapshot::default() })),
            sender,
        };
        controller.submit(Action::UninstallMise(Environment::Go, "go".into(), "1.24".into()));
        let id = controller.snapshot().output_tasks[0].id;
        for attempt in 0..3 {
            if attempt == 2 {
                fs::write(directory.path().join("allow-inventory"), "")?;
            }
            if attempt > 0 {
                controller.retry_task(id);
            }
            let Request::QueueInstall(job) = receiver.try_recv()? else {
                panic!("expected a queued job");
            };
            start_installs(
                &controller.state,
                &None,
                &mut VecDeque::from([job]),
                &mut BTreeMap::new(),
                &mut false,
            );
            let Request::FinishInstall(job) = receiver.recv_timeout(Duration::from_secs(5))? else {
                panic!("expected a finished job");
            };
            let mut state = controller.state.lock().unwrap();
            finish_install(&mut state, &mut None, *job);
            let expected = if attempt == 2 {
                TaskStatus::Finished
            } else {
                TaskStatus::RefreshFailed
            };
            assert!(state.output_tasks[0].status == expected);
        }
        let calls = fs::read_to_string(directory.path().join("calls"))?;
        assert_eq!(calls.lines().filter(|line| *line == "uninstall go@1.24").count(), 1);
        assert_eq!(calls.lines().filter(|line| *line == "ls --installed --json").count(), 3);
        Ok(())
    }

    #[test]
    fn install_queue_runs_independent_jobs_within_limit() {
        let (sender, _receiver) = mpsc::channel();
        let pending = |id, action: Action| QueuedInstall {
            id,
            scope: action.install_scope().unwrap(),
            action,
            sender: sender.clone(),
        };
        let queued = VecDeque::from([
            pending(2, Action::Install("redis".into())),
            pending(3, Action::InstallMise(Environment::Node, "22".into())),
            pending(4, Action::InstallMise(Environment::Go, "latest".into())),
        ]);
        let mut running = BTreeMap::from([(1, InstallScope::Brew)]);
        assert_eq!(runnable_install(&queued, &running, 1), None);
        assert_eq!(runnable_install(&queued, &running, 2), Some(1));
        running.insert(5, InstallScope::Mise(Environment::Node));
        assert_eq!(runnable_install(&queued, &running, 2), None);
        assert_eq!(runnable_install(&queued, &running, 3), Some(2));
        running.remove(&1);
        assert_eq!(runnable_install(&queued, &running, 2), Some(0));
    }

    #[test]
    fn preferences_update_immediately_even_during_an_install() {
        let (sender, receiver) = mpsc::channel();
        let controller = Controller {
            state: Arc::new(Mutex::new(Snapshot::default())),
            sender,
        };
        controller.submit(Action::InstallMise(Environment::Node, "22".into()));
        let before = controller.snapshot();
        assert!(controller.snapshot_if_changed(before.generation).is_none());

        controller.set_preference(Preference::Visibility(Environment::Go, false));
        controller.set_preference(Preference::Theme(Theme::Dark));
        controller.set_preference(Preference::Visibility(Environment::Node, false));
        controller.set_preference(Preference::Visibility(Environment::Go, true));
        controller.set_preference(Preference::Theme(Theme::Light));
        let after = controller.snapshot_if_changed(before.generation).unwrap();

        assert!(after.environment_visible(Environment::Go));
        assert!(!after.environment_visible(Environment::Node));
        assert_eq!(after.theme(), Theme::Light);
        assert_eq!(after.busy, before.busy);
        assert_eq!(after.installation_counts(), (0, 1));
        assert_eq!(after.generation, before.generation + 5);
        assert_eq!(receiver.try_iter().count(), 6);
    }
}
