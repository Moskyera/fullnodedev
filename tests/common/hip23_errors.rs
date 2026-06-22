//! Stable HIP-23 error tokens for indexers (v1 string matching).
//! Maps on-chain error text to normalized codes — see `doc/HIP23_indexer_dictionary.md`.

/// Raw substrings observed from protocol execution (do not rename without semver).
pub mod substring {
    pub const HEIGHT_OUTSIDE_WINDOW: &str = "submitted in height between";
    pub const HEIGHT_INVALID_RANGE: &str = "start height cannot be greater";
    pub const BALANCE_BELOW_FLOOR: &str = "lower than floor";
    pub const CHAIN_NOT_ALLOWED: &str = "chain id check failed";
    pub const GUARD_ONLY_TOPOLOGY: &str = "all GUARD";
    pub const TEX_SETTLEMENT_FAIL: &str = "settlement check failed";
    pub const TEX_SIG_FAIL: &str = "signature verification failed";
    pub const GAS_NOT_INITIALIZED: &str = "gas not initialized";
    pub const DUPLICATE_TX: &str = "already exists";
    pub const PROTOCOL_FEE_MISMATCH: &str = "Protocol fee must be";
}

/// Normalized indexer codes (stable across HIP-23 v1.x).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hip23ErrorCode {
    HeightOutsideWindow,
    HeightInvalidRange,
    BalanceBelowFloor,
    ChainNotAllowed,
    GuardOnlyTopology,
    TexSettlementFail,
    TexSigFail,
    GasNotInitialized,
    DuplicateTx,
    ProtocolFeeMismatch,
    Unknown,
}

pub fn classify_error(err: &str) -> Hip23ErrorCode {
    use substring::*;
    if err.contains(HEIGHT_OUTSIDE_WINDOW) {
        Hip23ErrorCode::HeightOutsideWindow
    } else if err.contains(HEIGHT_INVALID_RANGE) {
        Hip23ErrorCode::HeightInvalidRange
    } else if err.contains(BALANCE_BELOW_FLOOR) {
        Hip23ErrorCode::BalanceBelowFloor
    } else if err.contains(CHAIN_NOT_ALLOWED) {
        Hip23ErrorCode::ChainNotAllowed
    } else if err.contains(GUARD_ONLY_TOPOLOGY) {
        Hip23ErrorCode::GuardOnlyTopology
    } else if err.contains(TEX_SETTLEMENT_FAIL) {
        Hip23ErrorCode::TexSettlementFail
    } else if err.contains(TEX_SIG_FAIL) {
        Hip23ErrorCode::TexSigFail
    } else if err.contains(GAS_NOT_INITIALIZED) {
        Hip23ErrorCode::GasNotInitialized
    } else if err.contains(DUPLICATE_TX) {
        Hip23ErrorCode::DuplicateTx
    } else if err.contains(PROTOCOL_FEE_MISMATCH) {
        Hip23ErrorCode::ProtocolFeeMismatch
    } else {
        Hip23ErrorCode::Unknown
    }
}

pub fn error_code_name(code: Hip23ErrorCode) -> &'static str {
    match code {
        Hip23ErrorCode::HeightOutsideWindow => "height_outside_window",
        Hip23ErrorCode::HeightInvalidRange => "height_invalid_range",
        Hip23ErrorCode::BalanceBelowFloor => "balance_below_floor",
        Hip23ErrorCode::ChainNotAllowed => "chain_not_allowed",
        Hip23ErrorCode::GuardOnlyTopology => "guard_only_topology",
        Hip23ErrorCode::TexSettlementFail => "tex_settlement_imbalance",
        Hip23ErrorCode::TexSigFail => "tex_sig_fail",
        Hip23ErrorCode::GasNotInitialized => "gas_not_initialized",
        Hip23ErrorCode::DuplicateTx => "duplicate_tx",
        Hip23ErrorCode::ProtocolFeeMismatch => "protocol_fee_mismatch",
        Hip23ErrorCode::Unknown => "unknown",
    }
}