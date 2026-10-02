//! JSON lines in <log dir>/proxy.log, rotated to proxy.log.1 at 10 MB. Readers (App
//! Sandbox's API and GUI) tail this file. A connection that gets through writes an
//! "open" line as soon as it is connected and a "close" line (bytes, duration) when it
//! ends; a refused or failed one writes only "close". Lines of one connection share "id".

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 10 * 1024 * 1024;

pub struct Logger {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

pub struct Entry<'a> {
    pub id: u64,
    pub phase: &'a str,
    pub vm: &'a str,
    pub vm_id: &'a str,
    pub method: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub ip: &'a str,
    pub result: &'a str,
    pub up: u64,
    pub down: u64,
    pub ms: u128,
}

/// RFC 3339 UTC timestamp without external crates (civil-from-days).
pub fn utc_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

impl Logger {
    pub fn new(dir: Option<PathBuf>) -> Logger {
        let path = dir.map(|d| d.join("proxy.log")).unwrap_or_default();
        Logger { path, file: Mutex::new(None) }
    }

    pub fn line(e: &Entry) -> String {
        format!(
            "{{\"t\":\"{}\",\"id\":{},\"phase\":{},\"vm\":{},\"vmId\":{},\"method\":{},\"host\":{},\"port\":{},\"ip\":{},\"result\":{},\"up\":{},\"down\":{},\"ms\":{}}}\n",
            utc_now(), e.id, json_str(e.phase), json_str(e.vm), json_str(e.vm_id), json_str(e.method), json_str(e.host), e.port,
            json_str(e.ip), json_str(e.result), e.up, e.down, e.ms
        )
    }

    pub fn write(&self, e: &Entry) {
        let line = Self::line(e);
        if self.path.as_os_str().is_empty() {
            eprint!("{line}");
            return;
        }
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if fs::metadata(&self.path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
            *guard = None;
            let _ = fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        if guard.is_none() {
            *guard = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        }
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaping_and_shape() {
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
        let l = Logger::line(&Entry { id: 7, phase: "close", vm: "AgentTest-1", vm_id: "x", method: "CONNECT", host: "github.com",
                                      port: 443, ip: "1.2.3.4", result: "ok", up: 1, down: 2, ms: 3 });
        assert!(l.starts_with("{\"t\":\"20"));
        assert!(l.contains(",\"id\":7,\"phase\":\"close\",\"vm\":\"AgentTest-1\","));
        assert!(l.ends_with("\"up\":1,\"down\":2,\"ms\":3}\n"));
    }

    #[test]
    fn timestamp_format() {
        let t = utc_now();
        assert_eq!(t.len(), 20);
        assert!(t.ends_with('Z') && &t[4..5] == "-" && &t[10..11] == "T");
    }
}
