fn submit_transaction(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let engcnf = ctx.engine.config();
    let Ok(bddts) = body_data_may_hex(&req) else {
        return api_error("transaction body invalid");
    };
    let txpkg = protocol::transaction::build_tx_package(bddts);
    let Ok(txpkg) = txpkg else {
        return api_error("transaction parse failed");
    };
    if let Some(resp) = reject_api_tx_non_canonical_dia_insc_push_wire(txpkg.tx_read()) {
        return resp;
    }

    if txpkg.fpur() < engcnf.lowest_fee_purity {
        return api_error(&format!(
            "The transaction fee purity {} is too low, the node minimum configuration is {}.",
            txpkg.fpur(), engcnf.lowest_fee_purity
        ));
    }
    let txr = txpkg.tx_read();
    let max_wire =
        protocol::transaction::effective_max_tx_wire_size(engcnf.max_tx_size, txr.ty());
    let txsz = txpkg.data().len();
    if txsz > max_wire {
        if txr.ty() == TransactionType4::TYPE {
            protocol::metrics::emit(protocol::metrics::PqcMetricEvent::Type4MempoolRejected);
        }
        return api_error(&format!(
            "tx size {} cannot exceed {} bytes",
            txsz, max_wire
        ));
    }

    // `ret` is the only thing a wallet or a mining pool learns from this endpoint.
    // Submitting asynchronously answers Ok() the instant the work is queued, before
    // the node has executed the transaction, run the minter checks or inserted it
    // into the mempool, and every rejection after that point is dropped without a
    // log (node handler loop: `let _ = ack`). The caller is then told a transfer was
    // accepted that the node never held, and a pool records a payout that can never
    // confirm. Wait for the node's real answer, exactly as /submit/miner/success
    // already does for blocks. `async=true` keeps the old fire-and-forget behaviour
    // for callers that do not read the result.
    let is_async = q_bool(&req, "async", false);
    let only_insert_txpool = q_bool(&req, "only_insert_txpool", false);
    if let Err(e) = ctx
        .hnoder
        .submit_transaction(&txpkg, is_async, only_insert_txpool)
    {
        if txr.ty() == TransactionType4::TYPE {
            protocol::metrics::emit(protocol::metrics::PqcMetricEvent::Type4MempoolRejected);
        }
        return api_error(&e);
    }

    let mut data = serde_json::Map::new();
    data.insert("hash".to_owned(), json!(txpkg.hash().to_hex()));
    data.insert("tx_type".to_owned(), json!(txr.ty()));
    data.insert("wire_size".to_owned(), json!(txsz));
    let main = txr.main();
    data.insert("main_address".to_owned(), json!(main.to_readable()));
    data.insert("address_version".to_owned(), json!(main.version()));
    if txr.ty() == TransactionType4::TYPE {
        if let Some(sign) = txr.hybrid_signs().first() {
            data.insert("sign_alg".to_owned(), json!(sign.alg_id()));
        }
        let mut hybrid_sigs = vec![];
        for sign in txr.hybrid_signs() {
            let alg = sign.alg_id();
            hybrid_sigs.push(json!({
                "alg": alg,
                "sign_alg": alg,
                "body_len": sign.body.length(),
            }));
        }
        data.insert("hybrid_signatures".to_owned(), json!(hybrid_sigs));
        println!(
            "[rpc] submit type4 ok hash={} size={} main={} alg={}",
            txpkg.hash().to_hex(),
            txsz,
            main.to_readable(),
            txr.hybrid_signs().first().map(|s| s.alg_id()).unwrap_or(0)
        );
    }
    api_data(data)
}

#[cfg(test)]
mod submit_transaction_ack_tests {
    use super::*;

    use basis::config::EngineConf;

    fn scoped_protocol_setup() -> protocol::setup::TestSetupScopeGuard {
        let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
        crate::setup::register_protocol_extensions(&mut setup);
        protocol::setup::install_test_scope(setup)
    }

    struct AckEngine {
        cnf: EngineConf,
    }
    impl EngineRead for AckEngine {
        fn config(&self) -> &EngineConf {
            &self.cnf
        }
    }
    impl Engine for AckEngine {}

    /// Stands in for the node. A real node reports a mempool rejection only on the
    /// synchronous path: the asynchronous one queues the work and answers Ok()
    /// before the transaction has been looked at at all.
    struct AckNoder {
        engine: Arc<dyn Engine>,
        seen_async: Mutex<Option<bool>>,
    }
    impl HNoder for AckNoder {
        fn engine(&self) -> Arc<dyn Engine> {
            self.engine.clone()
        }
        fn submit_transaction(&self, _: &TxPkg, is_async: bool, _: bool) -> Rerr {
            *self.seen_async.lock().unwrap() = Some(is_async);
            match is_async {
                true => Ok(()),
                false => errf!("tx fee purity too low"),
            }
        }
    }

    fn test_conf() -> EngineConf {
        let mut cnf = EngineConf::new(&IniObj::default());
        cnf.lowest_fee_purity = 0;
        cnf.max_tx_size = 100_000;
        cnf.max_tx_actions = 16;
        cnf.contract_cache_size = 0.0;
        cnf
    }

    fn tx_body() -> Vec<u8> {
        let mut tx = TransactionType3::default();
        tx.ty = Uint1::from(TransactionType3::TYPE);
        tx.timestamp = Timestamp::from(1u64);
        tx.fee = Amount::small_mei(1);
        let body = tx.serialize();
        // the endpoint parses the body back, so the fixture must round trip
        assert!(protocol::transaction::build_tx_package(body.clone()).is_ok());
        body
    }

    fn call_submit(query: Vec<(&str, &str)>) -> (Value, Option<bool>) {
        let engine: Arc<dyn Engine> = Arc::new(AckEngine { cnf: test_conf() });
        let noder = Arc::new(AckNoder {
            engine: engine.clone(),
            seen_async: Mutex::new(None),
        });
        let ctx = ApiExecCtx {
            engine,
            hnoder: noder.clone(),
            launch_time: 0,
            miner_worker_notice_count: Arc::default(),
        };
        let req = ApiRequest {
            query: query
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            headers: HashMap::new(),
            body: tx_body(),
        };
        let resp = submit_transaction(&ctx, req);
        let out: Value = serde_json::from_slice(&resp.body).unwrap();
        let seen = *noder.seen_async.lock().unwrap();
        (out, seen)
    }

    #[test]
    fn a_node_rejection_is_reported_instead_of_a_bogus_ret_0() {
        let _setup = scoped_protocol_setup();
        let (out, seen_async) = call_submit(vec![]);
        // the node must be asked synchronously, so the caller sees the real answer
        assert_eq!(seen_async, Some(false));
        assert_eq!(out["ret"], json!(1));
        assert_eq!(out["err"], json!("tx fee purity too low"));
    }

    #[test]
    fn fire_and_forget_stays_available_behind_an_explicit_flag() {
        let _setup = scoped_protocol_setup();
        let (out, seen_async) = call_submit(vec![("async", "true")]);
        assert_eq!(seen_async, Some(true));
        assert_eq!(out["ret"], json!(0));
    }
}