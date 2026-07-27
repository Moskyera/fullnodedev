// `burn90` is kept here only as a backward-compatible query alias for `extra9`.
// Its estimate semantics depend on tx type:
// - tx_type < 3: legacy extra9 fee split / burn behavior
// - tx_type >= 3: Type3 delta-only returned-gas surcharge behavior
fn fee_average(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let unit = q_string(&req, "unit", "fin");
    let consumption = req.query_u64("consumption", 0);
    let extra9 = q_bool(&req, "extra9", q_bool(&req, "burn90", false));
    let txty = req.query_u64("tx_type", 2) as u8;
    let avgfeep = ctx.engine.average_fee_purity();

    let mut data = serde_json::Map::new();
    data.insert("purity".to_owned(), json!(avgfeep));

    if consumption > 0 {
        let Some(fee238) = (avgfeep as u128).checked_mul(consumption as u128) else {
            return api_error("fee estimate overflow");
        };
        if fee238 > u64::MAX as u128 {
            return api_error("fee estimate overflow");
        }
        let mut base = Amount::unit238(fee238 as u64);
        if base.is_zero() {
            base = Amount::zhu(1);
        }
        let mut setfee = base.clone();
        if extra9 {
            if txty < protocol::transaction::TransactionType3::TYPE {
                if let Ok(f) = base.dist_mul(10) {
                    setfee = f;
                }
            } else if let Ok(f) = base.dist_mul(9) {
                setfee = f;
            }
        }
        data.insert("feasible".to_owned(), json!(setfee.to_unit_string(&unit)));
    }
    api_data(data)
}

fn fee_raise(ctx: &ApiExecCtx, mut req: ApiRequest) -> ApiResponse {
    let fee_s = q_string(&req, "fee", "");
    let fee_prikey = take_secret_query(&mut req, "fee_prikey");
    let hash = q_string(&req, "hash", "");
    let Ok(fee) = Amount::from(&fee_s) else {
        return api_error("fee format invalid");
    };
    let Ok(acc) = Account::create_by(&fee_prikey) else {
        return api_error("fee_prikey format invalid");
    };

    let bddts = if !hash.is_empty() {
        let Ok(hx) = hex::decode(&hash) else {
            return api_error("hash parse failed");
        };
        if hx.len() != Hash::SIZE {
            return api_error("hash size invalid");
        }
        let txhx = Hash::must(&hx);
        let txf = ctx.hnoder.txpool().find(&txhx);
        let Some(tx) = txf else {
            return api_error(&format!("cannot find tx by hash {} in tx pool", hash));
        };
        tx.data().to_vec()
    } else {
        let Ok(b) = body_data_may_hex(&req) else {
            return api_error("tx body invalid");
        };
        b.into()
    };

    let txb = protocol::transaction::transaction_create(&bddts);
    let Ok((mut txb, _)) = txb else {
        return api_error("transaction parse failed");
    };
    if let Some(resp) = reject_api_tx_non_canonical_dia_insc_push_wire(txb.as_read()) {
        return resp;
    }

    let old_fee = txb.fee();
    if fee < *old_fee {
        return api_error(&format!(
            "fee {} cannot be less than previous fee {}",
            fee, old_fee
        ));
    }
    txb.set_fee(fee.clone());
    if txb.fill_sign(&acc).is_err() {
        return api_error("fill sign failed");
    }
    let txhash = txb.hash();
    let txhashwf = txb.hash_with_fee();
    let txpkg = TxPkg::create(txb);

    let is_async = true;
    if let Err(e) = ctx.hnoder.submit_transaction(&txpkg, is_async, false) {
        return api_error(&e);
    }

    api_data(serde_json::Map::from_iter([
        ("hash".to_owned(), json!(txhash.to_hex())),
        ("hash_with_fee".to_owned(), json!(txhashwf.to_hex())),
        ("fee".to_owned(), json!(fee.to_fin_string())),
        ("tx_body".to_owned(), json!(txpkg.tx().serialize().to_hex())),
    ]))
}
