use anyhow::{Result, bail};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyClass;
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum LoginStatus {
    #[default]
    Unavailable,
    Disabled,
    PendingApproval,
    Enabled,
}

fn available() -> bool {
    let in_bundle = std::env::current_exe().ok().is_some_and(|path| {
        path.parent()
            .and_then(|macos| macos.parent())
            .is_some_and(|contents| contents.join("Info.plist").is_file())
    });
    in_bundle && AnyClass::get(c"SMAppService").is_some()
}

// Read the launch event during applicationDidFinishLaunching, before any UI is opened.
pub fn launched_at_login() -> bool {
    let Some(event) = NSAppleEventManager::sharedAppleEventManager().currentAppleEvent() else {
        return false;
    };
    // Apple Event IDs and keywords are four-character codes (UInt32).
    let event_id: u32 = unsafe { objc2::msg_send![&*event, eventID] };
    if event_id != u32::from_be_bytes(*b"oapp") {
        return false;
    }
    let launch_reason: Option<Retained<NSAppleEventDescriptor>> = unsafe {
        objc2::msg_send![&*event, paramDescriptorForKeyword: u32::from_be_bytes(*b"prdt")]
    };
    launch_reason.is_some_and(|reason| reason.enumCodeValue() == u32::from_be_bytes(*b"lgit"))
}

pub fn status() -> LoginStatus {
    if !available() {
        return LoginStatus::default();
    }
    autoreleasepool(|_| {
        // The class is present and the service refers to this app's own bundle.
        let status = unsafe { SMAppService::mainAppService().status() };
        match status {
            SMAppServiceStatus::Enabled => LoginStatus::Enabled,
            SMAppServiceStatus::RequiresApproval => LoginStatus::PendingApproval,
            // A new app may have no service record until it first registers.
            _ => LoginStatus::Disabled,
        }
    })
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    if !available() {
        bail!("请从 macOS 应用包 Usenv.app 启动后再设置登录启动");
    }
    autoreleasepool(|_| {
        // SMAppService is available on this system and targets our own app bundle.
        let service = unsafe { SMAppService::mainAppService() };
        let status = unsafe { service.status() };
        let registered =
            status == SMAppServiceStatus::Enabled || status == SMAppServiceStatus::RequiresApproval;
        if enabled && !registered {
            unsafe { service.registerAndReturnError() }
                .map_err(|error| anyhow::anyhow!("注册登录项失败：{error}"))?;
        } else if !enabled && registered {
            unsafe { service.unregisterAndReturnError() }
                .map_err(|error| anyhow::anyhow!("移除登录项失败：{error}"))?;
        }
        let status = unsafe { service.status() };
        if enabled && status == SMAppServiceStatus::RequiresApproval {
            bail!("请在 macOS「系统设置 → 通用 → 登录项与扩展」中允许 usenv");
        }
        let registered =
            status == SMAppServiceStatus::Enabled || status == SMAppServiceStatus::RequiresApproval;
        if registered != enabled {
            bail!("macOS 未能更新 usenv 的登录项状态");
        }
        Ok(())
    })
}
