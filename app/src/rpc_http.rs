//! Shared worker to fullnode RPC helpers.

use std::io::Read;
use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::HeaderValue;

/// Hard cap on a single fullnode JSON body (prevents hang/OOM from huge replies).
pub const MAX_RPC_BODY_BYTES: u64 = 2 * 1024 * 1024;
pub const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the one shared client used by miners. All ordinary requests inherit a
/// deadline, while the long-poll endpoint can opt into a longer per-request one.
pub fn build_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .no_proxy()
        .connect_timeout(RPC_CONNECT_TIMEOUT)
        .timeout(RPC_REQUEST_TIMEOUT)
        .build()
}

pub fn apply_api_token(mut req: RequestBuilder, api_token: &str) -> RequestBuilder {
    let token = api_token.trim();
    if token.is_empty() {
        return req;
    }
    if let Ok(v) = HeaderValue::from_str(token) {
        req = req.header("x-api-token", v);
    }
    req
}

pub fn get_text(
    client: &Client,
    url: &str,
    api_token: &str,
    timeout: Option<Duration>,
) -> Result<String, String> {
    let mut req = apply_api_token(client.get(url), api_token);
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    read_body_limited(resp)
}

pub fn post_text(
    client: &Client,
    url: &str,
    api_token: &str,
    body: Vec<u8>,
) -> Result<String, String> {
    let req = apply_api_token(client.post(url).body(body), api_token);
    let resp = req.send().map_err(|e| e.to_string())?;
    read_body_limited(resp)
}

fn read_limited<R: Read>(reader: R, declared_length: Option<u64>) -> Result<String, String> {
    if let Some(len) = declared_length {
        if len > MAX_RPC_BODY_BYTES {
            return Err(format!(
                "RPC response too large ({len} bytes, max {MAX_RPC_BODY_BYTES})"
            ));
        }
    }
    let mut limited = reader.take(MAX_RPC_BODY_BYTES.saturating_add(1));
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .map_err(|e| format!("RPC body read failed: {e}"))?;
    if buf.len() as u64 > MAX_RPC_BODY_BYTES {
        return Err(format!("RPC response exceeded {MAX_RPC_BODY_BYTES} bytes"));
    }
    String::from_utf8(buf).map_err(|e| format!("RPC body is not UTF-8: {e}"))
}

pub fn read_body_limited(resp: Response) -> Result<String, String> {
    let declared_length = resp.content_length();
    read_limited(resp, declared_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn secure_client_builds() {
        let client = build_client().expect("bounded RPC client");
        let _ = apply_api_token(client.get("http://127.0.0.1/"), "");
    }

    #[test]
    fn exact_body_limit_is_accepted() {
        let bytes = vec![b'x'; MAX_RPC_BODY_BYTES as usize];
        let body = read_limited(Cursor::new(bytes), Some(MAX_RPC_BODY_BYTES)).unwrap();
        assert_eq!(body.len() as u64, MAX_RPC_BODY_BYTES);
    }

    #[test]
    fn body_over_limit_is_rejected_without_content_length() {
        let bytes = vec![b'x'; MAX_RPC_BODY_BYTES as usize + 1];
        assert!(read_limited(Cursor::new(bytes), None).is_err());
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_reading() {
        let declared = MAX_RPC_BODY_BYTES + 1;
        assert!(read_limited(Cursor::new(Vec::<u8>::new()), Some(declared)).is_err());
    }

    #[test]
    fn non_utf8_body_is_rejected() {
        assert!(read_limited(Cursor::new([0xff]), None).is_err());
    }
}
