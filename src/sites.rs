use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::command::shell_quote;
use crate::settings::SettingsStore;

#[derive(Clone)]
pub struct Site {
    pub config: PathBuf,
    pub url: String,
    pub include: String,
    pub test_command: String,
    pub php_endpoint: String,
}

pub fn create(
    nginx_bin: &Path,
    nginx_config: &Path,
    nginx_prefix: Option<&Path>,
    fpm_config: &Path,
    document_root: &Path,
    port: u16,
) -> Result<Site> {
    ensure!(port >= 1024, "本地站点端口必须在 1024 到 65535 之间");
    ensure!(nginx_bin.is_absolute() && nginx_bin.is_file(), "未找到 Nginx 程序");
    ensure!(nginx_config.is_absolute() && nginx_config.is_file(), "未找到 Nginx 主配置");
    ensure!(document_root.is_absolute(), "请输入站点公开目录的绝对路径");
    let document_root = document_root.canonicalize().context("无法访问站点公开目录")?;
    ensure!(document_root.is_dir(), "站点公开目录必须是文件夹");
    let root = nginx_literal(path_text(&document_root)?)?;

    let mut pools = Pools::default();
    pools.read(fpm_config, 0)?;
    let mut addresses = pools.listen.values();
    let address = addresses.next().context("未找到 PHP-FPM 的 listen 指令")?;
    ensure!(addresses.next().is_none(), "所选 PHP-FPM 配置有多个监听池，请为站点选择只有一个监听池的 PHP-FPM 版本");
    let php_endpoint = fastcgi_endpoint(address, port)?;
    let endpoint = if php_endpoint.starts_with("unix:") {
        nginx_literal(&php_endpoint)?
    } else {
        php_endpoint.clone()
    };

    let directory = SettingsStore::discover()?.service_directory().with_file_name("sites");
    let config = directory.join(format!("{port}.conf"));
    let include = format!("include {};", nginx_literal(path_text(&config)?)?);
    let mut test_command = format!("{} -t", shell_quote(path_text(nginx_bin)?));
    if let Some(prefix) = nginx_prefix {
        test_command.push_str(&format!(" -p {}", shell_quote(path_text(prefix)?)));
    }
    test_command.push_str(&format!(" -c {}", shell_quote(path_text(nginx_config)?)));
    let contents = format!(
        r#"# Usenv local Nginx + PHP-FPM site
# Add the following line inside http {{}} in the Nginx main configuration:
# {include}
server {{
    listen 127.0.0.1:{port};
    server_name 127.0.0.1 localhost;
    allow 127.0.0.1;
    deny all;
    root {root};
    index index.php index.html;

    types {{
        text/html               html htm;
        text/css                css;
        text/javascript         js mjs;
        application/json        json map;
        image/svg+xml           svg;
        image/png               png;
        image/jpeg              jpg jpeg;
        image/gif               gif;
        image/webp              webp;
        image/avif              avif;
        image/x-icon            ico;
        font/woff               woff;
        font/woff2              woff2;
    }}
    default_type application/octet-stream;

    location ~ /\. {{
        return 404;
    }}

    location / {{
        try_files $uri $uri/ /index.php?$query_string;
    }}

    location ~* \.php(?:$|/) {{
        try_files $uri =404;
        fastcgi_pass {endpoint};
        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
        fastcgi_param QUERY_STRING    $query_string;
        fastcgi_param REQUEST_METHOD  $request_method;
        fastcgi_param CONTENT_TYPE    $content_type;
        fastcgi_param CONTENT_LENGTH  $content_length;
        fastcgi_param SCRIPT_NAME     $fastcgi_script_name;
        fastcgi_param REQUEST_URI     $request_uri;
        fastcgi_param DOCUMENT_URI    $document_uri;
        fastcgi_param DOCUMENT_ROOT   $document_root;
        fastcgi_param SERVER_PROTOCOL $server_protocol;
        fastcgi_param REQUEST_SCHEME  $scheme;
        fastcgi_param GATEWAY_INTERFACE CGI/1.1;
        fastcgi_param SERVER_SOFTWARE nginx/$nginx_version;
        fastcgi_param REMOTE_ADDR     $remote_addr;
        fastcgi_param REMOTE_PORT     $remote_port;
        fastcgi_param SERVER_ADDR     $server_addr;
        fastcgi_param SERVER_PORT     $server_port;
        fastcgi_param SERVER_NAME     $server_name;
        fastcgi_param REDIRECT_STATUS 200;
        fastcgi_param HTTP_PROXY      "";
    }}

    location ~* \.(?:php[0-9]+|phtml|phar)(?:$|/) {{
        return 404;
    }}
}}
"#
    );

    DirBuilder::new().mode(0o700).recursive(true).create(&directory)?;
    ensure!(
        fs::symlink_metadata(&directory)?.is_dir(),
        "站点配置目录必须是真实目录"
    );
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist_noclobber(&config).map_err(|error| error.error)
        .with_context(|| format!("无法创建 {}，如已存在请在编辑器中修改", config.display()))?;
    Ok(Site {
        config,
        url: format!("http://127.0.0.1:{port}"),
        include,
        test_command,
        php_endpoint,
    })
}

#[derive(Default)]
struct Pools {
    pool: Option<String>,
    listen: BTreeMap<String, String>,
    file_count: usize,
}

impl Pools {
    fn read(&mut self, path: &Path, depth: usize) -> Result<()> {
        const MAX_BYTES: u64 = 256 * 1024;
        ensure!(depth < 6 && self.file_count < 64, "PHP-FPM 的配置包含层级或文件数量过多");
        self.file_count += 1;
        let metadata = path.metadata().with_context(|| format!("无法读取 {}", path.display()))?;
        ensure!(metadata.is_file() && metadata.len() <= MAX_BYTES, "无法读取 {}", path.display());
        let mut source = String::new();
        File::open(path)?.take(MAX_BYTES + 1).read_to_string(&mut source)
            .with_context(|| format!("无法读取 {}", path.display()))?;
        ensure!(source.len() as u64 <= MAX_BYTES, "PHP-FPM 配置文件过大");

        for line in source.lines() {
            let line = line.trim();
            if line.starts_with([';', '#']) {
                continue;
            }
            if let Some((section, _)) = line.strip_prefix('[').and_then(|line| line.split_once(']')) {
                let section = section.trim().to_ascii_lowercase();
                self.pool = (section != "global").then_some(section);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "include" => {
                    for included in include_paths(ini_value(value)?)? {
                        self.read(&included, depth + 1)?;
                    }
                }
                key if key.eq_ignore_ascii_case("listen") => {
                    if let Some(pool) = &self.pool {
                        self.listen.insert(pool.clone(), ini_value(value)?.to_owned());
                    }
                }
                _ => (),
            }
        }
        Ok(())
    }
}

fn ini_value(value: &str) -> Result<&str> {
    let value = value.trim();
    if let Some(unquoted) = value.strip_prefix('"') {
        let (value, suffix) = unquoted.split_once('"').context("PHP-FPM 配置的引号未闭合")?;
        ensure!(suffix.trim().is_empty() || suffix.trim().starts_with([';', '#']), "无法识别 PHP-FPM 配置值");
        return Ok(value);
    }
    Ok(value.split([';', '#']).next().unwrap_or_default().trim())
}

fn include_paths(pattern: &str) -> Result<Vec<PathBuf>> {
    let path = Path::new(pattern);
    ensure!(path.is_absolute() && !pattern.contains(['$', '?', '[', ']', '\\']),
        "PHP-FPM 的 include 需要使用绝对路径或目录下的 *.conf");
    let (directory, filename) = path.parent().zip(path.file_name().and_then(|name| name.to_str()))
        .context("无法识别 PHP-FPM 的 include 路径")?;
    ensure!(!path_text(directory)?.contains('*'), "PHP-FPM 的 include 通配符只能用在文件名中");
    let Some((prefix, suffix)) = filename.split_once('*') else {
        return Ok(vec![path.to_owned()]);
    };
    ensure!(!suffix.contains('*'), "PHP-FPM 的 include 文件名只能使用一个 *");
    let mut paths = Vec::new();
    for entry in directory.read_dir().with_context(|| format!("无法读取 {}", directory.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| {
            (prefix.starts_with('.') || !name.starts_with('.'))
                && name.starts_with(prefix) && name.ends_with(suffix)
                && name.len() >= prefix.len() + suffix.len()
        }) && !entry.path().is_dir() {
            ensure!(paths.len() < 64, "PHP-FPM 的 include 匹配了过多文件");
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn fastcgi_endpoint(address: &str, site_port: u16) -> Result<String> {
    if Path::new(address).is_absolute() {
        return Ok(format!("unix:{address}"));
    }
    let socket = if let Ok(port) = address.parse::<u16>() {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)
    } else if let Some(port) = address.strip_prefix("localhost:") {
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port.parse()?)
    } else {
        address.parse::<SocketAddr>().context("PHP-FPM 的 listen 需是本地 TCP 地址或绝对 Unix socket 路径")?
    };
    let ip = match socket.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    ensure!(ip.is_loopback() && socket.port() != 0, "请选择使用本地地址的 PHP-FPM 监听池");
    ensure!(socket.port() != site_port, "站点端口与 PHP-FPM 的监听端口相同");
    Ok(SocketAddr::new(ip, socket.port()).to_string())
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("路径包含无法识别的字符")
}

fn nginx_literal(value: &str) -> Result<String> {
    ensure!(!value.chars().any(char::is_control) && !value.contains(['$', '*', '?', '[', ']']),
        "路径包含 Nginx 配置无法直接使用的字符");
    Ok(format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
}
