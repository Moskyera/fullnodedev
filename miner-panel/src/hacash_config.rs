use std::path::{Path, PathBuf};

use field::Address;

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
    work_dir.join("..").join("..").join("hacash.config.ini")
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

#[derive(Clone, Default)]
pub struct DiamondMinerSettings {
    pub reward: String,
    pub bid_password: String,
    pub bid_min: String,
    pub bid_max: String,
    pub bid_step: String,
}

pub fn read_diamond_miner(path: &Path) -> DiamondMinerSettings {
    let Ok(content) = std::fs::read_to_string(path) else {
        return DiamondMinerSettings {
            bid_min: "1:0".to_string(),
            bid_max: "31:0".to_string(),
            bid_step: "0:5".to_string(),
            ..Default::default()
        };
    };
    DiamondMinerSettings {
        reward: read_section_key(&content, "diamondminer", "reward"),
        bid_password: read_section_key(&content, "diamondminer", "bid_password"),
        bid_min: {
            let v = read_section_key(&content, "diamondminer", "bid_min");
            if v.is_empty() { "1:0".into() } else { v }
        },
        bid_max: {
            let v = read_section_key(&content, "diamondminer", "bid_max");
            if v.is_empty() { "31:0".into() } else { v }
        },
        bid_step: {
            let v = read_section_key(&content, "diamondminer", "bid_step");
            if v.is_empty() { "0:5".into() } else { v }
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

pub fn write_miner_reward(path: &Path, wallet: &str) -> std::io::Result<()> {
    let wallet = wallet.trim();
    let content = read_or_empty(path);
    let updated = upsert_miner_reward(&content, wallet);
    write_config(path, &updated)
}

pub fn write_diamond_miner(path: &Path, wallet: &str, d: &DiamondMinerSettings) -> std::io::Result<()> {
    let content = read_or_empty(path);
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
    updated = upsert_section_fields(
        &updated,
        "miner",
        &[("enable", "false")],
    );
    write_config(path, &updated)
}

pub fn write_hac_miner_only(path: &Path, wallet: &str) -> std::io::Result<()> {
    let content = read_or_empty(path);
    let mut updated = upsert_miner_reward(&content, wallet);
    updated = upsert_section_fields(&updated, "diamondminer", &[("enable", "false")]);
    write_config(path, &updated)
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn write_config(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
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
            let key = line
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase();
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
            let trimmed = line.trim();
            if trimmed.starts_with("reward") {
                *line = format!("reward = {wallet}");
                has_reward = true;
            } else if trimmed.starts_with("enable") {
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
}