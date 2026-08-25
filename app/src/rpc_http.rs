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

/// Turn a `connect` config value into the base URL every request is built on.
///
/// The miners used to paste this value straight into `format!("http://{}/...")`,
/// so a `connect = https://pool.example.org` produced
/// `http://https://pool.example.org/...` and the rig sat in a thirty-second
/// error loop for ever, with nothing anywhere naming the scheme as the cause.
/// Plaintext was not a choice an operator could decline.
///
/// It matters beyond convenience. The pool credits a share to whatever address
/// the query string names, and the replay key does not include it, so anyone who
/// can SEE a submission can resend the same nonces under their own address and
/// take the credit. Over the public internet that requires only a position on
/// the path; over TLS it requires breaking TLS.
///
/// Accepted, in order:
///   * `https://host[:port]` and `http://host[:port]`, used as given
///   * `host:port` - legacy, and still the common case, read as plain HTTP
///
/// Any trailing slash is trimmed so callers can concatenate a path that starts
/// with one without producing a double.
pub fn base_url(connect: &str) -> String {
    let c = connect.trim().trim_end_matches('/');
    if c.starts_with("http://") || c.starts_with("https://") {
        c.to_string()
    } else {
        format!("http://{c}")
    }
}

/// Is this base URL plaintext to somewhere that is not this machine?
///
/// `None` when there is nothing to say. The warning is deliberately about the
/// SHARE path rather than about privacy: an observer of plaintext traffic can
/// resend a miner's nonces under their own payout address, and the pool has no
/// way to tell the two apart.
pub fn plaintext_warning(base: &str) -> Option<String> {
    let rest = base.strip_prefix("http://")?;
    let host = rest
        .split('/')
        .next()
        .unwrap_or(rest)
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(rest);
    let local = host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("172.16.")
        || host.starts_with("172.17.")
        || host.starts_with("172.18.")
        || host.starts_with("172.19.")
        || host.starts_with("172.2")
        || host.starts_with("172.30.")
        || host.starts_with("172.31.");
    if local {
        return None;
    }
    Some(format!(
        "[connect] {base} is PLAIN HTTP to a host that is not this machine or your own network. \
         Anyone on the path can read this miner's submissions, and because a pool credits a \
         share to whatever payout address the request names, they can resend the same work \
         under THEIR address and be paid for it. If the pool offers https, use it: put \
         `connect = https://<host>` in the config."
    ))
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
    let status = resp.status();
    let declared_length = resp.content_length();
    let body = read_limited(resp, declared_length)?;
    // reqwest returns Ok for ANY HTTP status. A 5xx (or 408/429) is a transient
    // server/proxy failure, NOT an application reply - surface it as an error so
    // callers retry (e.g. a winning block submit) instead of mistaking a 502
    // error page for a response and dropping the block. Deterministic 4xx replies
    // are passed through so the caller can read the node's error body.
    if status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("upstream HTTP {status}: {snippet}"));
    }
    Ok(body)
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

    #[test]
    fn a_connect_value_becomes_a_base_url_without_nesting_one_scheme_in_another() {
        // The shape every existing config has, and it must keep working exactly
        // as it did: a bare host:port is plain HTTP.
        assert_eq!(base_url("127.0.0.1:8081"), "http://127.0.0.1:8081");
        assert_eq!(base_url("  127.0.0.1:8081  "), "http://127.0.0.1:8081");

        // The shape that used to be pasted INSIDE another scheme, producing
        // http://https://pool.example.org/... and leaving the rig in a
        // thirty-second error loop for ever with nothing naming the cause.
        assert_eq!(
            base_url("https://pool.example.org"),
            "https://pool.example.org"
        );
        assert_eq!(base_url("http://node.local:8080"), "http://node.local:8080");

        // A trailing slash would otherwise double up against paths that start
        // with one.
        assert_eq!(
            base_url("https://pool.example.org/"),
            "https://pool.example.org"
        );
        assert_eq!(base_url("127.0.0.1:8081/"), "http://127.0.0.1:8081");
    }

    #[test]
    fn plaintext_is_only_worth_warning_about_when_it_leaves_the_machine() {
        // Loopback and private ranges are the ordinary, correct setup: the node
        // or pool is on this box or this LAN, and there is no path to sit on.
        for quiet in [
            "http://127.0.0.1:8080",
            "http://localhost:9777",
            "http://192.168.1.50:9777",
            "http://10.0.0.9:9777",
        ] {
            assert!(
                plaintext_warning(quiet).is_none(),
                "{quiet} should not warn"
            );
        }
        // TLS anywhere is fine by definition.
        assert!(plaintext_warning("https://pool.example.org").is_none());

        // Plain HTTP to somebody else's machine is the case that matters, and
        // the warning has to say WHY - not "unencrypted" but "someone can be
        // paid for your work", which is what actually happens.
        let w = plaintext_warning("http://pool.example.org:9777")
            .expect("plain http to a remote pool must warn");
        assert!(w.contains("resend the same work"), "{w}");
        assert!(w.contains("https://"), "and say what to do instead: {w}");
    }
}
