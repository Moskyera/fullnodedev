/// The cached template may only be served while it really extends the current tip.
/// Height alone is not enough: a reorg replaces the block at the tip height with a
/// different one, keeping `*stf[0].height > lasthei` true while the template's parent
/// is already orphaned. A block mined on an orphaned parent is PoW valid but lands off
/// the main chain, so it pays nothing while the pool still credits the round.
fn miner_pending_stuff_stale(
    stf: &VecDeque<MinerBlockStuff>,
    lasthei: u64,
    tiphash: &Hash,
) -> bool {
    if stf.is_empty() {
        return true;
    }
    *stf[0].height <= lasthei || stf[0].block.prevhash() != tiphash
}

fn miner_pending(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let detail = q_bool(&req, "detail", false);
    let transaction = q_bool(&req, "transaction", false);
    let stuff = q_bool(&req, "stuff", false);
    let base64 = q_bool(&req, "base64", false);

    if !ctx.engine.config().miner_enable {
        return api_error("miner not enabled");
    }

    #[cfg(not(debug_assertions))]
    {
        let gotdmintx = ctx
            .hnoder
            .txpool()
            .first_at(TXGID_DIAMINT)
            .unwrap()
            .is_some();
        if ctx.engine.config().is_mainnet() && !gotdmintx && curtimes() < ctx.launch_time + 30 {
            return api_error("miner worker must be launched at least 30 secs after node start");
        }
    }

    let tip = ctx.engine.latest_block();
    let lasthei = tip.height().uint();
    let tiphash = tip.hash();
    let need_create_new = {
        let stf = MINER_PENDING_BLOCK.lock().unwrap();
        miner_pending_stuff_stale(&stf, lasthei, &tiphash)
    };

    if need_create_new {
        let _pack_guard = MINER_PACKING_LOCK.lock().unwrap();
        // Double check
        let still_need_create = {
            let stf = MINER_PENDING_BLOCK.lock().unwrap();
            miner_pending_stuff_stale(&stf, lasthei, &tiphash)
        };
        if still_need_create {
            miner_reset_next_new_block(ctx.engine.clone(), ctx.hnoder.txpool().as_ref());
        }
    }

    get_miner_pending_block_stuff(detail, transaction, stuff, base64)
}

#[cfg(test)]
mod miner_pending_stuff_tests {
    use super::*;

    fn pending_stuff(height: u64, prevhash: Hash) -> MinerBlockStuff {
        let mut block = BlockV1::default();
        block.intro.head.height = BlockHeight::from(height);
        block.intro.head.prevhash = prevhash;
        MinerBlockStuff {
            height: BlockHeight::from(height),
            block_nonce: Uint4::default(),
            coinbase_nonce: Hash::default(),
            target_hash: Hash::default(),
            coinbase_tx: crate::TransactionCoinbase::default(),
            block,
            mrklrts: vec![],
        }
    }

    #[test]
    fn empty_pending_queue_is_always_stale() {
        let stf = VecDeque::new();
        assert!(miner_pending_stuff_stale(&stf, 100, &Hash::default()));
    }

    #[test]
    fn template_on_the_current_tip_is_fresh_but_a_passed_height_is_stale() {
        let tip = Hash::from([0x11u8; Hash::SIZE]);
        let mut stf = VecDeque::new();
        stf.push_front(pending_stuff(101, tip.clone()));
        assert!(!miner_pending_stuff_stale(&stf, 100, &tip));
        // the chain already moved on to the template height
        assert!(miner_pending_stuff_stale(&stf, 101, &tip));
    }

    #[test]
    fn same_height_reorg_of_the_tip_invalidates_the_template() {
        let old_tip = Hash::from([0x11u8; Hash::SIZE]);
        let new_tip = Hash::from([0x22u8; Hash::SIZE]);
        let mut stf = VecDeque::new();
        stf.push_front(pending_stuff(101, old_tip));
        // the tip height did not move, but the tip block itself was replaced: the
        // template's parent is orphaned and must not be mined on any more
        assert!(miner_pending_stuff_stale(&stf, 100, &new_tip));
    }
}
