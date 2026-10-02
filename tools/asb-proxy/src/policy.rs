//! Proxy policy, read from a small text file App Sandbox writes:
//!
//! ```text
//! # asb-proxy policy v1
//! [default]
//! allow_unknown=0          # 1 = VMs with no [vm ...] section use [default]
//! block_private=1
//! ports=80,443
//! allow=                   # comma list; empty = any host
//! deny=
//! log=1
//! max_conns=64
//! [vm 01234567-89ab-cdef-0123-456789abcdef]
//! name=AgentTest-1
//! block_private=1
//! ...                      # keys not given fall back to [default]
//! ```
//!
//! Host rules: an entry matches the host itself and every subdomain ("example.com"
//! matches "example.com" and "a.example.com"); a leading "*." is accepted and means
//! the same. Matching is case-insensitive. Deny wins over allow.

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    pub name: String,
    pub block_private: bool,
    pub ports: Vec<u16>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub log: bool,
    pub max_conns: usize,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            name: String::new(),
            block_private: true,
            ports: vec![80, 443],
            allow: Vec::new(),
            deny: Vec::new(),
            log: true,
            max_conns: 64,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub default: Rules,
    pub allow_unknown: bool,
    /// Keyed by lowercase VM GUID (no braces).
    pub vms: HashMap<String, Rules>,
}

/// Why a request was refused (logged and reported to the client).
#[derive(Debug, PartialEq)]
pub enum Denial {
    Port(u16),
    HostDenied,
    HostNotAllowed,
}

impl Denial {
    pub fn as_str(&self) -> String {
        match self {
            Denial::Port(p) => format!("port-{p}"),
            Denial::HostDenied => "host-denied".into(),
            Denial::HostNotAllowed => "host-not-allowed".into(),
        }
    }
}

fn normalise_host(h: &str) -> String {
    h.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn list(v: &str) -> Vec<String> {
    v.split([',', ' ', ';'])
        .map(|e| normalise_host(e.trim_start_matches("*.")))
        .filter(|e| !e.is_empty())
        .collect()
}

fn host_matches(host: &str, entry: &str) -> bool {
    host == entry || (host.len() > entry.len() && host.ends_with(entry) && host.as_bytes()[host.len() - entry.len() - 1] == b'.')
}

impl Rules {
    fn apply(&mut self, key: &str, value: &str) {
        let v = value.trim();
        match key {
            "name" => self.name = v.to_string(),
            "block_private" => self.block_private = v != "0",
            "ports" => self.ports = v.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
            "allow" => self.allow = list(v),
            "deny" => self.deny = list(v),
            "log" => self.log = v != "0",
            "max_conns" => {
                if let Ok(n) = v.parse::<usize>() {
                    self.max_conns = n.clamp(1, 4096);
                }
            }
            _ => {}
        }
    }

    /// Port and host checks (the resolved-address check happens after DNS).
    pub fn check(&self, host: &str, port: u16) -> Result<(), Denial> {
        if !self.ports.contains(&port) {
            return Err(Denial::Port(port));
        }
        let h = normalise_host(host);
        if self.deny.iter().any(|e| host_matches(&h, e)) {
            return Err(Denial::HostDenied);
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|e| host_matches(&h, e)) {
            return Err(Denial::HostNotAllowed);
        }
        Ok(())
    }
}

impl Policy {
    pub fn parse(text: &str) -> Policy {
        let mut p = Policy::default();
        // First pass: [default], so VM sections can start from it.
        let mut section: Option<String> = None;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = Some(line[1..line.len() - 1].trim().to_ascii_lowercase());
                continue;
            }
            if section.as_deref() == Some("default") {
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim();
                    if k == "allow_unknown" {
                        p.allow_unknown = v.trim() == "1";
                    } else {
                        p.default.apply(k, v);
                    }
                }
            }
        }
        let mut current: Option<String> = None;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let s = line[1..line.len() - 1].trim().to_ascii_lowercase();
                current = s.strip_prefix("vm ").map(|g| g.trim().trim_matches(|c| c == '{' || c == '}').to_string());
                if let Some(g) = &current {
                    p.vms.entry(g.clone()).or_insert_with(|| p.default.clone());
                }
                continue;
            }
            if let (Some(g), Some((k, v))) = (&current, line.split_once('=')) {
                if let Some(r) = p.vms.get_mut(g) {
                    r.apply(k.trim(), v);
                }
            }
        }
        p
    }

    /// Rules for a VM GUID, or None if the VM is unknown and unknown VMs are refused.
    pub fn rules_for(&self, vm_guid: &str) -> Option<&Rules> {
        match self.vms.get(&vm_guid.to_ascii_lowercase()) {
            Some(r) => Some(r),
            None if self.allow_unknown => Some(&self.default),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# asb-proxy policy v1\n[default]\nallow_unknown=0\nblock_private=1\nports=80,443\nlog=1\n\
        [vm 0123ABCD-0000-0000-0000-000000000001]\nname=AgentTest-1\nallow=*.github.com, crates.io\ndeny=evil.github.com\n\
        [vm {0123abcd-0000-0000-0000-000000000002}]\nname=Open\nports=443\nblock_private=0\n";

    #[test]
    fn parses_defaults_and_vm_sections() {
        let p = Policy::parse(SAMPLE);
        assert!(!p.allow_unknown);
        assert_eq!(p.default.ports, vec![80, 443]);
        let a = p.rules_for("0123abcd-0000-0000-0000-000000000001").unwrap();
        assert_eq!(a.name, "AgentTest-1");
        assert_eq!(a.allow, vec!["github.com", "crates.io"]);
        assert!(a.block_private);
        assert_eq!(a.ports, vec![80, 443]); // inherited from [default]
        let b = p.rules_for("0123ABCD-0000-0000-0000-000000000002").unwrap();
        assert_eq!(b.ports, vec![443]);
        assert!(!b.block_private);
        assert!(p.rules_for("ffffffff-0000-0000-0000-000000000000").is_none());
    }

    #[test]
    fn host_rules() {
        let p = Policy::parse(SAMPLE);
        let a = p.rules_for("0123abcd-0000-0000-0000-000000000001").unwrap();
        assert_eq!(a.check("github.com", 443), Ok(()));
        assert_eq!(a.check("API.GitHub.com.", 443), Ok(()));
        assert_eq!(a.check("static.crates.io", 80), Ok(()));
        assert_eq!(a.check("evil.github.com", 443), Err(Denial::HostDenied));
        assert_eq!(a.check("x.evil.github.com", 443), Err(Denial::HostDenied));
        assert_eq!(a.check("notgithub.com", 443), Err(Denial::HostNotAllowed));
        assert_eq!(a.check("github.com.evil.net", 443), Err(Denial::HostNotAllowed));
        assert_eq!(a.check("github.com", 22), Err(Denial::Port(22)));
    }

    #[test]
    fn allow_unknown_uses_default() {
        let p = Policy::parse("[default]\nallow_unknown=1\nports=443\n");
        let r = p.rules_for("anything").unwrap();
        assert_eq!(r.ports, vec![443]);
        assert_eq!(r.check("example.com", 443), Ok(()));
    }
}
