use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::brew::Environment;
use crate::config::{self, ServicePaths};
use crate::settings::atomic_write;

pub struct Instance {
    root: PathBuf,
    environment: Environment,
}

impl Instance {
    pub fn new(directory: &Path, formula: &str) -> Result<Self> {
        let environment = Environment::from_formula(formula)
            .filter(Environment::supports_instance)
            .with_context(|| format!("不支持的服务 {formula}"))?;
        Ok(Self {
            root: directory.join(formula),
            environment,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> PathBuf {
        self.root.join(match self.environment {
            Environment::Mysql | Environment::Mariadb => "my.cnf",
            Environment::Postgresql => "data/postgresql.conf",
            Environment::Redis => "redis.conf",
            Environment::Nginx => "nginx.conf",
            Environment::Php => "php-fpm.conf",
            Environment::Meilisearch => "config.toml",
            _ => unreachable!(),
        })
    }

    pub fn credentials(&self) -> Option<PathBuf> {
        match self.environment {
            Environment::Mysql => Some(self.root.join("data/initial-password.log")),
            Environment::Postgresql => Some(self.root.join("data/initial-password.txt")),
            _ => None,
        }
    }

    pub fn paths(&self, opt: &Path) -> ServicePaths {
        let (binary, data) = match self.environment {
            Environment::Mysql => ("bin/mysqld", Some("data")),
            Environment::Mariadb => ("bin/mariadbd", Some("data")),
            Environment::Postgresql => ("bin/postgres", Some("data")),
            Environment::Redis => ("bin/redis-server", Some("data")),
            Environment::Nginx => ("bin/nginx", Some("www")),
            Environment::Php => ("sbin/php-fpm", None),
            Environment::Meilisearch => ("bin/meilisearch", Some("data.ms")),
            _ => unreachable!(),
        };
        ServicePaths {
            executable: opt.join(binary),
            config: Some(self.config()),
            data: data.map(|subdir| self.root.join(subdir)),
            php_ini: (self.environment == Environment::Php).then(|| self.root.join("php.ini")),
            initial_credentials: self.credentials().is_some_and(|path| path.is_file()),
        }
    }

    pub fn prepare_php_ini(&self, opt: &Path) -> Result<PathBuf> {
        let path = self.root.join("php.ini");
        match fs::symlink_metadata(&path) {
            Ok(_) => require_file(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let bytes = match config::php_loaded_ini(&opt.join("bin"))? {
                    Some(source) => fs::read(&source)
                        .with_context(|| format!("无法读取 {}", source.display()))?,
                    None => b"; usenv PHP-FPM configuration\n".to_vec(),
                };
                atomic_write(&path, &bytes)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(path)
    }

    pub fn prepare(&self, opt: &Path) -> Result<()> {
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&self.root)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join(".setup.lock"))?;
        lock.lock()?;
        fs::create_dir_all(self.root.join("logs"))?;
        match self.environment {
            Environment::Mysql | Environment::Mariadb | Environment::Postgresql => {
                self.prepare_database(opt)?;
            }
            Environment::Redis => fs::create_dir_all(self.root.join("data"))?,
            Environment::Meilisearch => (),
            Environment::Nginx => fs::create_dir_all(self.root.join("www"))?,
            Environment::Php => {
                self.prepare_php_ini(opt)?;
            }
            _ => unreachable!(),
        }
        let template = match self.environment {
            Environment::Mysql => "[mysqld]\nbind-address = 127.0.0.1\nport = 3306\nmysqlx = OFF\n",
            Environment::Mariadb => concat!(
                "# Usenv Unix socket: mariadb.sock in this instance directory.\n",
                "[mysqld]\n",
                "bind-address = 127.0.0.1\n",
                "port = 3307\n",
            ),
            Environment::Redis => "bind 127.0.0.1 ::1\nprotected-mode yes\nport 6379\ndir ./\n",
            Environment::Meilisearch => "http_addr = \"127.0.0.1:7700\"\ndb_path = \"data.ms\"\n",
            Environment::Nginx => concat!(
                "worker_processes 1;\n",
                "pid logs/nginx.pid;\n",
                "error_log logs/nginx-error.log;\n",
                "events { worker_connections 256; }\n",
                "http {\n",
                "    client_body_temp_path client_body_temp;\n",
                "    proxy_temp_path proxy_temp;\n",
                "    fastcgi_temp_path fastcgi_temp;\n",
                "    uwsgi_temp_path uwsgi_temp;\n",
                "    scgi_temp_path scgi_temp;\n",
                "    access_log logs/nginx-access.log;\n",
                "    server {\n",
                "        listen 127.0.0.1:8080;\n",
                "        root www;\n",
                "        index index.html;\n",
                "    }\n",
                "}\n",
            ),
            Environment::Php => concat!(
                "[global]\n",
                "error_log = logs/php-fpm-error.log\n",
                "\n[www]\n",
                "listen = 127.0.0.1:9000\n",
                "pm = ondemand\n",
                "pm.max_children = 5\n",
            ),
            Environment::Postgresql => {
                ensure!(
                    self.config().is_file(),
                    "找不到 {}",
                    self.config().display()
                );
                return Ok(());
            }
            _ => unreachable!(),
        };
        let path = self.config();
        match fs::symlink_metadata(&path) {
            Ok(_) => ensure!(path.is_file(), "{} 不是配置文件", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                atomic_write(&path, template.as_bytes())?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub fn write_job(&self, opt: &Path, label: &str) -> Result<PathBuf> {
        let config = self.config();
        ensure!(config.is_file(), "找不到配置文件 {}", config.display());
        let data = self.root.join("data");
        let mut cwd = self.root.clone();
        let mut arguments = Vec::new();
        match self.environment {
            Environment::Mysql | Environment::Mariadb => {
                self.require_database(&data)?;
                let socket = if self.environment == Environment::Mariadb {
                    path_text(&self.root.join("mariadb.sock"))?.into()
                } else {
                    match config::mysql_socket(opt, &config)? {
                        Some(socket) => socket,
                        None => path_text(&self.root.join("mysql.sock"))?.into(),
                    }
                };
                arguments.extend([
                    format!("--defaults-file={}", path_text(&config)?),
                    format!("--datadir={}", path_text(&data)?),
                    format!("--socket={socket}"),
                ]);
            }
            Environment::Postgresql => {
                self.require_database(&data)?;
                arguments.extend([
                    "-D".into(),
                    path_text(&data)?.into(),
                    "-c".into(),
                    format!("data_directory={}", path_text(&data)?),
                ]);
            }
            Environment::Redis => {
                ensure!(data.is_dir(), "找不到数据目录 {}", data.display());
                cwd = data.clone();
                arguments.extend([
                    path_text(&config)?.into(),
                    "--dir".into(),
                    path_text(&data)?.into(),
                    "--daemonize".into(),
                    "no".into(),
                ]);
            }
            Environment::Nginx => {
                arguments.extend([
                    "-p".into(),
                    path_text(&self.root)?.into(),
                    "-c".into(),
                    path_text(&config)?.into(),
                    "-g".into(),
                    "daemon off;".into(),
                    "-e".into(),
                    "stderr".into(),
                ]);
            }
            Environment::Php => {
                let ini = self.prepare_php_ini(opt)?;
                arguments.extend([
                    "-F".into(),
                    "-p".into(),
                    path_text(&self.root)?.into(),
                    "-y".into(),
                    path_text(&config)?.into(),
                    "-c".into(),
                    path_text(&ini)?.into(),
                ]);
            }
            Environment::Meilisearch => {
                arguments.extend(["--config-file-path".into(), path_text(&config)?.into()]);
            }
            _ => unreachable!(),
        };
        let program = self.paths(opt).executable;
        require_file(&program)?;
        arguments.insert(0, path_text(&program)?.into());
        let log = self.root.join("logs/service.log");
        let mut job = json!({
            "Label": label,
            "ProgramArguments": arguments,
            "RunAtLoad": true,
            "KeepAlive": true,
            "ExitTimeOut": 120,
            "WorkingDirectory": cwd,
            "StandardOutPath": log,
            "StandardErrorPath": log,
        });
        if self.environment == Environment::Postgresql {
            job["EnvironmentVariables"] = json!({"LC_ALL": "C"});
            job["KeepAlive"] = json!({"SuccessfulExit": false});
        }
        if self.environment == Environment::Meilisearch {
            job["KeepAlive"] = json!(false);
        }
        let plist = self.root.join("service.plist");
        write_launch_job(&plist, &job)?;
        Ok(plist)
    }

    pub fn fast_stop_postgresql(&self, opt: &Path, pid: u32) -> Result<()> {
        if self.environment != Environment::Postgresql {
            return Ok(());
        }
        let data = self.root.join("data");
        let recorded = fs::read_to_string(data.join("postmaster.pid"))
            .ok()
            .and_then(|source| source.lines().next()?.trim().parse::<u32>().ok());
        if recorded != Some(pid) {
            return Ok(());
        }
        let output = Command::new(opt.join("bin/pg_ctl"))
            .arg("-D")
            .arg(data)
            .args(["-m", "fast", "-W", "stop"])
            .output()?;
        ensure!(
            output.status.success(),
            "PostgreSQL 快速关闭失败\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }

    fn prepare_database(&self, opt: &Path) -> Result<()> {
        let data = self.root.join("data");
        match fs::symlink_metadata(&data) {
            Ok(_) => return self.require_database(&data),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        // Initialize in an adjacent directory so a failed run never leaves a partial cluster.
        let staging = Staging::new(&self.root)?;
        let cluster = staging.0.join("data");
        if self.environment == Environment::Postgresql {
            let user = current_user()?;
            let password = Command::new("/usr/bin/openssl")
                .args(["rand", "-hex", "24"])
                .output()?;
            ensure!(password.status.success(), "无法生成 PostgreSQL 初始密码");
            let password_file = staging.0.join("password");
            atomic_write(&password_file, &password.stdout)?;
            run_init(
                Command::new(opt.join("bin/initdb")).args([
                    "-D",
                    path_text(&cluster)?,
                    "--auth-local=peer",
                    "--auth-host=scram-sha-256",
                    "--encoding=UTF8",
                    "--locale=C",
                    "-U",
                    &user,
                    "--pwfile",
                    path_text(&password_file)?,
                    "--no-instructions",
                ]),
                "PostgreSQL",
            )?;
            let config = cluster.join("postgresql.conf");
            OpenOptions::new()
                .append(true)
                .open(config)?
                .write_all(b"\nlisten_addresses = 'localhost'\nunix_socket_directories = ''\n")?;
            atomic_write(
                &cluster.join("initial-password.txt"),
                format!(
                    "用户 / User: {}\n密码 / Password: {}",
                    user,
                    String::from_utf8_lossy(&password.stdout)
                )
                .as_bytes(),
            )?;
        } else if self.environment == Environment::Mysql {
            let output = run_init(
                Command::new(opt.join("bin/mysqld"))
                    .args(["--no-defaults", "--initialize"])
                    .arg(format!("--datadir={}", path_text(&cluster)?)),
                "MySQL",
            )?;
            atomic_write(&cluster.join("initial-password.log"), &output)?;
        } else {
            let user = current_user()?;
            run_init(
                Command::new(opt.join("bin/mariadb-install-db"))
                    .env_remove("MYSQLD_BOOTSTRAP")
                    .env("TMPDIR", &staging.0)
                    .args([
                        "--no-defaults",
                        "--auth-root-authentication-method=socket",
                        "--skip-test-db",
                    ])
                    .arg(format!("--basedir={}", path_text(opt)?))
                    .arg(format!("--datadir={}", path_text(&cluster)?))
                    .arg(format!("--auth-root-socket-user={user}")),
                "MariaDB",
            )?;
        }
        self.require_database(&cluster)?;
        fs::rename(cluster, &data)
            .with_context(|| format!("无法保存数据目录 {}", data.display()))?;
        Ok(())
    }

    fn require_database(&self, data: &Path) -> Result<()> {
        match self.environment {
            Environment::Mysql => require_file(&data.join("mysql.ibd")),
            Environment::Postgresql => require_file(&data.join("PG_VERSION")),
            Environment::Mariadb => {
                ensure!(
                    fs::symlink_metadata(data)?.is_dir()
                        && data.join("mysql").is_dir()
                        && !data.join("mysql.ibd").exists(),
                    "{} 不是独立的 MariaDB 数据目录",
                    data.display()
                );
                require_file(&data.join("aria_log_control"))
            }
            _ => unreachable!(),
        }
    }
}

pub(crate) fn write_launch_job(plist: &Path, job: &serde_json::Value) -> Result<()> {
    let mut convert = Command::new("/usr/bin/plutil")
        .args(["-convert", "xml1", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("无法生成服务启动项")?;
    convert
        .stdin
        .take()
        .context("无法写入启动项")?
        .write_all(&serde_json::to_vec(job)?)?;
    let result = convert.wait_with_output()?;
    ensure!(result.status.success(), "无法生成服务启动项");
    atomic_write(plist, &result.stdout)
}

pub(crate) fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("路径必须是 UTF-8 文本 · {}", path.display()))
}

fn require_file(path: &Path) -> Result<()> {
    ensure!(path.is_file(), "缺少文件 {}", path.display());
    Ok(())
}

fn current_user() -> Result<String> {
    let output = Command::new("/usr/bin/id").arg("-un").output()?;
    ensure!(output.status.success(), "无法读取当前用户名");
    Ok(std::str::from_utf8(&output.stdout)?.trim_end().to_owned())
}

fn run_init(command: &mut Command, service: &str) -> Result<Vec<u8>> {
    let output = command
        .output()
        .with_context(|| format!("无法运行 {service} 初始化命令"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = stderr
            .lines()
            .chain(stdout.lines())
            .filter(|line| !line.contains("temporary password"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("{service} 数据目录初始化失败\n{}", details.trim());
    }
    Ok(output.stderr)
}

pub(crate) struct Staging(pub PathBuf);

impl Staging {
    pub(crate) fn new(parent: &Path) -> Result<Self> {
        let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = parent.join(format!(".init-{}-{id}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
