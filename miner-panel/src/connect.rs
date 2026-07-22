#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectMode {
    /// Local hacash.exe fullnode (solo mining, rewards to your wallet).
    Solo,
    /// Remote server with Hacash miner RPC API (pool or shared fullnode).
    Pool,
}

impl ConnectMode {
    pub fn for_connect(connect: &str) -> ConnectMode {
        if is_local_connect(connect) {
            ConnectMode::Solo
        } else {
            ConnectMode::Pool
        }
    }
}

pub const SOLO_DEFAULT: &str = "127.0.0.1:8080";

pub fn normalize_connect(input: &str) -> Result<String, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("connection address is empty".into());
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Err("HTTPS is not supported by the Hacash miner RPC".into());
    }
    let without_scheme = if lower.starts_with("http://") {
        &raw[7..]
    } else {
        raw
    };
    if without_scheme.contains('/') || without_scheme.contains('?') || without_scheme.contains('#')
    {
        return Err("use only host:port, without a URL path".into());
    }

    let (host, port_text) = if without_scheme.starts_with('[') {
        let close = without_scheme
            .find(']')
            .ok_or_else(|| "invalid bracketed IPv6 address".to_string())?;
        let host = &without_scheme[..=close];
        let rest = without_scheme
            .get(close + 1..)
            .ok_or_else(|| "missing RPC port".to_string())?;
        let port = rest
            .strip_prefix(':')
            .ok_or_else(|| "missing RPC port".to_string())?;
        (host, port)
    } else {
        let (host, port) = without_scheme
            .rsplit_once(':')
            .ok_or_else(|| "connection must be host:port".to_string())?;
        if host.contains(':') {
            return Err("IPv6 addresses must use brackets, for example [::1]:8080".into());
        }
        (host, port)
    };

    if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
        return Err("invalid RPC host".into());
    }
    let port: u16 = port_text
        .parse()
        .map_err(|_| "RPC port must be between 1 and 65535".to_string())?;
    if port == 0 {
        return Err("RPC port must be between 1 and 65535".into());
    }
    Ok(format!("{}:{}", host.trim(), port))
}

pub fn connect_port(connect: &str) -> Option<u16> {
    let normalized = normalize_connect(connect).ok()?;
    normalized.rsplit_once(':')?.1.parse().ok()
}

pub fn is_local_connect(connect: &str) -> bool {
    let normalized = normalize_connect(connect).unwrap_or_else(|_| connect.trim().to_string());
    let host = normalized
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(&normalized)
        .trim_matches(['[', ']'])
        .trim();
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Pools / services that expose the same miner HTTP RPC as a fullnode
/// (/query/miner/pending, /query/miner/notice, /submit/miner/success).
#[derive(Clone)]
pub struct PoolPreset {
    pub label: &'static str,
    pub host: &'static str,
}

pub fn pool_presets() -> Vec<PoolPreset> {
    vec![
        PoolPreset {
            label: "Custom pool host",
            host: "",
        },
        PoolPreset {
            label: "LAN fullnode / cluster",
            host: "192.168.1.10:8080",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_local_and_remote_connect_modes() {
        assert_eq!(
            ConnectMode::for_connect("127.0.0.1:8080"),
            ConnectMode::Solo
        );
        assert_eq!(
            ConnectMode::for_connect("localhost:8080"),
            ConnectMode::Solo
        );
        assert_eq!(ConnectMode::for_connect("[::1]:8080"), ConnectMode::Solo);
        assert_eq!(
            ConnectMode::for_connect("192.168.1.10:8080"),
            ConnectMode::Pool
        );
        assert_eq!(
            ConnectMode::for_connect("pool.example:8080"),
            ConnectMode::Pool
        );
    }

    #[test]
    fn normalizes_beginner_friendly_http_input() {
        assert_eq!(
            normalize_connect(" http://localhost:8080 ").unwrap(),
            "localhost:8080"
        );
        assert_eq!(connect_port("[::1]:8081"), Some(8081));
    }

    #[test]
    fn rejects_paths_https_and_invalid_ports() {
        assert!(normalize_connect("https://pool.example:8080").is_err());
        assert!(normalize_connect("pool.example:8080/api").is_err());
        assert!(normalize_connect("pool.example:0").is_err());
        assert!(normalize_connect("pool.example").is_err());
    }
}
