use field::*;
use sys::*;

// Local development is allowed from genesis to this height.
pub const DEV_OPEN_MAX_HEIGHT: u64 = 65_432;

// Set the real mainnet activation height before rollout.
pub const ONLINE_OPEN_HEIGHT: u64 = 765_432;
// Post-quantum Type4 transaction activation height (soft-fork).
pub const PQC_TYPE4_OPEN_HEIGHT: u64 = 876_543;
pub const MAINNET_CHAIN_ID: u32 = 0;

// One-time pre-upgrade allowlist.
// In the middle closed interval only legacy tx/action kinds below are allowed.
// Remove this whole file after the activation height has passed and the gate is no longer needed.

#[inline]
fn is_pre_upgrade_allowed_tx_type(tx_type: u8) -> bool {
    matches!(tx_type, 1 | 2)
}

#[inline]
fn is_pqc_tx_type(tx_type: u8) -> bool {
    tx_type == 4
}

#[inline]
pub fn is_pqc_type4_open(height: u64) -> bool {
    height >= PQC_TYPE4_OPEN_HEIGHT
}

#[inline]
fn is_pre_upgrade_allowed_action(kind: u16) -> bool {
    matches!(
        kind,
        1 | 13 | 14 | // Hac*Trs
        2 | 3 | // Channel*
        4 | // DiamondMint
        5 | 6 | 7 | 8 | // Dia*Trs
        32 | 33 // DiaInscPush / DiaInscClean
    )
}

#[inline]
pub fn is_online_upgrade_open(height: u64) -> bool {
    height >= ONLINE_OPEN_HEIGHT
}

#[inline]
fn is_dev_upgrade_open(height: u64) -> bool {
    height <= DEV_OPEN_MAX_HEIGHT
}

#[inline]
pub fn check_gated_tx(chain_id: u32, height: u64, tx_type: u8) -> Rerr {
    if chain_id != MAINNET_CHAIN_ID {
        return Ok(());
    }
    if is_pqc_tx_type(tx_type) {
        // The post-quantum (PQC) transaction type 4 is NOT part of the official
        // Istanbul mainnet upgrade (the official node registers tx types 1-3 only).
        // Reject it on mainnet at ALL heights so mainnet consensus stays
        // byte-faithful with the official node. PQC remains fully available on
        // non-mainnet chain_ids, which returned Ok above (for testnet / future
        // rollout). PQC_TYPE4_OPEN_HEIGHT is retained only as documentation of the
        // intended future activation height.
        let _ = (PQC_TYPE4_OPEN_HEIGHT, is_pqc_type4_open(height));
        return errf!(
            "PQC tx type {} is not enabled on mainnet (chain_id {})",
            tx_type,
            chain_id
        );
    }
    if is_online_upgrade_open(height)
        || is_dev_upgrade_open(height)
        || is_pre_upgrade_allowed_tx_type(tx_type)
    {
        return Ok(());
    }
    errf!(
        "tx type {} not enabled at height {}, allowed when height >= {}",
        tx_type,
        height,
        ONLINE_OPEN_HEIGHT
    )
}

#[inline]
pub fn check_gated_action(chain_id: u32, height: u64, kind: u16) -> Rerr {
    if chain_id != MAINNET_CHAIN_ID {
        return Ok(());
    }
    if is_online_upgrade_open(height)
        || is_dev_upgrade_open(height)
        || is_pre_upgrade_allowed_action(kind)
    {
        return Ok(());
    }
    errf!(
        "action kind {} not enabled at height {}, allowed when height >= {}",
        kind,
        height,
        ONLINE_OPEN_HEIGHT
    )
}

#[inline]
pub fn check_transfer_addr_online_open(
    chain_id: u32,
    height: u64,
    from: &Address,
    to: &Address,
) -> Rerr {
    if chain_id != MAINNET_CHAIN_ID {
        return Ok(());
    }
    // PQC address versions (v6 PQCKEY, v7 HYBRID / ML-DSA-65) are not part of the
    // official Istanbul mainnet — the official node's Address::check_version
    // rejects them. Reject any transfer touching a PQC address at ALL mainnet
    // heights (this must run before the online-open early return below) so our
    // mainnet acceptance stays byte-faithful with the official node.
    if from.is_pqckey() || from.is_hybrid() || to.is_pqckey() || to.is_hybrid() {
        return errf!("PQC address versions (v6/v7) are not enabled on mainnet");
    }
    if is_online_upgrade_open(height) {
        return Ok(());
    }
    if from.is_scriptmh() {
        return errf!(
            "transfer from scriptmh address is not enabled before height {}",
            ONLINE_OPEN_HEIGHT
        );
    }
    if from.is_contract() || to.is_contract() {
        return errf!(
            "contract transfer in/out is not enabled before height {}",
            ONLINE_OPEN_HEIGHT
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_marker_height_is_not_online_open() {
        let mid = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        assert!(is_dev_upgrade_open(0));
        assert!(!is_online_upgrade_open(0));
        assert!(check_gated_tx(MAINNET_CHAIN_ID, mid, 3).is_err());
        assert!(check_gated_action(MAINNET_CHAIN_ID, mid, 25).is_err());
    }

    #[test]
    fn middle_height_is_closed_for_gated_tx_and_action() {
        let height = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        assert!(!is_online_upgrade_open(height));
        assert!(check_gated_tx(MAINNET_CHAIN_ID, height, 3).is_err());
        assert!(check_gated_action(MAINNET_CHAIN_ID, height, 25).is_err());
    }

    #[test]
    fn online_height_is_open_for_gated_tx_and_action() {
        assert!(is_online_upgrade_open(ONLINE_OPEN_HEIGHT));
        assert!(check_gated_tx(MAINNET_CHAIN_ID, ONLINE_OPEN_HEIGHT, 3).is_ok());
        assert!(check_gated_action(MAINNET_CHAIN_ID, ONLINE_OPEN_HEIGHT, 25).is_ok());
    }

    #[test]
    fn ungated_kind_always_passes() {
        let height = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        assert!(check_gated_action(MAINNET_CHAIN_ID, height, 1).is_ok());
        assert!(check_gated_action(MAINNET_CHAIN_ID, height, 2).is_ok());
        assert!(check_gated_action(MAINNET_CHAIN_ID, height, 4).is_ok());
        assert!(check_gated_action(MAINNET_CHAIN_ID, height, 32).is_ok());
        assert!(check_gated_tx(MAINNET_CHAIN_ID, height, 1).is_ok());
        assert!(check_gated_tx(MAINNET_CHAIN_ID, height, 2).is_ok());
    }

    #[test]
    fn representative_non_allowlist_actions_are_gated() {
        let height = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        for kind in [
            10u16,  // SatToTrs
            17,     // AssetToTrs
            22,     // TexCellAct
            0x0401, // TxMessage
            0x0412, // HeightScope
            25,     // AstSelect
            34,     // DiaInscEdit
            40,     // ContractDeploy
            0x0601, // ViewBalance
            0x0701, // EnvHeight
        ] {
            assert!(
                check_gated_action(MAINNET_CHAIN_ID, height, kind).is_err(),
                "kind {}",
                kind
            );
        }
    }

    #[test]
    fn tx_type3_is_gated_in_middle_closed_interval() {
        let height = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        assert!(check_gated_tx(MAINNET_CHAIN_ID, height, 3).is_err());
    }

    #[test]
    fn sidechain_bypasses_gates() {
        let sidechain_id = 1u32;
        let height = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        assert!(check_gated_tx(sidechain_id, height, 3).is_ok());
        assert!(check_gated_action(sidechain_id, height, 25).is_ok());
    }

    #[test]
    fn tx_type4_is_gated_in_middle_closed_interval() {
        let height = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
        assert!(check_gated_tx(MAINNET_CHAIN_ID, height, 4).is_err());
    }

    #[test]
    fn tx_type4_is_rejected_on_mainnet_at_all_heights() {
        // PQC type 4 is neutralized on mainnet to match the official Istanbul node
        // (which has no type 4) — rejected at the dev window, the middle interval,
        // the online-open height, and the former PQC activation height alike.
        for height in [0u64, DEV_OPEN_MAX_HEIGHT, ONLINE_OPEN_HEIGHT, PQC_TYPE4_OPEN_HEIGHT] {
            assert!(check_gated_tx(MAINNET_CHAIN_ID, height, 4).is_err());
        }
    }

    #[test]
    fn tx_type4_is_allowed_on_non_mainnet() {
        // PQC stays available off mainnet (testnet / sidechain / future rollout).
        let sidechain_id = 1u32;
        assert!(check_gated_tx(sidechain_id, PQC_TYPE4_OPEN_HEIGHT, 4).is_ok());
        assert!(check_gated_tx(sidechain_id, 0, 4).is_ok());
    }

    #[test]
    fn pqc_addresses_are_rejected_in_mainnet_transfers() {
        let priva = Address::from_readable("1271438866CSDpJUqrnchoJAiGGBFSQhjd").unwrap();
        let pqc = Address::create_pqckey([9u8; 20]);
        let hybrid = Address::create_hybrid([7u8; 20]);
        // A normal privakey->privakey transfer is fine at any mainnet height.
        assert!(check_transfer_addr_online_open(MAINNET_CHAIN_ID, ONLINE_OPEN_HEIGHT, &priva, &priva).is_ok());
        // Any PQC address as from or to is rejected on mainnet, even above online-open.
        assert!(check_transfer_addr_online_open(MAINNET_CHAIN_ID, ONLINE_OPEN_HEIGHT, &pqc, &priva).is_err());
        assert!(check_transfer_addr_online_open(MAINNET_CHAIN_ID, ONLINE_OPEN_HEIGHT, &priva, &hybrid).is_err());
        // Off mainnet, PQC addresses are allowed.
        assert!(check_transfer_addr_online_open(1u32, ONLINE_OPEN_HEIGHT, &pqc, &hybrid).is_ok());
    }
}
