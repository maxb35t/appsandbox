//! Running as a Windows service (App Sandbox installs it as "AppSandboxProxy", running as
//! NT AUTHORITY\LocalService). The service's command-line arguments carry the config.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

pub const SERVICE_NAME: &str = "AppSandboxProxy";

const SERVICE_WIN32_OWN_PROCESS: u32 = 0x10;
const SERVICE_STOPPED: u32 = 1;
const SERVICE_STOP_PENDING: u32 = 3;
const SERVICE_RUNNING: u32 = 4;
const SERVICE_ACCEPT_STOP: u32 = 1;
const SERVICE_ACCEPT_SHUTDOWN: u32 = 4;
const SERVICE_CONTROL_STOP: u32 = 1;
const SERVICE_CONTROL_INTERROGATE: u32 = 4;
const SERVICE_CONTROL_SHUTDOWN: u32 = 5;
const NO_ERROR: u32 = 0;
const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

#[repr(C)]
struct ServiceTableEntry {
    name: *mut u16,
    proc_: Option<extern "system" fn(u32, *mut *mut u16)>,
}

#[repr(C)]
struct ServiceStatus {
    service_type: u32,
    current_state: u32,
    controls_accepted: u32,
    win32_exit_code: u32,
    service_specific_exit_code: u32,
    check_point: u32,
    wait_hint: u32,
}

type Handler = extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;

#[link(name = "advapi32")]
extern "system" {
    fn StartServiceCtrlDispatcherW(table: *const ServiceTableEntry) -> i32;
    fn RegisterServiceCtrlHandlerExW(name: *const u16, handler: Handler, ctx: *mut c_void) -> isize;
    fn SetServiceStatus(handle: isize, status: *const ServiceStatus) -> i32;
}

static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static RUN: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn report(state: u32) {
    let st = ServiceStatus {
        service_type: SERVICE_WIN32_OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        win32_exit_code: NO_ERROR,
        service_specific_exit_code: 0,
        check_point: 0,
        wait_hint: if state == SERVICE_STOP_PENDING { 5000 } else { 0 },
    };
    let h = STATUS_HANDLE.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: `h` came from RegisterServiceCtrlHandlerExW; `st` is valid for the call.
        unsafe { SetServiceStatus(h, &st) };
    }
}

extern "system" fn handler(control: u32, _ty: u32, _data: *mut c_void, _ctx: *mut c_void) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING);
            STOP.store(true, Ordering::Release);
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let name = wide(SERVICE_NAME);
    // SAFETY: `name` is NUL-terminated and outlives the call; `handler` has the right ABI.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), handler, std::ptr::null_mut()) };
    if h == 0 {
        return;
    }
    STATUS_HANDLE.store(h, Ordering::Release);
    report(SERVICE_RUNNING);
    if let Some(run) = RUN.get() {
        // The proxy runs until the process exits; the service just waits for STOP.
        thread::spawn(run);
    }
    while !STOP.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(250));
    }
    report(SERVICE_STOPPED);
}

/// Hands the process to the service control manager; returns once the service stops.
pub fn run_as_service(run: Box<dyn Fn() + Send + Sync>) -> Result<(), String> {
    let _ = RUN.set(run);
    let mut name = wide(SERVICE_NAME);
    let table = [
        ServiceTableEntry { name: name.as_mut_ptr(), proc_: Some(service_main) },
        ServiceTableEntry { name: std::ptr::null_mut(), proc_: None },
    ];
    // SAFETY: `table` is a NULL-terminated SERVICE_TABLE_ENTRYW array that lives for the call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(format!("not started by the service manager ({})", std::io::Error::last_os_error()));
    }
    Ok(())
}
