use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// One selectable entry in the pool directory.
///
/// Pools / services that expose the same miner HTTP RPC as a fullnode
/// (/query/miner/pending, /query/miner/notice, /submit/miner/success). The base
/// Hacash worker protocol is just `connect = host:port` — there is no separate
/// pool auth/stratum layer — so any node or pool that speaks this API is
/// reachable by pointing `connect` at it. New pools can therefore be added
/// without rebuilding the panel: drop a `pools.json` next to the exe (see
/// [`load_pool_directory`]) and they appear in the dropdown.
#[derive(Clone, Debug, PartialEq)]
pub struct PoolInfo {
    /// Display name in the dropdown.
    pub name: String,
    /// host:port that speaks the miner API. Empty = the user must paste it
    /// (used for pools that hand out their address via a web config generator).
    pub connect: String,
    /// One-line guidance shown under the dropdown.
    pub note: String,
    /// Optional "learn more / get your address" link.
    pub url: String,
    /// True only for endpoints we actually connected to and verified.
    pub verified: bool,
    /// Optional per-pool worker overrides, applied when the pool is selected.
    /// `None` keeps the panel's current value.
    pub nonce_max: Option<u32>,
    pub notice_wait: Option<u64>,
}

impl PoolInfo {
    fn simple(name: &str, connect: &str, note: &str, url: &str) -> PoolInfo {
        PoolInfo {
            name: name.to_string(),
            connect: connect.to_string(),
            note: note.to_string(),
            url: url.to_string(),
            verified: false,
            nonce_max: None,
            notice_wait: None,
        }
    }
}

/// The pools that ship with the panel. Always present, even offline.
/// Community payout pools hand out their `host:port` through a web config
/// generator, so we cannot hard-code a verified address; the user pastes it
/// (or we publish it later via `pools.json`, with no rebuild).
pub fn builtin_pools() -> Vec<PoolInfo> {
    vec![
        PoolInfo::simple(
            "Custom pool / node",
            "",
            "Enter any host:port that runs the Hacash miner API (a pool or a shared full node).",
            "",
        ),
        PoolInfo::simple(
            "LAN full node / cluster",
            "192.168.1.10:8080",
            "Point every PC on your network at one full node; their hashrate adds up.",
            "",
        ),
        PoolInfo::simple(
            "Hacash.Diamonds pool",
            "",
            "Community pool. Get your host:port from the pool page, then paste it above.",
            "https://www.hacash.diamonds/pool",
        ),
        PoolInfo::simple(
            "Hacash Community (HACPool)",
            "",
            "Community pool: PROP payouts, low fee, small minimum. Get host:port from the pool site.",
            "https://pool.hacash.community",
        ),
        PoolInfo::simple(
            "HacashPool.com",
            "",
            "Community pool. Get your host:port from the pool site, then paste it above.",
            "https://hacashpool.com",
        ),
    ]
}

#[derive(serde::Deserialize)]
struct PoolJson {
    name: String,
    #[serde(default)]
    connect: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    nonce_max: Option<u32>,
    #[serde(default)]
    notice_wait: Option<u64>,
}

/// Build the pool directory: the built-in list, then merge an optional
/// `pools.json` sitting next to the panel. Entries whose `name` matches a
/// built-in override it (so a verified address can be published for a known
/// pool); new names are appended. A fresh pool therefore appears in the panel
/// by shipping/downloading a `pools.json` — no rebuild required. A missing or
/// malformed file simply falls back to the built-ins.
pub fn load_pool_directory(dir: &Path) -> Vec<PoolInfo> {
    let mut pools = builtin_pools();
    let Ok(raw) = std::fs::read_to_string(dir.join("pools.json")) else {
        return pools;
    };
    let Ok(entries) = serde_json::from_str::<Vec<PoolJson>>(&raw) else {
        return pools;
    };
    for e in entries {
        if e.name.trim().is_empty() {
            continue;
        }
        let info = PoolInfo {
            name: e.name,
            connect: e.connect,
            note: e.note,
            url: e.url,
            verified: e.verified,
            nonce_max: e.nonce_max,
            notice_wait: e.notice_wait,
        };
        match pools
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&info.name))
        {
            Some(slot) => *slot = info,
            None => pools.push(info),
        }
    }
    pools
}

/// Best-effort reachability check: resolve `connect` (host:port) and open a TCP
/// connection with a short timeout. Confirms the endpoint is listening and
/// reachable FROM HERE. It cannot prove external/NAT reachability of a pool you
/// host — only that this machine can open the socket. Returns the elapsed
/// milliseconds on success, or a human-readable error.
pub fn probe_reachable(connect: &str, timeout_ms: u64) -> Result<u128, String> {
    let addr = normalize_connect(connect)?;
    let socket_addrs = addr
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr}: {e}"))?;
    let started = Instant::now();
    let mut last_err = format!("no address resolved for {addr}");
    for sa in socket_addrs {
        match TcpStream::connect_timeout(&sa, Duration::from_millis(timeout_ms)) {
            Ok(_) => return Ok(started.elapsed().as_millis()),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
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

    #[test]
    fn pool_directory_merges_and_overrides_pools_json() {
        let dir = std::env::temp_dir().join(format!(
            "hacash-pooldir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // No pools.json -> built-ins only, "Custom" first.
        let base = load_pool_directory(&dir);
        assert_eq!(base[0].name, "Custom pool / node");
        let base_len = base.len();

        std::fs::write(
            dir.join("pools.json"),
            r#"[
              {"name":"Hacash.Diamonds pool","connect":"1.2.3.4:8080","verified":true},
              {"name":"Fresh Community Pool","connect":"5.6.7.8:3333","notice_wait":30}
            ]"#,
        )
        .unwrap();
        let merged = load_pool_directory(&dir);

        // Same name -> overridden in place (new connect + verified flag).
        let diamonds = merged
            .iter()
            .find(|p| p.name == "Hacash.Diamonds pool")
            .unwrap();
        assert_eq!(diamonds.connect, "1.2.3.4:8080");
        assert!(diamonds.verified);

        // New name -> appended, with its optional override parsed.
        let fresh = merged
            .iter()
            .find(|p| p.name == "Fresh Community Pool")
            .unwrap();
        assert_eq!(fresh.connect, "5.6.7.8:3333");
        assert_eq!(fresh.notice_wait, Some(30));

        assert_eq!(merged.len(), base_len + 1);
        assert_eq!(merged[0].name, "Custom pool / node");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_reachable_rejects_invalid_address() {
        assert!(probe_reachable("", 100).is_err());
        assert!(probe_reachable("not-a-host-port", 100).is_err());
    }
}
