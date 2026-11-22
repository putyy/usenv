use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::brew::{Brew, Environment};
use crate::command;
use crate::settings::{SettingsStore, atomic_write};

const PIE_URL: &str = "https://github.com/php/pie/releases/download/1.5.5/pie.phar";
const PIE_SHA256: &str = "2ca0dff6868f1b53c095625674deef38ac698588e0a25e4321cd834a5b7553bc";
const INI_HEADER: &str = "; usenv php extension v1";
const OPCACHE_INI_HEADER: &str = "; usenv opcache settings v1";
const OPCACHE_ENABLE: &str = "opcache.enable";
const PHP_INFO_PREFIX: &str = "USENV_PHP ";
const PHP_PROBE: &str = r#"
    $loaded = [];
    foreach (array_merge(get_loaded_extensions(), get_loaded_extensions(true)) as $extension) {
        $loaded[strtolower($extension)] = phpversion($extension) ?: null;
    }
    echo "\nUSENV_PHP ", json_encode([
        'version_id' => PHP_VERSION_ID,
        'abi' => php_uname('m') . ':' . basename(PHP_EXTENSION_DIR),
        'compiled_extension_dir' => PHP_EXTENSION_DIR,
        'extension_dir' => ini_get('extension_dir'),
        'openssl_cafile' => ini_get('openssl.cafile') ?: null,
        'openssl_capath' => ini_get('openssl.capath') ?: null,
        'ini' => php_ini_loaded_file() ?: null,
        'scanned' => php_ini_scanned_files() ?: '',
        'loaded' => $loaded,
        'opcache_enabled' => array_key_exists('zend opcache', $loaded)
            ? filter_var(ini_get('opcache.enable'), FILTER_VALIDATE_BOOLEAN)
            : null,
        'opcache_cli' => filter_var(ini_get('opcache.enable_cli'), FILTER_VALIDATE_BOOLEAN),
    ]);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Extension {
    Opcache,
    Swoole,
    Redis,
    Xdebug,
    Apcu,
    Imagick,
    Mongodb,
    Memcached,
    Yaml,
}

impl Extension {
    pub const ALL: [Self; 9] = [
        Self::Opcache,
        Self::Swoole,
        Self::Redis,
        Self::Xdebug,
        Self::Apcu,
        Self::Imagick,
        Self::Mongodb,
        Self::Memcached,
        Self::Yaml,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Opcache => "OPcache",
            Self::Swoole => "Swoole",
            Self::Redis => "Redis（phpredis）",
            Self::Xdebug => "Xdebug",
            Self::Apcu => "APCu",
            Self::Imagick => "Imagick",
            Self::Mongodb => "MongoDB",
            Self::Memcached => "Memcached",
            Self::Yaml => "YAML",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Opcache => "opcache",
            Self::Swoole => "swoole",
            Self::Redis => "redis",
            Self::Xdebug => "xdebug",
            Self::Apcu => "apcu",
            Self::Imagick => "imagick",
            Self::Mongodb => "mongodb",
            Self::Memcached => "memcached",
            Self::Yaml => "yaml",
        }
    }

    pub fn is_bundled(self) -> bool {
        self == Self::Opcache
    }

    fn package(self) -> Option<&'static str> {
        Some(match self {
            Self::Opcache => return None,
            Self::Swoole => "swoole/swoole",
            Self::Redis => "phpredis/phpredis",
            Self::Xdebug => "xdebug/xdebug",
            Self::Apcu => "apcu/apcu",
            Self::Imagick => "imagick/imagick",
            Self::Mongodb => "mongodb/mongodb-extension",
            Self::Memcached => "php-memcached/php-memcached",
            Self::Yaml => "pecl/yaml",
        })
    }

    fn library(self) -> String {
        format!("{}.so", self.key())
    }

    fn directive(self) -> &'static str {
        if matches!(self, Self::Xdebug | Self::Opcache) {
            "zend_extension"
        } else {
            "extension"
        }
    }

    fn brew_dependencies(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Imagick => &[("imagemagick", "--with-imagick=")],
            Self::Memcached => &[
                ("libmemcached", "--with-libmemcached-dir="),
                ("zlib", "--with-zlib-dir="),
            ],
            Self::Yaml => &[("libyaml", "--with-yaml=")],
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionState {
    Missing,
    Enabled,
    Disabled,
    External,
    Broken,
}

impl ExtensionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "未安装",
            Self::Enabled => "已启用",
            Self::Disabled => "已安装，未启用",
            Self::External => "外部管理",
            Self::Broken => "需要修复",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionInfo {
    pub extension: Extension,
    pub state: ExtensionState,
    pub version: Option<String>,
    pub config: Option<PathBuf>,
    pub loaded: bool,
    pub detail: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub enum Operation {
    Install(Option<String>),
    Enable,
    Disable,
    Uninstall,
}

#[derive(Debug)]
pub struct CommandLineToolsRequired;

impl fmt::Display for CommandLineToolsRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("需要安装 Xcode 命令行工具。完成 macOS 安装提示后再试。")
    }
}

impl std::error::Error for CommandLineToolsRequired {}

#[derive(Deserialize)]
struct Runtime {
    version_id: u32,
    abi: String,
    compiled_extension_dir: PathBuf,
    extension_dir: PathBuf,
    openssl_cafile: Option<PathBuf>,
    openssl_capath: Option<PathBuf>,
    ini: Option<PathBuf>,
    scanned: String,
    loaded: BTreeMap<String, Option<String>>,
    opcache_enabled: Option<bool>,
    opcache_cli: bool,
}

struct Target {
    bin: PathBuf,
    runtime: Runtime,
    scan_dir: Option<PathBuf>,
    data_dir: PathBuf,
}

struct ManagedIni {
    source: String,
    version: String,
    abi: String,
    enabled: bool,
}

pub fn inspect(brew: &Brew, formula: &str) -> Result<Vec<ExtensionInfo>> {
    let target = Target::new(brew, formula)?;
    Extension::ALL
        .into_iter()
        .map(|extension| target.info(extension))
        .collect()
}

pub fn apply(
    brew: &Brew,
    formula: &str,
    extension: Extension,
    operation: Operation,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let target = Target::new(brew, formula)?;
    let ini = target
        .ini(extension)
        .context("这个 PHP 版本没有可用的附加 INI 扫描目录")?;
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&target.data_dir)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(target.data_dir.join(".extensions.lock"))?;
    lock.lock()?;

    let mut target = Target::new(brew, formula)?;
    ensure!(
        target.ini(extension).as_deref() == Some(ini.as_path()),
        "PHP 配置路径已改变，请刷新后重试"
    );
    if extension.is_bundled() {
        return set_opcache(&target, &ini, operation);
    }
    let info = target.info(extension)?;
    ensure!(info.state != ExtensionState::External, "请通过现有配置管理这个扩展");
    let module = target.module(extension);
    let managed = read_managed_ini(&ini, extension, &module)?;

    match operation {
        Operation::Install(version) => {
            ensure!(
                matches!(info.state, ExtensionState::Missing | ExtensionState::Broken),
                "这个扩展已由 Usenv 安装"
            );
            install(
                brew,
                &mut target,
                extension,
                version.as_deref(),
                managed.as_ref(),
                proxy,
                log,
            )
        }
        Operation::Enable | Operation::Disable => {
            let enabled = matches!(operation, Operation::Enable);
            ensure!(
                info.state == if enabled { ExtensionState::Disabled } else { ExtensionState::Enabled },
                "这个扩展当前无法切换启用状态"
            );
            let managed = managed.context("找不到 Usenv 扩展配置")?;
            let old = ini_directive(extension, &module, managed.enabled)?;
            let new = ini_directive(extension, &module, enabled)?;
            let updated = managed.source.replacen(&old, &new, 1);
            atomic_write(&ini, updated.as_bytes())?;
            if let Err(error) = verify_loaded(&target, extension, enabled, log) {
                atomic_write(&ini, managed.source.as_bytes())
                    .with_context(|| format!("{error:#}\n无法还原扩展配置"))?;
                return Err(error);
            }
            Ok(())
        }
        Operation::Uninstall => {
            ensure!(
                matches!(info.state, ExtensionState::Enabled | ExtensionState::Disabled | ExtensionState::Broken),
                "这个扩展没有由 Usenv 安装"
            );
            if managed.is_some() {
                fs::remove_file(&ini).with_context(|| format!("无法移除 {}", ini.display()))?;
                let verified = verify_loaded(&target, extension, false, log);
                if let Err(error) = verified {
                    if let Some(previous) = &managed {
                        atomic_write(&ini, previous.source.as_bytes())
                            .with_context(|| format!("{error:#}\n无法还原扩展配置"))?;
                    }
                    return Err(error);
                }
            }
            if let Err(error) = remove_module(&module) {
                if let Some(previous) = &managed {
                    atomic_write(&ini, previous.source.as_bytes())
                        .with_context(|| format!("{error:#}\n无法还原扩展配置"))?;
                }
                return Err(error);
            }
            Ok(())
        }
    }
}

pub fn fpm_uses_config(
    brew: &Brew,
    formula: &str,
    config: &Path,
    instance_ini: Option<&Path>,
) -> Result<bool> {
    ensure!(Environment::from_formula(formula) == Some(Environment::Php), "需要选择 PHP 版本");
    let mut command = Command::new(brew.opt_path(formula)?.join("sbin/php-fpm"));
    command.env_remove("PHP_INI_SCAN_DIR").env_remove("PHPRC");
    let brew_ini = if instance_ini.is_none() {
        brew.service_paths(formula).ok().and_then(|paths| paths.php_ini)
    } else {
        None
    };
    if let Some(ini) = instance_ini.or(brew_ini.as_deref()).filter(|path| path.is_file()) {
        command.arg("-c").arg(ini);
    }
    command.arg("-i");
    let output = command::output(command, "读取 PHP-FPM 扩展扫描目录", None)?;
    let output_scan = output.lines().find_map(|line| {
        line.split_once("=>")
            .filter(|(key, _)| key.trim() == "Scan this dir for additional .ini files")
            .map(|(_, value)| value)
    });
    let Some(directory) = config.parent() else { return Ok(false) };
    Ok(output_scan.into_iter().flat_map(scan_directories).any(|entry| same_path(&entry, directory)))
}

impl Target {
    fn new(brew: &Brew, formula: &str) -> Result<Self> {
        ensure!(Environment::from_formula(formula) == Some(Environment::Php), "需要选择 PHP 版本");
        let bin = brew.bin_path(formula)?;
        let runtime = probe(&bin, None)?;
        let mut php = Command::new(bin.join("php"));
        php.arg("--ini");
        let ini = command::output(php, "读取 PHP 扩展扫描目录", None)?;
        let scan = ini.lines().find_map(|line| line.strip_prefix("Scan for additional .ini files in:"));
        let major = runtime.version_id / 10000;
        let minor = runtime.version_id / 100 % 100;
        let version_label = format!("{major}.{minor}");
        let default_dir = brew.prefix().join("etc/php").join(&version_label).join("conf.d");
        let directories = scan.map(scan_directories).unwrap_or_default();
        let scan_dir = directories
            .iter()
            .find(|path| same_path(path, &default_dir))
            .cloned()
            .or_else(|| directories.into_iter().find(|path| path.is_dir()));
        let store = SettingsStore::discover()?;
        let service_dir = store.service_directory();
        let arch = runtime.abi.split_once(':').map_or("", |(arch, _)| arch);
        ensure!(matches!(arch, "arm64" | "aarch64" | "x86_64"), "无法识别 PHP 架构");
        let data_dir = service_dir
            .parent()
            .context("无法定位 Usenv 数据目录")?
            .join("php")
            .join(version_label)
            .join(arch);
        Ok(Self { bin, runtime, scan_dir, data_dir })
    }

    fn ini(&self, extension: Extension) -> Option<PathBuf> {
        self.scan_dir.as_ref().map(|path| {
            if extension.is_bundled() {
                path.join("zz-usenv-opcache.ini")
            } else {
                path.join(format!("90-usenv-{}.ini", extension.key()))
            }
        })
    }

    fn module(&self, extension: Extension) -> PathBuf {
        self.data_dir.join(extension.library())
    }

    fn info(&self, extension: Extension) -> Result<ExtensionInfo> {
        if extension.is_bundled() {
            return Ok(self.opcache_info());
        }
        let module = self.module(extension);
        let ini_path = self.ini(extension);
        let managed = match ini_path.as_deref() {
            Some(ini) => read_managed_ini(ini, extension, &module)?,
            None => None,
        };
        let live = self.runtime.loaded.get(extension.key());
        let external_config = self.config_files().find(|path| {
            let is_managed = managed.is_some() && ini_path.as_ref().is_some_and(|ini| same_path(ini, path));
            !is_managed && config_loads_extension(path, extension)
        });
        let occupied = managed.is_none() && ini_path.as_ref().is_some_and(|ini| entry_exists(ini));
        let foreign_module = entry_exists(&module) && !module_is_regular(&module);
        let (state, version, config) = if external_config.is_some() || occupied || foreign_module {
            (ExtensionState::External, live.cloned().flatten(), external_config.or(ini_path))
        } else if let Some(managed) = managed {
            let state = if !module_is_regular(&module) || managed.abi != self.runtime.abi {
                ExtensionState::Broken
            } else if managed.enabled {
                if live.is_some() { ExtensionState::Enabled } else { ExtensionState::Broken }
            } else if live.is_some() {
                ExtensionState::External
            } else {
                ExtensionState::Disabled
            };
            (state, live.cloned().flatten().or(Some(managed.version)), ini_path)
        } else if live.is_some() || entry_exists(&self.runtime.extension_dir.join(extension.library())) {
            (ExtensionState::External, live.cloned().flatten(), None)
        } else if entry_exists(&module) {
            (ExtensionState::Broken, None, None)
        } else {
            (ExtensionState::Missing, None, None)
        };
        Ok(ExtensionInfo { extension, state, version, config, loaded: live.is_some(), detail: None })
    }

    fn config_files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.runtime.ini.iter().cloned().chain(
            self.runtime.scanned.split(",\n").map(str::trim).map(PathBuf::from).filter(|path| path.is_absolute()),
        )
    }

    fn opcache_library(&self) -> Option<PathBuf> {
        let root = self.bin.parent()?;
        let extension_dir = &self.runtime.compiled_extension_dir;
        let abi = extension_dir.file_name()?;
        let relative = extension_dir.strip_prefix(root).ok().map(Path::to_owned).or_else(|| {
            extension_dir.strip_prefix(root.canonicalize().ok()?).ok().map(Path::to_owned)
        });
        relative.map(|path| root.join(path))
            .into_iter()
            .chain([root.join("lib/php").join(abi), root.join("lib/php/extensions").join(abi)])
            .map(|path| path.join("opcache.so"))
            .find(|path| path.is_file())
    }

    fn opcache_info(&self) -> ExtensionInfo {
        let extension = Extension::Opcache;
        let enabled = self.runtime.opcache_enabled;
        let own_config = self.ini(extension).filter(|path| entry_exists(path));
        let existing_loader = self.config_files().find(|path| config_loads_extension(path, extension));
        let config = own_config.clone().filter(|path| path.is_file())
            .or_else(|| self.config_files().filter(|path| config_sets_opcache(path)).last())
            .or_else(|| existing_loader.clone())
            .or_else(|| self.runtime.ini.clone());
        let editable = self.scan_dir.is_some()
            && own_config.as_ref().is_none_or(|path| {
                module_is_regular(path) && fs::read_to_string(path)
                    .is_ok_and(|source| source.lines().next() == Some(OPCACHE_INI_HEADER))
            });
        let state = match enabled {
            Some(true) if editable => ExtensionState::Enabled,
            Some(false) if editable => ExtensionState::Disabled,
            Some(_) => ExtensionState::External,
            None if existing_loader.is_some() => ExtensionState::External,
            None if editable && self.runtime.version_id < 80500 && self.opcache_library().is_some() => {
                ExtensionState::Disabled
            }
            None => ExtensionState::Missing,
        };
        let detail = enabled.map(|enabled| if enabled && self.runtime.opcache_cli {
            "CLI 缓存已开启"
        } else {
            "CLI 缓存已关闭"
        });
        ExtensionInfo {
            extension, state,
            version: self.runtime.loaded.get("zend opcache").cloned().flatten(),
            config,
            loaded: enabled.is_some(),
            detail,
        }
    }
}

fn set_opcache(target: &Target, ini: &Path, operation: Operation) -> Result<()> {
    let enable = match operation {
        Operation::Enable => true,
        Operation::Disable => false,
        _ => bail!("OPcache 随 PHP 提供，可在此开启或关闭缓存"),
    };
    let expected = if enable { ExtensionState::Disabled } else { ExtensionState::Enabled };
    ensure!(target.opcache_info().state == expected, "OPcache 当前无法切换，请刷新或检查配置");
    let directory = ini.parent().context("无效的 OPcache 配置路径")?;
    fs::create_dir_all(directory)
        .with_context(|| format!("无法准备 OPcache 配置目录 {}", directory.display()))?;

    let previous = if entry_exists(ini) {
        ensure!(module_is_regular(ini), "OPcache 配置文件已被替换");
        let source = fs::read_to_string(ini)?;
        ensure!(source.lines().next() == Some(OPCACHE_INI_HEADER), "请通过现有文件管理 OPcache");
        Some(source)
    } else {
        None
    };
    let mut updated = previous.clone().unwrap_or_else(|| format!("{OPCACHE_INI_HEADER}\n"));
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    let setting = if enable { "1" } else { "0" };
    let directive = format!("{OPCACHE_ENABLE}={setting}\n");
    if let Some(line) = updated.split_inclusive('\n').find(|line| {
        line.trim_start().split_once('=').is_some_and(|(key, _)| key.trim() == OPCACHE_ENABLE)
    }) {
        updated = updated.replacen(line, &directive, 1);
    } else {
        updated.push_str(&directive);
    }

    let loader = if enable && target.runtime.opcache_enabled.is_none() {
        let library = target.opcache_library().context("找不到当前 PHP 自带的 opcache.so")?;
        let loader = ini.with_file_name("00-usenv-opcache-loader.ini");
        let content = format!("{INI_HEADER}\n{}\n", ini_directive(Extension::Opcache, &library, true)?);
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        temp.write_all(content.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(&loader)
            .with_context(|| format!("无法新建 {}，请检查已有加载配置", loader.display()))?;
        Some(loader)
    } else {
        None
    };
    if let Err(error) = atomic_write(ini, updated.as_bytes()) {
        if let Some(loader) = loader {
            fs::remove_file(loader).with_context(|| format!("{error:#}\n无法还原 OPcache 加载配置"))?;
        }
        return Err(error);
    }
    let verified = probe(&target.bin, None).and_then(|runtime| {
        ensure!(
            runtime.opcache_enabled == Some(enable),
            "PHP 没有应用 OPcache 设置，请检查扫描目录中是否有其他配置覆盖 {OPCACHE_ENABLE}"
        );
        Ok(())
    });
    if let Err(error) = verified {
        let restored = match previous {
            Some(previous) => atomic_write(ini, previous.as_bytes()),
            None => fs::remove_file(ini).map_err(Into::into),
        };
        let loader_restored = loader.map_or(Ok(()), |path| fs::remove_file(path).map_err(Into::into));
        restored.and(loader_restored).with_context(|| format!("{error:#}\n无法还原 OPcache 配置"))?;
        return Err(error);
    }
    Ok(())
}

fn config_sets_opcache(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|content| {
        content.lines().any(|line| line.split_once('=').is_some_and(|(name, _)| {
            name.trim().starts_with("opcache.")
        }))
    })
}

fn install(
    brew: &Brew,
    target: &mut Target,
    extension: Extension,
    version: Option<&str>,
    previous: Option<&ManagedIni>,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let package = package_spec(extension, version)?;
    let ini = target.ini(extension).context("找不到扩展配置目录")?;
    let config_dir = ini.parent().context("扩展配置路径无效")?;
    fs::create_dir_all(config_dir).context("无法准备 PHP 配置目录，请检查 Homebrew 配置权限")?;
    tempfile::NamedTempFile::new_in(config_dir)
        .context("无法写入 PHP 配置目录，请检查 Homebrew 配置权限")?;
    let php_config = target.bin.join("php-config");
    let phpize = target.bin.join("phpize");
    ensure!(php_config.is_file() && phpize.is_file(), "这个 PHP 版本缺少 php-config 或 phpize");
    let mut extension_dir_cmd = Command::new(&php_config);
    extension_dir_cmd.arg("--extension-dir");
    let extension_dir = command::output(extension_dir_cmd, "读取 PHP 编译扩展目录", None)?;
    let extension_dir = Path::new(extension_dir.trim());
    let relative_dir = extension_dir.strip_prefix("/").context("PHP 编译扩展目录不是绝对路径")?;
    ensure!(
        !relative_dir.as_os_str().is_empty()
            && relative_dir.components().all(|part| matches!(part, Component::Normal(_))),
        "PHP 编译扩展目录无效"
    );

    let mut developer_directory = Command::new("/usr/bin/xcode-select");
    developer_directory.arg("-p");
    command::output(developer_directory, "检测 Xcode 开发目录", None)
        .context(CommandLineToolsRequired)?;
    let mut clang = Command::new("/usr/bin/xcrun");
    clang.args(["--find", "clang"]);
    command::output(clang, "检测 Xcode 命令行工具", None).context(CommandLineToolsRequired)?;

    let mut configure_options = prepare_brew_dependencies(brew, extension, proxy, log)?;
    if extension == Extension::Memcached {
        configure_options.push("--enable-memcached".into());
    }
    let work = tempfile::Builder::new().prefix("usenv-php-extension-").tempdir()?;
    let stage = work.path().join("stage");
    let staged_dir = stage.join(relative_dir);
    fs::create_dir_all(&staged_dir)?;
    let build_ini = work.path().join("php.ini");
    // Homebrew's php.ini and php-config can refer to the same extension
    // directory under different paths. PIE and make must use the same path
    // after prepending INSTALL_ROOT.
    let mut build_config = format!("extension_dir={}\n", php_ini_path(extension_dir)?);
    for (setting, value) in [
        ("openssl.cafile", &target.runtime.openssl_cafile),
        ("openssl.capath", &target.runtime.openssl_capath),
    ] {
        if let Some(value) = value {
            build_config.push_str(&format!("{setting}={}\n", php_ini_path(value)?));
        }
    }
    fs::write(&build_ini, build_config)?;
    let pie = download_pie(work.path(), proxy, log)?;
    let binary = if target.runtime.version_id >= 80100 {
        target.bin.join("php")
    } else {
        let latest = brew.prefix().join("opt/php/bin/php");
        ensure!(latest.is_file(), "运行 PIE 需要 PHP 8.1 或更新版本，请先安装一个较新的 PHP");
        latest
    };
    let inherited = env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    let paths = env::join_paths(
        [target.bin.clone(), brew.prefix().join("bin"), brew.prefix().join("sbin")]
            .into_iter()
            .chain(env::split_paths(&inherited)),
    )?;
    let mut build = Command::new(binary);
    build
        .arg(&pie)
        .args(["--no-interaction", "--no-ansi", "-vvv", "install", "--skip-enable-extension"])
        .args(["--auto-install-build-tools", "--auto-install-system-dependencies"])
        .arg("--with-php-config")
        .arg(&php_config)
        .arg("--with-phpize-path")
        .arg(&phpize)
        .arg(package)
        .args(configure_options)
        .current_dir(work.path())
        .env("PATH", paths)
        .env("PHPRC", &build_ini)
        .env("PHP_INI_SCAN_DIR", "")
        .env("PIE_WORKING_DIRECTORY", work.path().join("pie"))
        .env("COMPOSER_HOME", work.path().join("composer"))
        .env("COMPOSER_NO_INTERACTION", "1")
        .env("INSTALL_ROOT", &stage)
        .env("NONINTERACTIVE", "1")
        .env("HOMEBREW_NO_INSTALL_CLEANUP", "1");
    if let Err(error) = command::stream(build, "编译 PHP 扩展", proxy, log) {
        if error.is::<command::NeedsTerminal>() {
            return Err(error).context("请在终端安装输出中列出的 Homebrew 编译依赖，再重试安装扩展");
        }
        return Err(error);
    }

    let staged = staged_dir.join(extension.library());
    ensure!(module_is_regular(&staged), "PIE 没有生成预期的扩展文件 {}", staged.display());
    let checked = probe(&target.bin, Some((extension, &staged)))?;
    let Some(version) = checked.loaded.get(extension.key()).cloned().flatten() else {
        let mut details = Command::new(target.bin.join("php"));
        let mut directive = std::ffi::OsString::from(format!("{}=", extension.directive()));
        directive.push(&staged);
        details.args(["-n", "-d"]).arg(directive).args(["--ri", extension.key()]);
        command::stream(details, "检查编译后的 PHP 扩展", None, log)?;
        bail!("这个 PHP 版本无法加载编译后的扩展");
    };
    let current = probe(&target.bin, None)?;
    ensure!(current.abi == target.runtime.abi, "编译期间 PHP 版本发生变化，请重新安装扩展");
    target.runtime = current;
    ensure!(
        matches!(target.info(extension)?.state, ExtensionState::Missing | ExtensionState::Broken),
        "扩展状态在编译期间发生变化，请刷新后重试"
    );
    let module = target.module(extension);
    let current_ini = read_managed_ini(&ini, extension, &module)?;
    ensure!(
        previous.map(|ini| &ini.source) == current_ini.as_ref().map(|ini| &ini.source),
        "扩展配置已被其他程序更新，请刷新后重试"
    );
    let new_line = ini_directive(extension, &module, true)?;
    let updated = if let Some(previous) = previous {
        replace_metadata(previous, &target.runtime.abi, &version, &new_line, extension, &module)?
    } else {
        format!("{INI_HEADER}\n; abi={}\n; version={version}\n{new_line}\n", target.runtime.abi)
    };
    ensure!(!entry_exists(&ini) || previous.is_some(), "扩展配置已由其他程序创建，请刷新后重试");
    ensure!(!entry_exists(&module) || module_is_regular(&module), "Usenv 扩展文件不是普通文件");
    let old_module = if module_is_regular(&module) {
        let backup = tempfile::NamedTempFile::new_in(&target.data_dir)?;
        fs::copy(&module, backup.path())?;
        Some(backup)
    } else {
        None
    };
    let new_module = tempfile::NamedTempFile::new_in(&target.data_dir)?;
    fs::copy(staged, new_module.path())?;
    new_module.as_file().sync_all()?;
    new_module.persist(&module).context("无法保存 Usenv 扩展文件")?;
    let result = atomic_write(&ini, updated.as_bytes()).and_then(|_| verify_loaded(target, extension, true, log));
    if let Err(error) = result {
        let restored = match old_module {
            Some(backup) => backup.persist(&module).map(|_| ()).context("无法还原扩展文件"),
            None => remove_module(&module),
        };
        let restored_ini = match previous {
            Some(previous) => atomic_write(&ini, previous.source.as_bytes()),
            None => remove_module(&ini),
        };
        restored.and(restored_ini).with_context(|| format!("{error:#}\n无法还原 PHP 扩展状态"))?;
        return Err(error);
    }
    Ok(())
}

fn prepare_brew_dependencies(
    brew: &Brew,
    extension: Extension,
    proxy: Option<&str>,
    log: &(dyn Fn(&str) + Sync),
) -> Result<Vec<OsString>> {
    extension
        .brew_dependencies()
        .iter()
        .map(|&(formula, option)| {
            let prefix = if let Some(prefix) = brew_dependency_prefix(brew, formula) {
                prefix
            } else {
                let terminal = format!(
                    "{} install --formula homebrew/core/{formula}",
                    command::shell_quote(&brew.path().to_string_lossy()),
                );
                brew.install_package(formula, proxy, log)
                    .with_context(|| format!("请在终端执行 {terminal} 后重试安装扩展"))?;
                brew_dependency_prefix(brew, formula)
                    .with_context(|| format!("Homebrew 没有返回 {formula} 的已安装路径"))?
            };
            let mut configure = OsString::from(option);
            configure.push(prefix);
            Ok(configure)
        })
        .collect()
}

fn brew_dependency_prefix(brew: &Brew, formula: &str) -> Option<PathBuf> {
    let mut command = Command::new(brew.path());
    command
        .args(["--prefix", "--installed", formula])
        .env("HOMEBREW_NO_AUTO_UPDATE", "1");
    let output = command::output(command, "读取 Homebrew 编译依赖路径", None).ok()?;
    let prefix = PathBuf::from(output.trim());
    (prefix.is_absolute() && prefix.is_dir()).then_some(prefix)
}

fn download_pie(directory: &Path, proxy: Option<&str>, log: &(dyn Fn(&str) + Sync)) -> Result<PathBuf> {
    let pie = directory.join("pie.phar");
    let mut curl = Command::new("/usr/bin/curl");
    curl.args(["-q", "--proto", "=https", "--proto-redir", "=https", "-fL", "--connect-timeout", "20", "--max-time", "180", "-o"])
        .arg(&pie)
        .arg(PIE_URL);
    command::stream(curl, "下载官方 PIE", proxy, log)?;
    let mut sum = Command::new("/usr/bin/shasum");
    sum.args(["-a", "256"]).arg(&pie);
    let sha = command::output(sum, "校验 PIE 下载文件", None)?;
    ensure!(sha.split_whitespace().next() == Some(PIE_SHA256), "官方 PIE 下载文件校验失败");
    Ok(pie)
}

fn probe(bin: &Path, library: Option<(Extension, &Path)>) -> Result<Runtime> {
    let mut php = Command::new(bin.join("php"));
    if let Some((extension, path)) = library {
        let mut directive = std::ffi::OsString::from(format!("{}=", extension.directive()));
        directive.push(path);
        php.args(["-n", "-d"]).arg(directive);
    }
    php.args(["-r", PHP_PROBE]);
    let output = command::output(php, "检测 PHP 已加载的扩展", None)?;
    let data = output.lines().rev().find_map(|line| line.strip_prefix(PHP_INFO_PREFIX))
        .context("无法识别 PHP 扩展检测结果")?;
    serde_json::from_str(data).context("无法解析 PHP 扩展检测结果")
}

fn verify_loaded(target: &Target, extension: Extension, expected: bool, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let runtime = probe(&target.bin, None)?;
    if runtime.loaded.contains_key(extension.key()) != expected {
        if expected {
            let mut details = Command::new(target.bin.join("php"));
            details.args(["--ri", extension.key()]);
            let _ = command::stream(details, "检查 PHP 扩展加载", None, log);
        }
        bail!("PHP 没有按预期{} {}", if expected { "加载" } else { "停用" }, extension.label());
    }
    if expected {
        log(&format!("已确认 PHP 加载 {}", extension.label()));
    }
    Ok(())
}

fn read_managed_ini(path: &Path, extension: Extension, module: &Path) -> Result<Option<ManagedIni>> {
    if !module_is_regular(path) {
        return Ok(None);
    }
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("无法读取 {}", path.display())),
    };
    let mut lines = source.lines();
    if lines.next() != Some(INI_HEADER) {
        return Ok(None);
    }
    let Some(abi) = lines.next().and_then(|line| line.strip_prefix("; abi=")) else { return Ok(None) };
    let Some(version) = lines.next().and_then(|line| line.strip_prefix("; version=")) else { return Ok(None) };
    let active = ini_directive(extension, module, true)?;
    let inactive = ini_directive(extension, module, false)?;
    let Some(line) = lines.next() else { return Ok(None) };
    if line != active && line != inactive {
        return Ok(None);
    }
    let enabled = line == active;
    let (abi, version) = (abi.to_owned(), version.to_owned());
    Ok(Some(ManagedIni { source, version, abi, enabled }))
}

fn replace_metadata(
    previous: &ManagedIni,
    abi: &str,
    version: &str,
    new_line: &str,
    extension: Extension,
    module: &Path,
) -> Result<String> {
    Ok(previous.source
        .replacen(&format!("; abi={}", previous.abi), &format!("; abi={abi}"), 1)
        .replacen(&format!("; version={}", previous.version), &format!("; version={version}"), 1)
        .replacen(&ini_directive(extension, module, previous.enabled)?, new_line, 1))
}

fn config_loads_extension(path: &Path, extension: Extension) -> bool {
    let Ok(content) = fs::read_to_string(path) else { return false };
    content.lines().any(|line| {
        let Some((key, value)) = line.trim().split_once('=') else { return false };
        if !matches!(key.trim(), "extension" | "zend_extension") {
            return false;
        }
        let value = value.split(';').next().unwrap_or_default().trim().trim_matches(['"', '\'']);
        Path::new(value).file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.eq_ignore_ascii_case(extension.key()) || name.eq_ignore_ascii_case(&extension.library())
        })
    })
}

fn package_spec(extension: Extension, version: Option<&str>) -> Result<String> {
    let package = extension.package().context("这个扩展随 PHP 提供")?;
    let Some(version) = version.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(package.to_owned());
    };
    let version = version.strip_prefix('v').unwrap_or(version);
    ensure!(
        version.len() <= 24 && version.split('.').count() == 3
            && version.split('.').all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())),
        "请输入扩展完整版本号，例如 6.2.4"
    );
    Ok(format!("{package}:{version}"))
}

fn scan_directories(value: &str) -> Vec<PathBuf> {
    value.trim().split(':').map(PathBuf::from).filter(|path| path.is_absolute()).collect()
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right || left.canonicalize().ok().zip(right.canonicalize().ok()).is_some_and(|(a, b)| a == b)
}

fn php_ini_path(path: &Path) -> Result<String> {
    let value = path.to_str().context("PHP 扩展路径必须是 UTF-8 文本")?;
    ensure!(!value.contains(['"', '\\', '\r', '\n', '$']), "PHP 扩展路径包含 INI 不支持的字符");
    Ok(format!("\"{value}\""))
}

fn ini_directive(extension: Extension, module: &Path, enabled: bool) -> Result<String> {
    let prefix = if enabled { "" } else { "; " };
    Ok(format!("{prefix}{}={}", extension.directive(), php_ini_path(module)?))
}

fn entry_exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn module_is_regular(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|metadata| metadata.is_file())
}

fn remove_module(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("无法移除 {}", path.display())),
    }
}
