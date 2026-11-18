//! A minimal HTTP/1.1 JSON client for the ACDC board on this machine. Loopback only: command text never leaves it.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    host: String,
    port: u16,
    base: String,
}

impl Endpoint {
    /// Accepts `http://127.0.0.1:<port>/<base>` or `http://localhost:<port>/<base>`; anything else is refused.
    pub fn parse(url: &str) -> Result<Endpoint, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or("board url must start with http:// (the board runs on this machine)")?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = authority.rsplit_once(':').unwrap_or((authority, "80"));
        if host != "127.0.0.1" && host != "localhost" {
            return Err(format!(
                "board url host must be 127.0.0.1 or localhost, not {host}"
            ));
        }
        let port: u16 = port.parse().map_err(|_| "board url has a bad port")?;
        Ok(Endpoint {
            host: host.into(),
            port,
            base: format!("/{}", path.trim_end_matches('/')),
        })
    }
}

pub trait Transport {
    /// POSTs JSON to `base + path`; returns the decoded body of a 2xx answer, else the board's error sentence.
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, String>;
}

pub struct Http {
    pub endpoint: Endpoint,
    pub timeout: Duration,
}

impl Transport for Http {
    fn post(&mut self, path: &str, body: &Value) -> Result<Value, String> {
        let e = &self.endpoint;
        let addr = SocketAddr::from(([127, 0, 0, 1], e.port));
        let mut s = TcpStream::connect_timeout(&addr, self.timeout)
            .map_err(|err| format!("board unreachable: {err}"))?;
        s.set_read_timeout(Some(self.timeout)).ok();
        s.set_write_timeout(Some(self.timeout)).ok();
        let payload = body.to_string();
        let req = format!(
            "POST {}{} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             X-Board-Kind: other\r\nConnection: close\r\n\r\n{payload}",
            e.base.trim_end_matches('/'),
            path,
            e.host,
            e.port,
            payload.len()
        );
        s.write_all(req.as_bytes())
            .map_err(|err| format!("board write failed: {err}"))?;
        let mut raw = Vec::new();
        s.take(1 << 20)
            .read_to_end(&mut raw)
            .map_err(|err| format!("board read failed: {err}"))?;
        parse_response(&raw)
    }
}

fn parse_response(raw: &[u8]) -> Result<Value, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("board sent a malformed answer")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut body = raw[split + 4..].to_vec();
    let code: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or("board sent a malformed status line")?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        body = dechunk(&body).ok_or("board sent a malformed chunked body")?;
    }
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if (200..300).contains(&code) {
        Ok(value)
    } else {
        let msg = value["error"].as_str().unwrap_or("no message");
        Err(format!("board answered {code}: {msg}"))
    }
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&b[..eol]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        b = &b[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        if b.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_urls_are_accepted() {
        let e = Endpoint::parse("http://127.0.0.1:8787/api").unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.base.as_str()),
            ("127.0.0.1", 8787, "/api")
        );
        assert!(Endpoint::parse("http://localhost:9000/api/").is_ok());
        assert!(Endpoint::parse("https://127.0.0.1:8787/api").is_err());
        assert!(Endpoint::parse("http://example.com:8787/api").is_err());
        assert!(Endpoint::parse("http://127.0.0.1.example.com:80/api").is_err());
        assert!(Endpoint::parse("http://127.0.0.1:notaport/api").is_err());
    }

    #[test]
    fn answers_are_decoded() {
        let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 21\r\n\r\n{\"ok\":true,\"id\":\"t1\"}";
        assert_eq!(parse_response(ok).unwrap()["id"], "t1");
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n{\"ok\":true}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap()["ok"], true);
        let bad = b"HTTP/1.1 409 Conflict\r\n\r\n{\"error\":\"Task is held.\"}";
        assert_eq!(
            parse_response(bad).unwrap_err(),
            "board answered 409: Task is held."
        );
    }
}
