#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectMode {
    /// Local hacash.exe fullnode (solo mining, rewards to your wallet).
    Solo,
    /// Remote server with Hacash miner RPC API (pool or shared fullnode).
    Pool,
}

impl ConnectMode {
    pub fn from_u8(v: u8) -> ConnectMode {
        if v == 1 {
            ConnectMode::Pool
        } else {
            ConnectMode::Solo
        }
    }
}

pub const SOLO_DEFAULT: &str = "127.0.0.1:8080";

/// Pools / services that expose the same miner HTTP RPC as a fullnode
/// (`/query/miner/pending`, `/query/miner/notice`, `/submit/miner/success`).
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

/// CUDA-only miners (e.g. hacashdot) use a different protocol — not compatible.
pub const POOL_CUDA_NOTE: &str =
    "RPC pool = same API as fullnode. CUDA pool miners (hacashdot) are not supported here.";