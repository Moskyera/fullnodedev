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

    let is_async = true;
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