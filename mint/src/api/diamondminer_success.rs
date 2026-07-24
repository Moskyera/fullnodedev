/// Bid fee for the diamond mint tx the node signs on the miner's behalf.
/// `highest` is the top bid currently sitting in the tx pool plus whether it is our own.
fn diamond_mint_bid_offer(
    bid_min: &Amount,
    bid_max: &Amount,
    bid_step: &Amount,
    highest: Option<(&Amount, bool)>,
    mint_number: u32,
    pending_hei: u64,
) -> Ret<Amount> {
    let mut bid_offer = bid_min.clone();
    if let Some((hbfe, is_mine)) = highest {
        if hbfe > bid_max {
            bid_offer = bid_max.clone();
        } else if hbfe > &bid_offer {
            if is_mine {
                bid_offer = hbfe.clone();
            } else if let Ok(new_bid) = hbfe.add_mode_u64(bid_step) {
                bid_offer = new_bid;
            }
        }
    }
    if let Ok(new_bid) = bid_offer.compress(2, AmtCpr::Grow) {
        bid_offer = new_bid;
    }
    // dmer_bid_max is the operator's hard cap on the real money burned per diamond, and both
    // the bid step and compress(Grow) round upwards, so the clamp must come last of all.
    // The configured max is already compressed at config load, so it stays a valid fee.
    if bid_offer > *bid_max {
        bid_offer = bid_max.clone();
    }
    // Above DIAMOND_ABOVE_NUMBER_OF_MIN_FEE_AND_FORCE_CHECK_HIGHEST consensus rejects any
    // mint whose fee is under the block reward (check_diamond_mint_minimum_bidding_fee), so
    // a bid below that floor could never be mined and the diamond would be lost silently.
    if mint_number > action::DIAMOND_ABOVE_NUMBER_OF_MIN_FEE_AND_FORCE_CHECK_HIGHEST {
        let bidmin = genesis::block_reward(pending_hei);
        if bid_offer < bidmin {
            if bidmin > *bid_max {
                return errf!(
                    "diamond bidding fee minimum {} is above the configured bid_max {}, raise bid_max in config",
                    bidmin,
                    bid_max
                )
            }
            bid_offer = bidmin;
        }
    }
    Ok(bid_offer)
}

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
    let highest = match ctx.hnoder.txpool().first_at(TXGID_DIAMINT) {
        Ok(Some(fbtx)) => Some((fbtx.tx().fee().clone(), fbtx.tx().main() == bid_addr)),
        _ => None,
    };
    let pending_hei = ctx.engine.latest_block().height().uint() + 1;
    let bid_offer = match diamond_mint_bid_offer(
        &cnf.dmer_bid_min,
        &cnf.dmer_bid_max,
        &cnf.dmer_bid_step,
        highest.as_ref().map(|(hbfe, is_mine)| (hbfe, *is_mine)),
        mint_number,
        pending_hei,
    ) {
        Ok(v) => v,
        Err(e) => return api_error(&e),
    };

    let mut tx = TransactionType2::new_by(bid_addr, bid_offer, curtimes());
    if let Err(e) = tx.push_action(Box::new(mint)) {
        return api_error(&format!("push diamond mint action failed: {}", e));
    }
    if let Err(e) = tx.fill_sign(&cnf.dmer_bid_account) {
        return api_error(&format!("bid account cannot sign: {}", e));
    }
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

#[cfg(test)]
mod diamond_mint_bid_offer_tests {
    use super::*;

    const CKN: u32 = action::DIAMOND_ABOVE_NUMBER_OF_MIN_FEE_AND_FORCE_CHECK_HIGHEST;

    #[test]
    fn empty_pool_bids_the_configured_minimum() {
        let bid_min = Amount::small_mei(1);
        let bid_max = Amount::small_mei(10);
        let bid_step = Amount::small_mei(2);
        let offer = diamond_mint_bid_offer(&bid_min, &bid_max, &bid_step, None, CKN, 0).unwrap();
        assert_eq!(offer, bid_min);
    }

    #[test]
    fn raising_over_a_competitor_never_exceeds_the_configured_maximum() {
        let bid_min = Amount::small_mei(1);
        let bid_max = Amount::small_mei(10);
        let bid_step = Amount::small_mei(2);
        // a competitor sits exactly on our maximum: adding the step would spend 12 HAC,
        // 2 HAC above the cap the operator configured
        let highest = Amount::small_mei(10);
        let offer =
            diamond_mint_bid_offer(&bid_min, &bid_max, &bid_step, Some((&highest, false)), CKN, 0)
                .unwrap();
        assert_eq!(offer, bid_max);
    }

    #[test]
    fn a_misconfigured_minimum_above_the_maximum_is_still_capped() {
        let bid_min = Amount::small_mei(20);
        let bid_max = Amount::small_mei(10);
        let bid_step = Amount::small_mei(2);
        let offer = diamond_mint_bid_offer(&bid_min, &bid_max, &bid_step, None, CKN, 0).unwrap();
        assert_eq!(offer, bid_max);
    }

    #[test]
    fn bid_is_raised_to_the_consensus_minimum_above_the_force_check_number() {
        let bid_min = Amount::small(1, 247); // 0.1 HAC, under the block reward
        let bid_max = Amount::small_mei(10);
        let bid_step = Amount::small_mei(2);
        let reward = genesis::block_reward(0);
        // below the force check number the low minimum is still used as is
        let low = diamond_mint_bid_offer(&bid_min, &bid_max, &bid_step, None, CKN, 0).unwrap();
        assert_eq!(low, bid_min);
        // above it the fee must reach the block reward or consensus rejects the mint
        let offer = diamond_mint_bid_offer(&bid_min, &bid_max, &bid_step, None, CKN + 1, 0).unwrap();
        assert_eq!(offer, reward);
    }

    #[test]
    fn consensus_minimum_above_the_configured_maximum_is_reported_not_overspent() {
        let bid_min = Amount::small(1, 247);
        let bid_max = Amount::small(5, 247); // 0.5 HAC, under the block reward
        let bid_step = Amount::small(1, 247);
        let res = diamond_mint_bid_offer(&bid_min, &bid_max, &bid_step, None, CKN + 1, 0);
        assert!(res.is_err());
    }
}
