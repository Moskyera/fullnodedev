fn diamondminer_success(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let cnf = ctx.engine.config();
    if !cnf.dmer_enable {
        return api_error("diamond miner in config not enabled");
    }
    let Ok(actdts) = body_data_may_hex(&req) else {
        return api_error("hex format invalid");
    };
    let Ok((mint, _)) = action::DiamondMint::create(&actdts) else {
        return api_error("upload action failed");
    };

    let staptr = read_mint_state(ctx);
    let state = CoreStateRead::wrap(staptr.as_ref().as_ref());

    let act = &mint.d;
    let mint_number = *act.number;
    let mint_name = act.diamond.to_readable();
    let lastdia = state.get_latest_diamond();
    if mint_number != *lastdia.number + 1 {
        return api_error("invalid diamond number");
    }
    if mint_number > 1 && act.prev_hash != lastdia.born_hash {
        return api_error("invalid diamond prev hash");
    }
    if act.address != cnf.dmer_reward_address {
        return api_error("invalid diamond reward address");
    }

    let bid_addr = Address::from(cnf.dmer_bid_account.address().clone());
    let mut bid_offer = cnf.dmer_bid_min.clone();
    if let Ok(Some(fbtx)) = ctx.hnoder.txpool().first_at(TXGID_DIAMINT) {
        let hbfe = fbtx.tx().fee().clone();
        let mmax = cnf.dmer_bid_max.clone();
        let step = cnf.dmer_bid_step.clone();
        if hbfe > mmax {
            bid_offer = mmax;
        } else if hbfe > bid_offer {
            if fbtx.tx().main() == bid_addr {
                bid_offer = hbfe;
            } else if let Ok(new_bid) = hbfe.add_mode_u64(&step) {
                bid_offer = new_bid;
            }
        }
    }
    if let Ok(new_bid) = bid_offer.compress(2, AmtCpr::Grow) {
        bid_offer = new_bid;
    }

    let mut tx = TransactionType2::new_by(bid_addr, bid_offer, curtimes());
    tx.push_action(Box::new(mint)).unwrap();
    tx.fill_sign(&cnf.dmer_bid_account).unwrap();
    let txhx = tx.hash();
    let txpkg = TxPkg::create(Box::new(tx));
    if let Err(e) = ctx.hnoder.submit_transaction(&txpkg, false, true) {
        return api_error(&e);
    }
    let hxstr = txhx.to_hex();
    println!(
        "▒▒▒▒ DIAMOND SUCCESS: {}({}), tx hash: {}.",
        mint_name, mint_number, hxstr
    );
    api_ok(vec![("tx_hash", json!(hxstr))])
}
