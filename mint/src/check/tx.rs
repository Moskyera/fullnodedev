/// Minimum sane wire size for a signed Type 4 tx (~5 KB signatures + header).
const TYPE4_MEMPOOL_MIN_BYTES: usize = 512;
/// Typical signed Type 4 wire size is ~5–6 KB; log when larger for observability.
const TYPE4_MEMPOOL_WARN_BYTES: usize = 8 * 1024;

fn impl_tx_submit(this: &HacashMinter, engine: &dyn EngineRead, txp: &TxPkg) -> Rerr {
    let txr = txp.tx_read();
    let curr_hei = engine.latest_block().height().uint();
    let next_hei = curr_hei + 1;

    if txr.ty() == TransactionType4::TYPE {
        check_type4_mempool_submit(engine, txp, next_hei)?;
    }

    let Some(diamintact) = action::pickout_diamond_mint_action(txr) else {
        return Ok(()) // other normal tx
    };
    if next_hei % 5 == 0 {
        return errf!("diamond mint transaction cannot be submitted after height ending in 4 or 9")
    }
    check_diamond_mint_minimum_bidding_fee(next_hei, txr, &diamintact)?;
    let mut biddings = this.bidding_prove.lock().unwrap();
    biddings.record(curr_hei, txp, &diamintact);
    Ok(())
}

fn check_type4_mempool_submit(
    engine: &dyn EngineRead,
    txp: &TxPkg,
    next_hei: u64,
) -> Rerr {
    use protocol::metrics::PqcMetricEvent;
    use protocol::transaction::effective_max_tx_wire_size;

    let txr = txp.tx_read();
    let engcnf = engine.config();
    let wire_len = txp.data().len();
    let max_allowed = effective_max_tx_wire_size(engcnf.max_tx_size, txr.ty());

    if wire_len < TYPE4_MEMPOOL_MIN_BYTES {
        protocol::metrics::emit(PqcMetricEvent::Type4MempoolRejected);
        return errf!(
            "type 4 tx wire size {} below mempool minimum {}",
            wire_len,
            TYPE4_MEMPOOL_MIN_BYTES
        );
    }
    if wire_len > max_allowed {
        protocol::metrics::emit(PqcMetricEvent::Type4MempoolRejected);
        return errf!(
            "type 4 tx wire size {} exceeds mempool cap {} (engine max {})",
            wire_len,
            max_allowed,
            engcnf.max_tx_size
        );
    }

    protocol::upgrade::check_gated_tx(engcnf.chain_id, next_hei, txr.ty())?;

    let main = txr.main();
    if !main.is_pqckey() && !main.is_hybrid() {
        protocol::metrics::emit(PqcMetricEvent::Type4MempoolRejected);
        return errf!("type 4 mempool: main address must be PQCKEY (v6) or HYBRID (v7)");
    }
    if main.is_privakey_unknown() {
        protocol::metrics::emit(PqcMetricEvent::Type4MempoolRejected);
        return errf!(
            "type 4 mempool: main address {} has unknown system private key",
            main
        );
    }

    for sign in txr.hybrid_signs() {
        if let Err(e) = sign.check_wire() {
            protocol::metrics::emit(PqcMetricEvent::Type4MempoolRejected);
            return errf!("type 4 hybrid sign wire invalid: {}", e);
        }
    }

    if wire_len > TYPE4_MEMPOOL_WARN_BYTES {
        println!(
            "[mempool] type4 tx wire size {} bytes (>{} warn threshold)",
            wire_len, TYPE4_MEMPOOL_WARN_BYTES
        );
    }

    let hybrid = main.is_hybrid();
    let alg = txr
        .hybrid_signs()
        .first()
        .map(|s| s.alg_id())
        .unwrap_or(0);
    println!(
        "[mempool] type4 accepted size={} main={} version={} sign_alg={}",
        wire_len,
        main.to_readable(),
        main.version(),
        alg
    );
    protocol::metrics::emit(PqcMetricEvent::Type4MempoolAccepted { hybrid });
    Ok(())
}

fn impl_tx_pool_group(tx: &TxPkg) -> usize {
    let mut group_id = TXGID_NORMAL;
    if let Some(..) = action::pickout_diamond_mint_action(tx.tx_read()) {
        group_id = TXGID_DIAMINT;
    }
    group_id
}

fn impl_tx_pool_refresh(
    _this: &HacashMinter,
    eng: &dyn EngineRead,
    txpool: &dyn TxPool,
    txs: Vec<Hash>,
    blkhei: u64,
) {
    if blkhei % 15 == 0 {
        println!("{}.", txpool.print());
    }
    // drop all overdue diamond mint tx
    if blkhei % 5 == 0 {
        clean_invalid_diamond_mint_txs(eng, txpool, blkhei);
    }
    // drop all exist normal tx
    if txs.len() > 1 {
        let _ = txpool.drain(&txs[1..]); // over coinbase tx
    }
    // drop invalid normal
    if blkhei % 11 == 0 {
        // 1 hours
        clean_invalid_normal_txs(eng, txpool, blkhei);
    }
}

fn clean_invalid_normal_txs(eng: &dyn EngineRead, txpool: &dyn TxPool, blkhei: u64) {
    let pdhei = blkhei + 1;
    let mut sub_state = eng.fork_sub_state();
    let mut keep_rest_after_uncertain = false;
    let _ = txpool.retain_at(TXGID_NORMAL, &mut |a: &TxPkg| {
        if keep_rest_after_uncertain {
            return true;
        }
        let txr = a.tx_read();
        let exec = eng.try_execute_tx_by(txr, pdhei, &mut sub_state);
        if exec.is_ok() {
            return true;
        }
        // Keep Type 3+ (incl. Type 4 ~5–6 KB PQC txs) when deterministic precheck is uncertain.
        if txr.ty() >= TransactionType3::TYPE {
            keep_rest_after_uncertain = true;
            return true;
        }
        false // delete legacy txs that still fail under deterministic precheck
    });
}

fn clean_invalid_diamond_mint_txs(eng: &dyn EngineRead, txpool: &dyn TxPool, _blkhei: u64) {
    let sta = eng.state();
    let sta = sta.as_ref();
    let curdn = CoreStateRead::wrap(sta.as_ref())
        .get_latest_diamond()
        .number
        .uint();
    let nextdn = curdn + 1;
    let _ = txpool.retain_at(TXGID_DIAMINT, &mut |a: &TxPkg| {
        nextdn == action::get_diamond_mint_number(a.tx_read())
    });
}