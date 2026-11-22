use crate::brew::Environment;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceBackend {
    Homebrew,
    MiseLaunchd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub formula: String,
    pub environment: Environment,
    pub backend: ServiceBackend,
    pub running: bool,
    pub loaded: bool,
    pub auto_start: bool,
    pub status: String,
    pub manageable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    SetAutoStart(bool),
}

impl Service {
    pub fn toggle_action(&self) -> ServiceAction {
        if self.running || self.loaded {
            ServiceAction::Stop
        } else {
            ServiceAction::Start
        }
    }

    pub fn can_perform(&self, action: ServiceAction) -> bool {
        self.manageable && match action {
            ServiceAction::Start | ServiceAction::Stop => action == self.toggle_action(),
            ServiceAction::Restart => self.running,
            ServiceAction::SetAutoStart(_) => self.backend == ServiceBackend::Homebrew,
        }
    }

    pub fn supports_instance_mode(&self) -> bool {
        self.manageable
            && self.backend == ServiceBackend::Homebrew
            && self.environment.supports_instance()
    }

    pub fn can_uninstall(&self) -> bool {
        self.manageable && self.status != "unknown" && !self.running && !self.loaded && !self.auto_start
    }
}
