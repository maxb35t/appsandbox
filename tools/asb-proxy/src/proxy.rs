//! One client connection: read the request head, check the policy, resolve, check every
//! resolved address, connect, then tunnel (CONNECT) or forward one request (plain HTTP).

use crate::duplex::{pump, Duplex};
use crate::http::{self, Target};
use crate::ipfilter;
use crate::logger::{Entry, Logger};
use crate::policy::Policy;
use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Shared {
    pub policy: RwLock<Arc<Policy>>,
    pub log: Logger,
    conns: Mutex<HashMap<String, Arc<AtomicUsize>>>,
    next_id: AtomicU64,
}

impl Shared {
    pub fn new(policy: Policy, log: Logger) -> Shared {
        Shared { policy: RwLock::new(Arc::new(policy)), log, conns: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1) }
    }
    pub fn policy(&self) -> Arc<Policy> {
        match self.policy.read() {
            Ok(p) => p.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
    fn counter(&self, vm: &str) -> Arc<AtomicUsize> {
        let mut m = match self.conns.lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        m.entry(vm.to_string()).or_insert_with(|| Arc::new(AtomicUsize::new(0))).clone()
    }
}

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Resolve and keep only addresses the rules allow.
fn resolve(host: &str, port: u16, block_private: bool) -> Result<Vec<SocketAddr>, &'static str> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => (host, port).to_socket_addrs().map_err(|_| "dns-failed")?.collect(),
    };
    if addrs.is_empty() {
        return Err("dns-failed");
    }
    let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| !block_private || !ipfilter::blocked(a.ip())).collect();
    if ok.is_empty() {
        return Err("private-address");
    }
    Ok(ok)
}

pub fn handle(shared: &Shared, mut client: Box<dyn Duplex>, vm_id: &str) {
    let started = Instant::now();
    // Only uniqueness matters (it pairs a connection's open and close lines).
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let policy = shared.policy();
    let rules = policy.rules_for(vm_id).cloned();
    let vm_name = rules.as_ref().map(|r| if r.name.is_empty() { vm_id.to_string() } else { r.name.clone() })
        .unwrap_or_else(|| vm_id.to_string());
    let mut entry_method = String::from("-");
    let mut entry_host = String::new();
    let mut entry_port = 0u16;
    let mut entry_ip = String::new();
    let (mut up, mut down) = (0u64, 0u64);
    let log_it = rules.as_ref().map(|r| r.log).unwrap_or(true);

    let result: String = (|| -> String {
        // Read the request first, so every refusal below is a clean HTTP reply.
        let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
        let (head, extra) = match http::read_head(&mut client) {
            Ok(h) => h,
            // Browsers open spare connections and never use them, then close them (or, when
            // the browser exits, reset them): not worth a log line.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe) => return "idle".into(),
            Err(_) => return "error:bad-request".into(),
        };
        let Some(rules) = rules.as_ref() else {
            let _ = client.write_all(&http::response(403, "Forbidden", "this VM has no proxy policy"));
            return "denied:unknown-vm".into();
        };
        let slot = shared.counter(vm_id);
        if slot.fetch_add(1, Ordering::AcqRel) >= rules.max_conns {
            slot.fetch_sub(1, Ordering::AcqRel);
            let _ = client.write_all(&http::response(429, "Too Many Requests", "connection limit reached"));
            return "denied:max-conns".into();
        }
        let _slot = Slot(slot);
        let Some(target) = http::parse_target(&head) else {
            let _ = client.write_all(&http::response(400, "Bad Request", "expected CONNECT host:port or an absolute http:// URL"));
            return "error:bad-request".into();
        };
        entry_method = target.method().to_string();
        entry_host = target.host().to_string();
        entry_port = target.port();

        if let Err(d) = rules.check(target.host(), target.port()) {
            let _ = client.write_all(&http::response(403, "Forbidden", &format!("blocked by policy ({})", d.as_str())));
            return format!("denied:{}", d.as_str());
        }
        let addrs = match resolve(target.host(), target.port(), rules.block_private) {
            Ok(a) => a,
            Err(why) => {
                let code = if why == "private-address" { 403 } else { 502 };
                let _ = client.write_all(&http::response(code, if code == 403 { "Forbidden" } else { "Bad Gateway" }, why));
                return format!("{}:{why}", if code == 403 { "denied" } else { "error" });
            }
        };
        let mut upstream = None;
        for a in &addrs {
            if let Ok(s) = TcpStream::connect_timeout(a, CONNECT_TIMEOUT) {
                entry_ip = a.ip().to_string();
                upstream = Some(s);
                break;
            }
        }
        let Some(upstream) = upstream else {
            let _ = client.write_all(&http::response(502, "Bad Gateway", "could not connect"));
            return "error:connect-failed".into();
        };
        let _ = upstream.set_nodelay(true);
        // Logged before the client hears back, so the line exists while the tunnel is open.
        if log_it {
            shared.log.write(&Entry {
                id, phase: "open", vm: &vm_name, vm_id, method: &entry_method, host: &entry_host, port: entry_port,
                ip: &entry_ip, result: "ok", up: 0, down: 0, ms: started.elapsed().as_millis(),
            });
        }
        let mut up_w: Box<dyn Duplex> = Box::new(upstream);

        match &target {
            Target::Connect { .. } => {
                if client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").is_err() {
                    return "error:client-gone".into();
                }
            }
            Target::Http { .. } => {
                let Some(fwd) = http::rewrite_http_head(&head, &target) else { return "error:bad-request".into() };
                if up_w.write_all(&fwd).is_err() {
                    return "error:upstream-write".into();
                }
                up += fwd.len() as u64;
            }
        }
        if !extra.is_empty() {
            if up_w.write_all(&extra).is_err() {
                return "error:upstream-write".into();
            }
            up += extra.len() as u64;
        }

        let _ = client.set_read_timeout(Some(IDLE_TIMEOUT));
        let _ = up_w.set_read_timeout(Some(IDLE_TIMEOUT));
        let (mut c2, mut u2) = match (client.try_clone_box(), up_w.try_clone_box()) {
            (Ok(c), Ok(u)) => (c, u),
            _ => return "error:clone".into(),
        };
        let down_thread = thread::spawn(move || pump(&mut *u2, &mut *c2));
        up += pump(&mut *client, &mut *up_w);
        down = down_thread.join().unwrap_or(0);
        "ok".into()
    })();

    if log_it && result != "idle" {
        shared.log.write(&Entry {
            id, phase: "close", vm: &vm_name, vm_id, method: &entry_method, host: &entry_host, port: entry_port, ip: &entry_ip,
            result: &result, up, down, ms: started.elapsed().as_millis(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A tiny upstream that answers one HTTP request with "hello" and records what it got.
    fn upstream() -> (u16, thread::JoinHandle<Vec<u8>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (head, _) = http::read_head(&mut s).unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").unwrap();
            head
        });
        (port, h)
    }

    /// Runs `handle` on one side of a local TCP pair; returns the client side.
    fn proxy_with(policy: &str) -> TcpStream {
        let shared = Arc::new(Shared::new(Policy::parse(policy), Logger::new(None)));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            handle(&shared, Box::new(s), "vm-1");
        });
        TcpStream::connect(addr).unwrap()
    }

    fn roundtrip(mut c: TcpStream, req: &str) -> String {
        c.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    }

    const OPEN: &str = "[default]\nallow_unknown=0\n[vm vm-1]\nname=T\nblock_private=0\nports=1-ignored\n";

    #[test]
    fn plain_http_is_rewritten_and_forwarded() {
        let (port, up) = upstream();
        let pol = OPEN.replace("1-ignored", &port.to_string());
        let out = roundtrip(proxy_with(&pol), &format!("GET http://127.0.0.1:{port}/x HTTP/1.1\r\nProxy-Connection: keep-alive\r\n\r\n"));
        assert!(out.ends_with("hello"), "{out}");
        let got = String::from_utf8(up.join().unwrap()).unwrap();
        assert!(got.starts_with("GET /x HTTP/1.1\r\n"), "{got}");
        assert!(got.contains("Connection: close") && !got.contains("Proxy-Connection"));
    }

    #[test]
    fn connect_tunnels() {
        let (port, up) = upstream();
        let pol = OPEN.replace("1-ignored", &port.to_string());
        let mut c = proxy_with(&pol);
        c.write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
        let mut buf = [0u8; 39];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"HTTP/1.1 200 Connection established\r\n\r\n");
        c.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        assert!(out.ends_with("hello"));
        up.join().unwrap();
    }

    #[test]
    fn private_addresses_blocked_when_policy_says_so() {
        let (port, _up) = upstream();
        let pol = format!("[default]\n[vm vm-1]\nports={port}\nblock_private=1\n");
        let out = roundtrip(proxy_with(&pol), &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n"));
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        assert!(out.contains("private-address"));
    }

    #[test]
    fn unknown_vm_and_port_denied() {
        let out = roundtrip(proxy_with("[default]\nallow_unknown=0\n"), "CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 403") && out.contains("no proxy policy"), "{out}");
        let out = roundtrip(proxy_with("[default]\n[vm vm-1]\nports=443\n"), "CONNECT example.com:22 HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 403") && out.contains("port-22"), "{out}");
    }

    /// Like proxy_with, but logging to a fresh temp dir; also returns the handler thread.
    fn proxy_logged(policy: &str, tag: &str) -> (TcpStream, thread::JoinHandle<()>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("asb-proxy-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shared = Arc::new(Shared::new(Policy::parse(policy), Logger::new(Some(dir.clone()))));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            handle(&shared, Box::new(s), "vm-1");
        });
        (TcpStream::connect(addr).unwrap(), h, dir.join("proxy.log"))
    }

    #[test]
    fn connect_logs_open_then_close() {
        let (port, up) = upstream();
        let pol = OPEN.replace("1-ignored", &port.to_string());
        let (mut c, h, log) = proxy_logged(&pol, "open");
        c.write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
        let mut buf = [0u8; 39];
        c.read_exact(&mut buf).unwrap();
        // The tunnel is still open: its "open" line is already there.
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("\"phase\":\"open\"") && text.contains("\"result\":\"ok\""), "{text}");
        c.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        drop(c);
        h.join().unwrap();
        up.join().unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[1].contains("\"phase\":\"close\"") && !lines[1].contains("\"down\":0,"), "{text}");
        let id = |l: &str| l.split("\"id\":").nth(1).unwrap().split(',').next().unwrap().to_string();
        assert_eq!(id(lines[0]), id(lines[1]));
    }

    #[test]
    fn unused_connection_is_not_logged() {
        let (c, h, log) = proxy_logged(OPEN, "idle");
        drop(c);
        h.join().unwrap();
        assert!(std::fs::read_to_string(&log).unwrap_or_default().is_empty());
    }

    #[test]
    fn garbage_is_rejected() {
        let out = roundtrip(proxy_with(OPEN), "HELLO\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }
}
