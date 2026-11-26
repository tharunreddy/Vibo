//! Native macOS Launch at Login via `SMAppService` (macOS 13+).

use objc2::runtime::AnyClass;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

pub fn is_available() -> bool {
    AnyClass::get(c"SMAppService").is_some()
}

pub fn set_launch_at_login(enabled: bool) {
    if !is_available() {
        return;
    }
    let service = unsafe { SMAppService::mainAppService() };
    let status = unsafe { service.status() };

    if enabled {
        if status == SMAppServiceStatus::Enabled {
            return;
        }
        match unsafe { service.registerAndReturnError() } {
            Ok(()) => tracing::info!("Registered Vibo login item via SMAppService"),
            Err(err) => tracing::warn!(error = %err, "Failed to register login item"),
        }
    } else {
        if status == SMAppServiceStatus::NotRegistered || status == SMAppServiceStatus::NotFound {
            return;
        }
        match unsafe { service.unregisterAndReturnError() } {
            Ok(()) => tracing::info!("Unregistered Vibo login item via SMAppService"),
            Err(err) => tracing::warn!(error = %err, "Failed to unregister login item"),
        }
    }
}
