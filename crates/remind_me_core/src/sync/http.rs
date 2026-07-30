//! A minimal hand-rolled HTTP/1.1 client for talking to a hub's peer
//! server — the same `std::net::TcpStream`-based approach already used for
//! the Ollama embedding client and the webhook/HTTP-API servers, rather
//! than pulling in an async HTTP client for two request shapes.
//!
//! `http://` only, matching the same simplifying choice `embedder.rs`'s
//! `OllamaEmbedder` already made: a sync deployment that needs TLS puts a
//! reverse proxy in front, the same way an Ollama deployment would.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct HttpError(pub String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for HttpError {}

struct ParsedUrl {
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, HttpError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| HttpError(format!("only http:// sync URLs are supported, got {url:?}")))?;
    let (authority, path_and_query) = match rest.find('/') {
        Some(pos) => (&rest[..pos], rest[pos..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = authority
        .split_once(':')
        .ok_or_else(|| HttpError(format!("sync URL has no port: {url:?}")))?;
    let port: u16 = port
        .parse()
        .map_err(|_| HttpError(format!("invalid port in sync URL: {url:?}")))?;
    Ok(ParsedUrl {
        host: host.to_string(),
        port,
        path_and_query,
    })
}

/// One HTTP request/response round-trip. `body: None` sends a bare
/// request line (used for `GET`); `Some(json)` sends it with a
/// `Content-Type: application/json` body (used for `POST`).
fn request(
    method: &str,
    url: &str,
    secret: &str,
    body: Option<&str>,
) -> Result<(u16, String), HttpError> {
    let parsed = parse_url(url)?;
    use std::net::ToSocketAddrs;
    let addr = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| HttpError(format!("cannot resolve sync host {:?}", parsed.host)))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| HttpError(format!("cannot reach {}: {}", url, e)))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let request_text = match body {
        Some(body) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {secret}\r\n\
             Content-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            method = method,
            path = parsed.path_and_query,
            host = parsed.host,
            secret = secret,
            len = body.len(),
            body = body,
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {secret}\r\n\
             Connection: close\r\n\r\n",
            method = method,
            path = parsed.path_and_query,
            host = parsed.host,
            secret = secret,
        ),
    };
    stream
        .write_all(request_text.as_bytes())
        .map_err(|e| HttpError(format!("writing to {}: {}", url, e)))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| HttpError(format!("reading from {}: {}", url, e)))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, response_body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| HttpError(format!("malformed HTTP response from {url}")))?;
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| HttpError(format!("malformed HTTP status line from {url}")))?;
    Ok((status, response_body.to_string()))
}

pub fn post_json(url: &str, secret: &str, body: &str) -> Result<(u16, String), HttpError> {
    request("POST", url, secret, Some(body))
}

pub fn get(url: &str, secret: &str) -> Result<(u16, String), HttpError> {
    request("GET", url, secret, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_splits_host_port_and_path_with_query() {
        let parsed = parse_url("http://example.com:8766/sync/pull?since=x&limit=5").unwrap();
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 8766);
        assert_eq!(parsed.path_and_query, "/sync/pull?since=x&limit=5");
    }

    #[test]
    fn parse_url_defaults_to_root_path() {
        let parsed = parse_url("http://example.com:8766").unwrap();
        assert_eq!(parsed.path_and_query, "/");
    }

    #[test]
    fn parse_url_rejects_a_url_with_no_port() {
        assert!(parse_url("http://example.com/foo").is_err());
    }

    #[test]
    fn parse_url_rejects_https() {
        assert!(parse_url("https://example.com:8766/foo").is_err());
    }
}
