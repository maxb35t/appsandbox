//! asb-proxy: filtering HTTP/HTTPS proxy for App Sandbox VMs (maxb35t fork).
//!
//!   host    --policy FILE [--log-dir DIR] [--port N]   Windows host: accept VM connections
//!                                                      on Hyper-V socket port N (default 8)
//!   service --policy FILE [--log-dir DIR] [--port N]   same, run by the service manager
//!   guest   [LISTEN] [--port N]                        inside a VM: listen on LISTEN (default
//!                                                      127.0.0.1:3128), forward to the host
//!   check-policy FILE                                  parse a policy file and print it
//!
//! Host mode accepts from Windows guests (a5b0cafe-<N>-4000-8000-000000000001) and Linux
//! guests (<N>-facb-11e6-bd58-64006a7986d3), identifies the VM by the connection's VM id,
//! and applies that VM's rules from the policy file (re-read whenever it changes).

// Host mode is Windows-only, so on Linux (guest mode) much of the proxy is unused.
#![cfg_attr(not(windows), allow(dead_code))]

mod duplex;
mod http;
mod ipfilter;
mod logger;
mod policy;
mod proxy;
#[cfg(windows)]
mod hv;
#[cfg(windows)]
mod service;
#[cfg(target_os = "linux")]
mod vsock;

use duplex::{pump, Duplex};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::exit;
use std::thread;

const DEFAULT_PORT: u16 = 8;
const DEFAULT_LISTEN: &str = "127.0.0.1:3128";

struct Args {
    mode: String,
    positional: Option<String>,
    policy: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    port: u16,
}

fn usage() -> ! {
    eprintln!("usage: asb-proxy host|service --policy FILE [--log-dir DIR] [--port N]");
    eprintln!("       asb-proxy guest [LISTEN] [--port N]");
    eprintln!("       asb-proxy check-policy FILE");
    exit(2)
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mode = it.next().unwrap_or_else(|| usage());
    let mut a = Args { mode, positional: None, policy: None, log_dir: None, port: DEFAULT_PORT };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--policy" => a.policy = Some(it.next().unwrap_or_else(|| usage()).into()),
            "--log-dir" => a.log_dir = Some(it.next().unwrap_or_else(|| usage()).into()),
            "--port" => a.port = it.next().and_then(|p| p.parse().ok()).filter(|p| *p > 0).unwrap_or_else(|| usage()),
            _ if a.positional.is_none() && !arg.starts_with("--") => a.positional = Some(arg),
            _ => usage(),
        }
    }
    a
}

fn load_policy(path: &PathBuf) -> policy::Policy {
    match std::fs::read_to_string(path) {
        Ok(t) => policy::Policy::parse(&t),
        Err(e) => {
            eprintln!("asb-proxy: cannot read policy {}: {e}; refusing all VMs until it exists", path.display());
            policy::Policy::default() // allow_unknown = false, no VMs: everything denied
        }
    }
}

/// Re-reads the policy file when its modification time changes.
fn watch_policy(shared: std::sync::Arc<proxy::Shared>, path: PathBuf) {
    thread::spawn(move || {
        let stamp = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut last = stamp(&path);
        loop {
            thread::sleep(std::time::Duration::from_secs(2));
            let now = stamp(&path);
            if now != last {
                last = now;
                let p = std::sync::Arc::new(load_policy(&path));
                match shared.policy.write() {
                    Ok(mut g) => *g = p,
                    Err(e) => *e.into_inner() = p,
                }
            }
        }
    });
}

#[cfg(windows)]
fn run_host(a: &Args) -> Result<(), String> {
    use std::sync::Arc;
    let policy_path = a.policy.clone().ok_or("--policy is required")?;
    hv::init().map_err(|e| e.to_string())?;
    let shared = Arc::new(proxy::Shared::new(load_policy(&policy_path), logger::Logger::new(a.log_dir.clone())));
    watch_policy(shared.clone(), policy_path);
    let mut handles = Vec::new();
    for (label, svc) in [("windows", hv::windows_service(a.port)), ("linux", hv::linux_service(a.port))] {
        let l = hv::Listener::bind(hv::HV_GUID_CHILDREN, svc).map_err(|e| format!("{label} listener: {e}"))?;
        let shared = shared.clone();
        handles.push(thread::spawn(move || loop {
            match l.accept() {
                Ok((stream, vm)) => {
                    let shared = shared.clone();
                    thread::spawn(move || proxy::handle(&shared, Box::new(stream), &vm.lower()));
                }
                Err(e) => {
                    eprintln!("asb-proxy: accept ({label}): {e}");
                    thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }));
    }
    eprintln!("asb-proxy: host mode, channel port {}, policy {}", a.port,
              a.policy.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// Guest side: plain TCP <-> host channel, no parsing (the host does all checks).
fn run_guest(a: &Args) -> Result<(), String> {
    #[cfg(windows)]
    hv::init().map_err(|e| e.to_string())?;
    let listen = a.positional.clone().unwrap_or_else(|| DEFAULT_LISTEN.into());
    let l = TcpListener::bind(&listen).map_err(|e| format!("listen {listen}: {e}"))?;
    eprintln!("asb-proxy: guest mode, {listen} -> host channel port {}", a.port);
    let port = a.port;
    for conn in l.incoming() {
        let Ok(tcp) = conn else { continue };
        thread::spawn(move || {
            let host: std::io::Result<Box<dyn Duplex>> = {
                #[cfg(windows)]
                { hv::HvStream::connect(hv::HV_GUID_PARENT, hv::windows_service(port)).map(|s| Box::new(s) as Box<dyn Duplex>) }
                #[cfg(target_os = "linux")]
                { vsock::VsockStream::connect_host(port as u32).map(|s| Box::new(s) as Box<dyn Duplex>) }
                #[cfg(not(any(windows, target_os = "linux")))]
                { Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported OS")) }
            };
            let mut host = match host {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("asb-proxy: connect to host: {e}");
                    return;
                }
            };
            let mut client: Box<dyn Duplex> = Box::new(tcp);
            let (Ok(mut c2), Ok(mut h2)) = (client.try_clone_box(), host.try_clone_box()) else { return };
            let t = thread::spawn(move || pump(&mut *h2, &mut *c2));
            pump(&mut *client, &mut *host);
            let _ = t.join();
        });
    }
    Ok(())
}

fn main() {
    let a = parse_args();
    let r = match a.mode.as_str() {
        #[cfg(windows)]
        "host" => run_host(&a),
        #[cfg(windows)]
        "service" => {
            let args = std::sync::Arc::new(a);
            let run_args = args.clone();
            service::run_as_service(Box::new(move || {
                if let Err(e) = run_host(&run_args) {
                    eprintln!("asb-proxy: {e}");
                }
            }))
        }
        "guest" => run_guest(&a),
        "check-policy" => {
            let path: PathBuf = a.positional.clone().unwrap_or_else(|| usage()).into();
            let p = load_policy(&path);
            println!("allow_unknown={} default={:?}", p.allow_unknown, p.default);
            for (id, r) in &p.vms {
                println!("{id} {r:?}");
            }
            Ok(())
        }
        _ => usage(),
    };
    if let Err(e) = r {
        eprintln!("asb-proxy: {e}");
        exit(1);
    }
}
