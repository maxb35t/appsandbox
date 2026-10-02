//! The only HTTP parsing the proxy does: the client's first request head.
//! CONNECT host:port  -> tunnel.
//! GET http://host[:port]/path HTTP/1.x (any method) -> forward once, then Connection: close.

use std::io::{self, Read};

pub const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug, PartialEq)]
pub enum Target {
    Connect { host: String, port: u16 },
    Http { method: String, host: String, port: u16, path: String, version: String },
}

impl Target {
    pub fn host(&self) -> &str {
        match self {
            Target::Connect { host, .. } | Target::Http { host, .. } => host,
        }
    }
    pub fn port(&self) -> u16 {
        match self {
            Target::Connect { port, .. } | Target::Http { port, .. } => *port,
        }
    }
    pub fn method(&self) -> &str {
        match self {
            Target::Connect { .. } => "CONNECT",
            Target::Http { method, .. } => method,
        }
    }
}

/// Reads until the end of the request head. Returns (head bytes incl. CRLFCRLF,
/// any bytes read past it).
pub fn read_head<R: Read>(r: &mut R) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed before a request"));
        }
        let start = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf[start..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = start + pos + 4;
            let rest = buf.split_off(end);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'))
}

/// "host:port" or "[v6]:port"; default_port when no port is given.
fn split_authority(a: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => default_port?,
            None => return None,
        };
        (h.to_string(), port)
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().ok()?),
            Some(_) => return None, // bare IPv6 without brackets
            None => (a.to_string(), default_port?),
        }
    };
    if port == 0 || !valid_host(&host) {
        return None;
    }
    Some((host, port))
}

pub fn parse_target(head: &[u8]) -> Option<Target> {
    let text = std::str::from_utf8(head).ok()?;
    let line = text.split("\r\n").next()?;
    let mut parts = line.split(' ');
    let (method, uri, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    if !method.bytes().all(|b| b.is_ascii_uppercase()) || method.is_empty() {
        return None;
    }
    if method == "CONNECT" {
        let (host, port) = split_authority(uri, None)?;
        return Some(Target::Connect { host, port });
    }
    let rest = uri.strip_prefix("http://").or_else(|| uri.strip_prefix("HTTP://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return None; // no userinfo
    }
    let (host, port) = split_authority(authority, Some(80))?;
    Some(Target::Http { method: method.into(), host, port, path: path.into(), version: version.into() })
}

/// The head to send upstream for a plain-HTTP request: origin-form request line, hop-by-hop
/// and proxy headers dropped, a Host header guaranteed, and Connection: close (one request
/// per connection keeps the policy check on every request).
pub fn rewrite_http_head(head: &[u8], t: &Target) -> Option<Vec<u8>> {
    let Target::Http { method, host, port, path, version } = t else { return None };
    let text = std::str::from_utf8(head).ok()?;
    let mut out = format!("{method} {path} {version}\r\n");
    let mut has_host = false;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let name = line.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
        match name.as_str() {
            "connection" | "proxy-connection" | "keep-alive" | "proxy-authorization" | "proxy-authenticate"
            | "te" | "trailer" | "upgrade" => continue,
            "host" => has_host = true,
            _ => {}
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !has_host {
        if *port == 80 {
            out.push_str(&format!("Host: {host}\r\n"));
        } else {
            out.push_str(&format!("Host: {host}:{port}\r\n"));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    Some(out.into_bytes())
}

pub fn response(code: u16, reason: &str, detail: &str) -> Vec<u8> {
    let body = format!("asb-proxy: {detail}\n");
    format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect() {
        assert_eq!(parse_target(b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n"),
                   Some(Target::Connect { host: "github.com".into(), port: 443 }));
        assert_eq!(parse_target(b"CONNECT [2606:4700::1]:443 HTTP/1.1\r\n\r\n"),
                   Some(Target::Connect { host: "2606:4700::1".into(), port: 443 }));
        assert_eq!(parse_target(b"CONNECT github.com HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_target(b"CONNECT a b:1 HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn parses_absolute_http() {
        let t = parse_target(b"GET http://example.com/a?b=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(t, Target::Http { method: "GET".into(), host: "example.com".into(), port: 80,
                                     path: "/a?b=1".into(), version: "HTTP/1.1".into() });
        let t = parse_target(b"POST http://example.com:8080 HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(t.port(), 8080);
        assert_eq!(parse_target(b"GET /relative HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_target(b"GET https://example.com/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_target(b"GET http://user@example.com/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_target(b"get http://example.com/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_target(b"GET http://exa mple.com/ HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn rewrites_plain_http() {
        let head = b"GET http://example.com/x HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
        let t = parse_target(head).unwrap();
        let out = String::from_utf8(rewrite_http_head(head, &t).unwrap()).unwrap();
        assert_eq!(out, "GET /x HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\nConnection: close\r\n\r\n");
        let head = b"GET http://example.com:8080/ HTTP/1.1\r\n\r\n";
        let t = parse_target(head).unwrap();
        let out = String::from_utf8(rewrite_http_head(head, &t).unwrap()).unwrap();
        assert!(out.contains("Host: example.com:8080\r\n"));
    }

    #[test]
    fn reads_head_and_keeps_extra_bytes() {
        let data = b"CONNECT a.com:443 HTTP/1.1\r\n\r\nEXTRA".to_vec();
        let (head, rest) = read_head(&mut &data[..]).unwrap();
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(rest, b"EXTRA");
        let big = vec![b'a'; MAX_HEAD + 10];
        assert!(read_head(&mut &big[..]).is_err());
    }
}
