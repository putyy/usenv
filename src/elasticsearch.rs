use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::brew::Environment;
use crate::service::{Service, ServiceAction, ServiceBackend};
use crate::command;
use crate::config::ServicePaths;
use crate::instance::{Staging, path_text, write_launch_job};
use crate::runtimes::{MiseTool, valid_mise_version};

pub struct Elasticsearch {
    root: PathBuf,
    executable: PathBuf,
    bundled_config: PathBuf,
    version: String,
}

impl Elasticsearch {
    pub fn new(directory: &Path, tool: &MiseTool) -> Option<Self> {
        if tool.environment != Environment::Elasticsearch || !valid_mise_version(&tool.version) {
            return None;
        }
        let executable = fs::canonicalize(tool.executable.as_ref()?).ok()?;
        let bundled_config = executable.parent()?.parent()?.join("config");
        if !bundled_config.join("elasticsearch.yml").is_file() {
            return None;
        }
        Some(Self {
            root: directory.join(format!("elasticsearch@{}", tool.version)),
            executable,
            bundled_config,
            version: tool.version.clone(),
        })
    }

    pub fn service_name(&self) -> String {
        format!("elasticsearch@{}", self.version)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn label(&self) -> String {
        format!("io.github.putyy.usenv.elasticsearch.{}", self.version)
    }

    fn target(&self, domain: &str) -> String {
        format!("{domain}/{}", self.label())
    }

    pub fn service(&self, domain: &str) -> Service {
        let output = Command::new("/bin/launchctl")
            .args(["print", &self.target(domain)])
            .env("LC_ALL", "C")
            .output();
        let (loaded, running, status) = match output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let running = stdout.lines().any(|line| line.trim() == "state = running");
                let failed = stdout.lines().any(|line| {
                    line.trim()
                        .strip_prefix("last exit code = ")
                        .and_then(|code| code.parse::<i32>().ok())
                        .is_some_and(|code| code != 0)
                });
                (
                    true,
                    running,
                    if failed && !running { "error" } else { "none" },
                )
            }
            Ok(output) if service_not_found(&self.label(), &output.stderr)
                || service_not_found(&self.label(), &output.stdout) => (false, false, "none"),
            _ => (false, false, "unknown"),
        };
        Service {
            backend: ServiceBackend::MiseLaunchd,
            formula: self.service_name(),
            environment: Environment::Elasticsearch,
            running,
            loaded,
            auto_start: false,
            status: status.into(),
            manageable: true,
        }
    }

    pub fn paths(&self) -> ServicePaths {
        ServicePaths {
            executable: self.executable.clone(),
            config: Some(self.root.join("config/elasticsearch.yml")),
            data: Some(self.root.join("data")),
            php_ini: None,
            initial_credentials: false,
        }
    }

    pub fn config_file(&self) -> Result<PathBuf> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join(".setup.lock"))?;
        lock.lock()?;
        let config = self.root.join("config");
        if !config.exists() {
            let staging = Staging::new(&self.root)?;
            let copy = staging.0.join("config");
            let mut command = Command::new("/usr/bin/ditto");
            command.arg(&self.bundled_config).arg(&copy);
            command::output(command, "复制 Elasticsearch 配置", None)?;
            fs::rename(copy, &config)?;
        }
        let file = config.join("elasticsearch.yml");
        ensure!(file.is_file(), "找不到 {}", file.display());
        Ok(file)
    }

    pub fn operate(&self, domain: &str, action: ServiceAction) -> Result<()> {
        let service = self.service(domain);
        ensure!(
            service.status != "unknown",
            "无法读取 Elasticsearch 服务状态"
        );
        let target = self.target(domain);
        let stop = || {
            let mut command = Command::new("/bin/launchctl");
            command.args(["bootout", &target]);
            command::output(command, "停止 Elasticsearch", None).map(|_| ())
        };
        match action {
            ServiceAction::Stop if service.loaded => stop(),
            ServiceAction::Stop => Ok(()),
            ServiceAction::Start if service.running => Ok(()),
            ServiceAction::Start | ServiceAction::Restart => {
                let plist = self.prepare_job()?;
                if service.loaded {
                    stop()?;
                }
                let mut command = Command::new("/bin/launchctl");
                command.args(["bootstrap", domain]).arg(plist);
                command::output(command, "启动 Elasticsearch", None).map(|_| ())
            }
            ServiceAction::SetAutoStart(_) => anyhow::bail!("Elasticsearch 暂未提供登录启动"),
        }
    }

    fn prepare_job(&self) -> Result<PathBuf> {
        self.config_file()?;
        let data = self.root.join("data");
        let logs = self.root.join("logs");
        fs::create_dir_all(&data)?;
        fs::create_dir_all(&logs)?;
        let log = logs.join("service.log");
        let plist = self.root.join("service.plist");
        write_launch_job(
            &plist,
            &json!({
                "Label": self.label(),
                "RunAtLoad": true,
                "ExitTimeOut": 120,
                "ProgramArguments": [
                    path_text(&self.executable)?,
                    &format!("-Epath.data={}", path_text(&data)?),
                    &format!("-Epath.logs={}", path_text(&logs)?),
                    "-Ediscovery.type=single-node",
                    "-Enetwork.host=127.0.0.1",
                    "-Ehttp.host=127.0.0.1",
                ],
                "EnvironmentVariables": {"ES_PATH_CONF": self.root.join("config")},
                "WorkingDirectory": self.root,
                "StandardOutPath": log,
                "StandardErrorPath": log,
            }),
        )?;
        Ok(plist)
    }
}

fn service_not_found(label: &str, message: &[u8]) -> bool {
    let prefix = format!("Could not find service \"{label}\" in domain for ");
    String::from_utf8_lossy(message).lines().any(|line| line.trim().starts_with(&prefix))
}

pub fn launch_domain() -> Result<String> {
    let mut command = Command::new("/usr/bin/id");
    command.arg("-u");
    let uid = command::output(command, "读取当前用户 ID", None)?;
    let uid: u32 = uid.trim().parse().context("无效的用户 ID")?;
    Ok(format!("gui/{uid}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn missing_job_is_distinct_from_an_unreadable_domain() {
        let label = "io.github.putyy.usenv.elasticsearch.9.4.0";
        let missing = format!("Bad request.\nCould not find service \"{label}\" in domain for user gui: 501");
        assert!(service_not_found(label, missing.as_bytes()));
        assert!(!service_not_found("another.job", missing.as_bytes()));
        assert!(!service_not_found(label, b"Could not find domain for user gui: 501"));
        assert!(!service_not_found(label, b"Operation not permitted"));
    }

    #[test]
    fn launch_job_keeps_elastic_data_and_config_outside_mise() -> Result<()> {
        let staging = Staging::new(&std::env::temp_dir())?;
        let install = staging.0.join("mise/elasticsearch");
        let executable = install.join("bin/elasticsearch");
        fs::create_dir_all(install.join("bin"))?;
        fs::create_dir_all(install.join("config"))?;
        fs::write(&executable, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
        let bundled = install.join("config/elasticsearch.yml");
        fs::write(&bundled, "# Elastic defaults\n")?;
        let services = staging.0.join("usenv services");
        let instance = Elasticsearch::new(
            &services,
            &MiseTool {
                environment: Environment::Elasticsearch,
                name: "vfox:jdx/vfox-elasticsearch".into(),
                version: "9.4.0".into(),
                install_path: Some(install),
                executable: Some(executable),
                cargo_executable: None,
                active: Some(false),
            },
        )
        .context("missing instance")?;
        let config = instance.config_file()?;
        fs::write(&config, "http.port: 9210\n")?;
        let plist = fs::read_to_string(instance.prepare_job()?)?;
        assert!(plist.contains("-Enetwork.host=127.0.0.1"));
        assert!(plist.contains("usenv services/elasticsearch@9.4.0/data"));
        assert_eq!(fs::read_to_string(config)?, "http.port: 9210\n");
        assert_eq!(fs::read_to_string(bundled)?, "# Elastic defaults\n");
        Ok(())
    }
}
