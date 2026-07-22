#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_FILE_NONCE: AtomicU64 = AtomicU64::new(0);

use field::{Address, Amount};

pub fn find_hacash_config(work_dir: &Path) -> PathBuf {
    let candidates = [
        work_dir.join("hacash.config.ini"),
        work_dir.join("..").join("hacash.config.ini"),
        work_dir.join("..").join("..").join("hacash.config.ini"),
    ];
    for c in &candidates {
        if c.is_file() {
            return c.canonicalize().unwrap_or(c.clone());
        }
    }
    // A packaged release must never write outside its extracted folder. The
    // development layout is already handled by the candidates above.
    work_dir.join("hacash.config.ini")
}

fn strip_comment(value: &str) -> String {
    value.split(';').next().unwrap_or(value).trim().to_string()
}

pub fn read_reward_wallet(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    parse_miner_reward(&content)
}

fn parse_miner_reward(content: &str) -> String {
    let mut in_miner = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_miner = trimmed.eq_ignore_ascii_case("[miner]");
            continue;
        }
        if in_miner {
            let key = trimmed.split('=').next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case("reward") {
                if let Some((_, val)) = line.split_once('=') {
                    return strip_comment(val);
                }
            }
        }
    }
    String::new()
}

pub fn validate_wallet(wallet: &str) -> Result<(), String> {
    let trimmed = wallet.trim();
    if trimmed.is_empty() {
        return Err("empty".to_string());
    }
    Address::from_readable(trimmed).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn validate_hacd_wallet(wallet: &str) -> Result<(), String> {
    let trimmed = wallet.trim();
    if trimmed.is_empty() {
        return Err("empty".to_string());
    }
    let address = Address::from_readable(trimmed).map_err(|e| e.to_string())?;
    if !address.is_privakey() {
        return Err("HACD rewards require a PRIVAKEY address".to_string());
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct DiamondMinerSettings {
    pub reward: String,
    pub bid_password: String,
    pub bid_min: String,
    pub bid_max: String,
    pub bid_step: String,
}

pub fn validate_diamond_settings(d: &DiamondMinerSettings) -> Result<(), String> {
    let min = Amount::from(d.bid_min.trim()).map_err(|e| format!("invalid minimum bid: {e}"))?;
    let max = Amount::from(d.bid_max.trim()).map_err(|e| format!("invalid maximum bid: {e}"))?;
    let _step = Amount::from(d.bid_step.trim()).map_err(|e| format!("invalid bid step: {e}"))?;
    let zero = Amount::zero();
    if min < zero || max < zero {
        return Err("diamond bid values cannot be negative".into());
    }

    if min > max {
        return Err("minimum diamond bid cannot exceed maximum bid".into());
    }
    Ok(())
}

pub fn read_diamond_miner(path: &Path) -> DiamondMinerSettings {
    let Ok(content) = std::fs::read_to_string(path) else {
        return DiamondMinerSettings {
            // Bid amounts are plain HAC (mei/decimal): "1" = 1 HAC, "0.5" = half a
            // HAC. The colon form "X:Y" is coin(mantissa X, unit Y), so "1:0" is
            // 10^-248 HAC (dust), NOT 1 HAC: do not use it here.
            bid_min: "1".to_string(),
            bid_max: "31".to_string(),
            bid_step: "0.5".to_string(),
            ..Default::default()
        };
    };
    DiamondMinerSettings {
        reward: read_section_key(&content, "diamondminer", "reward"),
        bid_password: read_section_key(&content, "diamondminer", "bid_password"),
        bid_min: {
            let v = read_section_key(&content, "diamondminer", "bid_min");
            if v.is_empty() { "1".into() } else { v }
        },
        bid_max: {
            let v = read_section_key(&content, "diamondminer", "bid_max");
            if v.is_empty() { "31".into() } else { v }
        },
        bid_step: {
            let v = read_section_key(&content, "diamondminer", "bid_step");
            if v.is_empty() { "0.5".into() } else { v }
        },
    }
}

fn read_section_key(content: &str, section: &str, key: &str) -> String {
    let section_tag = format!("[{section}]");
    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed.eq_ignore_ascii_case(&section_tag);
            continue;
        }
        if in_section {
            let k = trimmed.split('=').next().unwrap_or("").trim();
            if k.eq_ignore_ascii_case(key) {
                if let Some((_, val)) = line.split_once('=') {
                    return strip_comment(val);
                }
            }
        }
    }
    String::new()
}

pub fn write_diamond_miner(
    path: &Path,
    wallet: &str,
    d: &DiamondMinerSettings,
    rpc_port: Option<u16>,
) -> std::io::Result<()> {
    let content = ensure_mainnet_node_section(&read_or_empty(path));
    let mut updated = upsert_section_fields(
        &content,
        "diamondminer",
        &[
            ("enable", "true"),
            ("reward", wallet.trim()),
            ("bid_password", d.bid_password.trim()),
            ("bid_min", d.bid_min.trim()),
            ("bid_max", d.bid_max.trim()),
            ("bid_step", d.bid_step.trim()),
        ],
    );
    updated = upsert_section_fields(&updated, "miner", &[("enable", "false")]);
    if let Some(port) = rpc_port {
        let listen = port.to_string();
        updated = upsert_section_fields(
            &updated,
            "server",
            &[
                ("enable", "true"),
                ("listen", &listen),
                ("bind", "127.0.0.1"),
                ("diamond_form", "true"),
            ],
        );
    }
    write_config(path, &updated)
}

pub fn write_hac_miner_only(
    path: &Path,
    wallet: &str,
    rpc_port: Option<u16>,
) -> std::io::Result<()> {
    let content = ensure_mainnet_node_section(&read_or_empty(path));
    let mut updated = upsert_miner_reward(&content, wallet);
    updated = upsert_section_fields(&updated, "diamondminer", &[("enable", "false")]);
    if let Some(port) = rpc_port {
        let listen = port.to_string();
        updated = upsert_section_fields(
            &updated,
            "server",
            &[
                ("enable", "true"),
                ("listen", &listen),
                ("bind", "127.0.0.1"),
                ("diamond_form", "true"),
            ],
        );
    }
    write_config(path, &updated)
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Guarantee the fullnode joins MAINNET. Without a `[node]` section (boot nodes +
/// `not_find_nodes = false`) hacash starts an ISOLATED LOCAL chain: the height stays
/// near 0, so x16rs runs at repeat=1 (an inflated MH/s that is NOT real mining). The
/// panel writes hacash.config.ini for the reward wallet, so it must ensure `[node]` is
/// present — otherwise a config created before START-MAINNET.bat has run is off-network.
/// An existing `[node]` section is left untouched (respects a user/launcher setup).
fn ensure_mainnet_node_section(content: &str) -> String {
    let has_node = content
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("[node]"));
    if has_node {
        return content.to_string();
    }
    let node = "[node]\nname = rust_node\nlisten = 3337\nboots = 54.193.49.59:3337, 182.92.163.225:3337, 54.219.80.127:3337\nnot_find_nodes = false\nfast_sync = true\n\n";
    format!("{node}{content}")
}

fn write_config(path: &Path, content: &str) -> std::io::Result<()> {
    atomic_write_private(path, content)
}

/// Write to a private, uniquely named file in the destination directory and
/// replace the old config atomically. A crash can leave the old or new complete
/// file, never a partially written config.
pub(crate) fn atomic_write_private(path: &Path, content: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let (temporary_path, mut temporary_file) = (0..64)
        .find_map(|_| {
            let nonce = TEMP_FILE_NONCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), nonce));
            match options.open(&candidate) {
                Ok(file) => Some(Ok((candidate, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique temporary config file",
            )
        })?;

    let write_result = temporary_file
        .write_all(content.as_bytes())
        .and_then(|_| temporary_file.sync_all());
    drop(temporary_file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }

    if let Err(error) = atomic_replace(&temporary_path, path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }

    #[cfg(unix)]
    File::open(parent)?.sync_all()?;

    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(once(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    // SAFETY: both buffers are valid, NUL-terminated UTF-16 paths and remain
    // alive for the duration of the call.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn upsert_section_fields(content: &str, section: &str, fields: &[(&str, &str)]) -> String {
    let section_tag = format!("[{section}]");
    let mut lines: Vec<String> = if content.is_empty() {
        Vec::new()
    } else {
        content.lines().map(|l| l.to_string()).collect()
    };

    let section_start = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case(&section_tag));

    if let Some(start) = section_start {
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, l)| {
                let t = l.trim();
                t.starts_with('[') && t.ends_with(']')
            })
            .map(|(i, _)| i)
            .unwrap_or(lines.len());

        let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
        for line in &mut lines[start + 1..end] {
            let key = line.split('=').next().unwrap_or("").trim().to_lowercase();
            for (k, v) in fields {
                if key == k.to_lowercase() {
                    *line = format!("{k} = {v}");
                    present.insert(k.to_lowercase());
                }
            }
        }
        let mut insert_pos = start + 1;
        for (k, v) in fields {
            if !present.contains(&k.to_lowercase()) {
                lines.insert(insert_pos, format!("{k} = {v}"));
                insert_pos += 1;
            }
        }
    } else {
        if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            lines.push(String::new());
        }
        lines.push(section_tag);
        for (k, v) in fields {
            lines.push(format!("{k} = {v}"));
        }
    }

    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn upsert_miner_reward(content: &str, wallet: &str) -> String {
    let mut lines: Vec<String> = if content.is_empty() {
        Vec::new()
    } else {
        content.lines().map(|l| l.to_string()).collect()
    };

    let miner_start = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case("[miner]"));

    if let Some(start) = miner_start {
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, l)| {
                let t = l.trim();
                t.starts_with('[') && t.ends_with(']')
            })
            .map(|(i, _)| i)
            .unwrap_or(lines.len());

        let mut has_reward = false;
        let mut has_enable = false;
        for line in &mut lines[start + 1..end] {
            let key = line
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if key == "reward" {
                *line = format!("reward = {wallet}");
                has_reward = true;
            } else if key == "enable" {
                *line = "enable = true".to_string();
                has_enable = true;
            }
        }
        let mut insert_pos = start + 1;
        if !has_reward {
            lines.insert(insert_pos, format!("reward = {wallet}"));
            insert_pos += 1;
        }
        if !has_enable {
            lines.insert(insert_pos, "enable = true".to_string());
        }
    } else {
        if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            lines.push(String::new());
        }
        lines.push("[miner]".to_string());
        lines.push("enable = true".to_string());
        lines.push(format!("reward = {wallet}"));
    }

    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_existing_section() {
        let input = "[node]\nlisten = 3033\n\n[miner]\nenable = true\nreward = old\n";
        let out = upsert_miner_reward(input, "1NewWalletAddr");
        assert!(out.contains("reward = 1NewWalletAddr"));
        assert!(!out.contains("reward = old"));
    }

    #[test]
    fn upsert_new_section() {
        let out = upsert_miner_reward("", "3xHYiddmZUtgY92pq7gGDoyHiGE7K47b4X");
        assert!(out.contains("[miner]"));
        assert!(out.contains("reward = 3xHYiddmZUtgY92pq7gGDoyHiGE7K47b4X"));
    }

    #[test]
    fn upsert_is_case_insensitive_without_duplicates() {
        let input = "[miner]\nEnable = false\nReward = old\n";
        let out = upsert_miner_reward(input, "1NewWalletAddr");
        assert_eq!(out.matches("reward = 1NewWalletAddr").count(), 1);
        assert_eq!(out.matches("enable = true").count(), 1);
    }

    #[test]
    fn validates_diamond_bid_range() {
        let valid = DiamondMinerSettings {
            bid_min: "1".into(),
            bid_max: "31".into(),
            bid_step: "0.5".into(),
            ..Default::default()
        };
        assert!(validate_diamond_settings(&valid).is_ok());

        let mut invalid = valid;
        invalid.bid_min = "40".into();
        assert!(validate_diamond_settings(&invalid).is_err());
    }

    #[test]
    fn local_rpc_settings_are_written() {
        let path =
            std::env::temp_dir().join(format!("hacash-fullnode-config-{}.ini", std::process::id()));
        write_hac_miner_only(&path, "wallet", Some(8085)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert!(raw.contains("[server]"));
        assert!(raw.contains("listen = 8085"));
        assert!(raw.contains("bind = 127.0.0.1"));
        assert!(raw.contains("diamond_form = true"));
    }

    #[test]
    fn fresh_hac_and_hacd_configs_get_mainnet_node_section() {
        // A panel-written config on an empty file must join MAINNET. Without [node]
        // (boots + not_find_nodes=false) hacash starts an isolated LOCAL chain
        // (height ~0 -> x16rs repeat=1 -> inflated MH/s, not real mining).
        let path = std::env::temp_dir().join(format!("hacash-node-cfg-{}.ini", std::process::id()));

        let _ = std::fs::remove_file(&path);
        write_hac_miner_only(&path, "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v", Some(8080)).unwrap();
        let hac = std::fs::read_to_string(&path).unwrap();
        assert!(hac.contains("[node]"), "HAC config missing [node]:\n{hac}");
        assert!(hac.contains("not_find_nodes = false"), "{hac}");
        assert!(hac.contains("boots = 54.193.49.59:3337"), "{hac}");

        let _ = std::fs::remove_file(&path);
        write_diamond_miner(
            &path,
            "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v",
            &DiamondMinerSettings::default(),
            Some(8080),
        )
        .unwrap();
        let hacd = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(hacd.contains("[node]"), "HACD config missing [node]:\n{hacd}");
        assert!(hacd.contains("not_find_nodes = false"), "{hacd}");
    }

    #[test]
    fn existing_node_section_is_preserved() {
        // An existing [node] (from START-MAINNET.bat or a custom user setup) must not
        // be clobbered or duplicated when the panel rewrites the miner fields.
        let path =
            std::env::temp_dir().join(format!("hacash-keepnode-cfg-{}.ini", std::process::id()));
        std::fs::write(
            &path,
            "[node]\nlisten = 9999\nnot_find_nodes = false\n\n[miner]\nreward = old\n",
        )
        .unwrap();
        write_hac_miner_only(&path, "1NewWallet", Some(8080)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(raw.contains("listen = 9999"), "custom [node] listen lost:\n{raw}");
        assert_eq!(raw.matches("[node]").count(), 1, "duplicate [node]:\n{raw}");
    }

    #[test]
    fn config_write_replaces_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "hacash-atomic-config-{}-{}.ini",
            std::process::id(),
            TEMP_FILE_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        write_config(&path, "old").unwrap();
        write_config(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn config_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "hacash-private-config-{}-{}.ini",
            std::process::id(),
            TEMP_FILE_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        write_config(&path, "bid_password = secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_file(path);
        assert_eq!(mode, 0o600);
    }
}
