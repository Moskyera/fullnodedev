#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MiningKind {
    Hac,
    Hacd,
}

impl MiningKind {
    pub fn from_code(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "hacd" | "diamond" | "dia" => MiningKind::Hacd,
            _ => MiningKind::Hac,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            MiningKind::Hac => "hac",
            MiningKind::Hacd => "hacd",
        }
    }
}

pub fn load_mining_kind(work_dir: &std::path::Path) -> MiningKind {
    let path = work_dir.join("miner-panel.mode");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| MiningKind::from_code(&s))
        .unwrap_or(MiningKind::Hac)
}

pub fn save_mining_kind(work_dir: &std::path::Path, kind: MiningKind) {
    let path = work_dir.join("miner-panel.mode");
    let _ = std::fs::write(path, kind.code());
}