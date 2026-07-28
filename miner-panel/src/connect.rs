use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::i18n::{Lang, load_lang, strings};

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
/// Hacash worker protocol is just `connect = host:port`, with no separate pool
/// auth/stratum layer, so any node or pool that speaks this API is reachable by
/// pointing `connect` at it. New pools can therefore be added without
/// rebuilding the panel: drop a `pools.json` next to the exe (see
/// [`load_pool_directory`]) and they appear in the dropdown.
///
/// `note` is advertising copy, and this panel cannot verify a third party's
/// payout scheme, fee or minimum: it has never connected to most of these
/// entries. So a note here states only what the panel itself can stand behind.
/// The pool's real terms are read from the pool at run time (`/terms`) and
/// shown on the dashboard; that, and not this list, is where a miner learns
/// what a pool pays.
///
/// HBIT is the one entry whose terms the panel can actually check, because
/// HBIT is the pool this project builds and it answers `/terms` and
/// `/earnings`: the dashboard shows its real scheme, fee and minimum, and this
/// miner's own paid total, read live from the pool. For every other entry here
/// the panel has no such source, so it says nothing about what they pay. Being
/// ours is not a substitute for having connected, so the HBIT entry ships
/// unverified with no address, exactly like the others.
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

/// The pools that ship with the panel, in the panel's own language. Always
/// present, even offline. Community payout pools hand out their `host:port`
/// through a web config generator, so we cannot hard-code a verified address;
/// the user pastes it (or we publish it later via `pools.json`, with no
/// rebuild).
///
/// HBIT, the pool this project builds and supports, is first. It ships with an
/// empty `connect` because no HBIT address is published anywhere in this
/// repository, and an address invented here is one a miner would paste in and
/// send real hashrate to. An operator publishes theirs through `pools.json`,
/// or the miner asks them for it.
///
/// No entry may promise a payout scheme, a fee or a minimum, HBIT's included.
/// The panel has never connected to any of these services, so it cannot know
/// those numbers; see [`PoolInfo`] for where they now come from instead. An
/// earlier note here advertised "PROP payouts, low fee, small minimum", which
/// described neither the third-party pool it named nor any pool in this
/// repository.
///
/// Names are NOT translated: `pools.json` overrides a built-in by matching its
/// `name`, so a name that changed with the language would silently stop
/// matching an operator's published file. Only the HBIT note is translated so
/// far; the four notes below shipped in English and translating them is a
/// separate pass, not part of adding HBIT.
pub fn builtin_pools_in(lang: Lang) -> Vec<PoolInfo> {
    vec![
        PoolInfo::simple("HBIT pool", "", strings(lang).pool_dir_hbit_note, ""),
        PoolInfo::simple(
            "Custom pool / node",
            "",
            "Enter any host:port that runs the Hacash miner API (a pool or a shared full node).",
            "",
        ),
        PoolInfo::simple(
            "LAN full node / cluster",
            "192.168.1.10:8080",
            "Point every PC on your network at one full node; their hashrate adds up. Rewards go to that node's wallet, not to each PC.",
            "",
        ),
        PoolInfo::simple(
            "Hacash.Diamonds pool",
            "",
            "Third-party pool, not verified by this panel. Get your host:port from the pool page, then paste it above.",
            "https://www.hacash.diamonds/pool",
        ),
        PoolInfo::simple(
            "Hacash Community (HACPool)",
            "",
            "Third-party pool, not verified by this panel. Get your host:port from the pool site, then paste it above.",
            "https://pool.hacash.community",
        ),
        PoolInfo::simple(
            "HacashPool.com",
            "",
            "Third-party pool, not verified by this panel. Get your host:port from the pool site, then paste it above.",
            "https://hacashpool.com",
        ),
    ]
}

/// Words a directory note may not use about a service this panel has never
/// connected to. A scheme, a fee level or a minimum is a claim about somebody
/// else's software, and the panel has no way to check any of them; the
/// dashboard reads the real terms from the pool instead.
const UNBACKED_NOTE_CLAIMS: [&str; 8] = [
    "prop",
    "pplns",
    "pps",
    "solo payouts",
    "low fee",
    "no fee",
    "zero fee",
    "small minimum",
];

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

/// Neutral replacement for a note that promised something the panel cannot
/// check.
pub const UNVERIFIED_TERMS_NOTE: &str = "Payout terms are not verified by this panel. The dashboard shows the pool's own terms once you connect.";

/// Refuse to repeat a payout claim about a service the panel has never checked.
/// `pools.json` is an unsigned data file that anyone can drop next to the exe,
/// so a note promising "PROP payouts, low fee, small minimum" would put words
/// in the software's mouth. The address, name and link are kept; only the
/// unbacked sentence is replaced.
fn backed_note(note: &str) -> String {
    let lowered = note.to_ascii_lowercase();
    if UNBACKED_NOTE_CLAIMS
        .iter()
        .any(|claim| lowered.contains(claim))
    {
        return UNVERIFIED_TERMS_NOTE.to_string();
    }
    note.to_string()
}

/// Build the pool directory in the language the panel is running in, which is
/// the one saved next to it (`miner-panel.lang`, the same file the rest of the
/// UI reads). See [`load_pool_directory_in`] for what merging does.
pub fn load_pool_directory(dir: &Path) -> Vec<PoolInfo> {
    load_pool_directory_in(dir, load_lang(dir))
}

/// Build the pool directory: the built-in list, then merge an optional
/// `pools.json` sitting next to the panel. Entries whose `name` matches a
/// built-in override it (so a verified address can be published for a known
/// pool); new names are appended. A fresh pool therefore appears in the panel
/// by shipping/downloading a `pools.json`, with no rebuild required. A missing
/// or malformed file simply falls back to the built-ins.
pub fn load_pool_directory_in(dir: &Path, lang: Lang) -> Vec<PoolInfo> {
    let mut pools = builtin_pools_in(lang);
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
            note: backed_note(&e.note),
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
/// host: only that this machine can open the socket. Returns the elapsed
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

        // No pools.json -> built-ins only, HBIT first.
        let base = load_pool_directory(&dir);
        assert_eq!(base[0].name, "HBIT pool");
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
        assert_eq!(merged[0].name, "HBIT pool");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_reachable_rejects_invalid_address() {
        assert!(probe_reachable("", 100).is_err());
        assert!(probe_reachable("not-a-host-port", 100).is_err());
    }

    #[test]
    fn no_builtin_entry_promises_a_payout_scheme_a_fee_or_a_minimum() {
        // One entry used to read "Community pool: PROP payouts, low fee, small
        // minimum". The panel has never connected to that pool, and the pool
        // this project ships pays PPLNS, so the sentence was unbacked either
        // way. Nothing in this list may make that kind of claim again.
        for pool in builtin_pools_in(Lang::En) {
            let note = pool.note.to_ascii_lowercase();
            for claim in UNBACKED_NOTE_CLAIMS {
                assert!(
                    !note.contains(claim),
                    "{} claims '{claim}' the panel cannot check: {}",
                    pool.name,
                    pool.note
                );
            }
            assert!(
                !pool.note.contains('\u{2014}'),
                "{} uses an em dash",
                pool.name
            );
        }
    }

    #[test]
    fn every_third_party_entry_says_it_is_unverified() {
        // An entry with no address of its own is a name the user has to trust.
        // Say plainly that the panel has not checked it.
        for pool in builtin_pools_in(Lang::En) {
            if pool.connect.is_empty() && !pool.url.is_empty() {
                assert!(
                    pool.note.to_ascii_lowercase().contains("not verified"),
                    "{} must say it is unverified: {}",
                    pool.name,
                    pool.note
                );
                assert!(!pool.verified, "{} was never connected to", pool.name);
            }
        }
    }

    #[test]
    fn hbit_is_first_and_ships_with_no_address_and_no_tick() {
        let pools = builtin_pools_in(Lang::En);
        let hbit = &pools[0];
        assert_eq!(hbit.name, "HBIT pool");
        // No HBIT host:port is published anywhere in this repository. An
        // invented one is an address a miner would paste in and point real
        // hashrate at, so it stays empty until an operator publishes theirs
        // through pools.json.
        assert!(
            hbit.connect.is_empty(),
            "HBIT must not ship an invented address"
        );
        assert!(hbit.url.is_empty(), "HBIT must not ship an invented link");
        // Being ours is not the same as having been reached: the tick in the
        // dropdown means the panel connected to that endpoint, and it has not.
        assert!(
            !hbit.verified,
            "the panel has never connected to an HBIT address"
        );
        assert!(hbit.nonce_max.is_none() && hbit.notice_wait.is_none());
    }

    #[test]
    fn the_hbit_note_states_only_what_the_panel_can_check_in_every_language() {
        for lang in Lang::ALL {
            let pools = builtin_pools_in(lang);
            let note = &pools[0].note;
            assert!(!note.trim().is_empty(), "{} has no HBIT note", lang.code());
            assert!(
                !note.contains('\u{2014}'),
                "{} uses an em dash: {note}",
                lang.code()
            );
            let lowered = note.to_ascii_lowercase();
            for claim in UNBACKED_NOTE_CLAIMS {
                assert!(
                    !lowered.contains(claim),
                    "{} claims '{claim}' about HBIT: {note}",
                    lang.code()
                );
            }
            // No digits: an address, a fee or a minimum typed into this list is
            // a number the panel cannot check, and the dashboard reads all
            // three from the pool itself.
            assert!(
                !note.chars().any(|c| c.is_ascii_digit()),
                "{} puts a number in the HBIT note: {note}",
                lang.code()
            );
            // It has to tell the miner where the address actually comes from.
            assert!(
                note.contains("host:port"),
                "{} must say where to get the address: {note}",
                lang.code()
            );
        }
    }

    #[test]
    fn directory_names_are_identical_in_every_language() {
        // pools.json overrides a built-in by matching its name, so a name that
        // changed with the language would silently stop matching an operator's
        // published file and append a duplicate entry instead.
        let en: Vec<String> = builtin_pools_in(Lang::En)
            .into_iter()
            .map(|p| p.name)
            .collect();
        for lang in Lang::ALL {
            let names: Vec<String> = builtin_pools_in(lang).into_iter().map(|p| p.name).collect();
            assert_eq!(names, en, "{} renamed a directory entry", lang.code());
        }
    }

    #[test]
    fn an_operator_publishes_the_hbit_address_through_pools_json() {
        // The panel ships no HBIT address, so this is the supported way one
        // reaches a miner: drop the file next to the exe, no rebuild.
        let dir = std::env::temp_dir().join(format!(
            "hacash-hbitdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pools.json"),
            r#"[{"name":"HBIT pool","connect":"1.2.3.4:9777","verified":true}]"#,
        )
        .unwrap();

        let pools = load_pool_directory(&dir);
        assert_eq!(pools[0].name, "HBIT pool");
        assert_eq!(pools[0].connect, "1.2.3.4:9777");
        assert!(pools[0].verified);
        assert_eq!(pools.len(), builtin_pools_in(Lang::En).len());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_downloaded_pools_json_cannot_reintroduce_a_payout_promise() {
        let dir = std::env::temp_dir().join(format!(
            "hacash-poolnote-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pools.json"),
            r#"[
              {"name":"Promising Pool","connect":"1.2.3.4:8080",
               "note":"PROP payouts, low fee, small minimum","verified":true}
            ]"#,
        )
        .unwrap();

        let pools = load_pool_directory(&dir);
        let entry = pools.iter().find(|p| p.name == "Promising Pool").unwrap();
        assert_eq!(entry.note, UNVERIFIED_TERMS_NOTE);
        // The address and the rest of the entry are untouched: only the claim
        // the panel cannot stand behind is replaced.
        assert_eq!(entry.connect, "1.2.3.4:8080");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_note_that_claims_nothing_is_left_exactly_as_written() {
        let plain = "Get your host:port from the pool site, then paste it above.";
        assert_eq!(backed_note(plain), plain);
        assert_eq!(backed_note(""), "");
        // The check is case insensitive, so shouting the claim does not slip
        // through.
        assert_eq!(backed_note("PPLNS pool"), UNVERIFIED_TERMS_NOTE);
        assert_eq!(backed_note("Pool with No Fee"), UNVERIFIED_TERMS_NOTE);
    }
}
