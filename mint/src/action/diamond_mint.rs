/*
    if self.number.to_u32() > DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE {
        skn = self.custom_message.parse(buf, skn)?;
    }
*/
combi_struct_field_more_than_condition! { DiamondMintData, {
    diamond              : DiamondName
    number               : DiamondNumber
    prev_hash            : Hash
    nonce                : Fixed8
    address              : Address
}, custom_message, Hash, number, DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE
}

/*
* simple hac to
*/
action_define! { DiamondMint, 4,
    ActScope::TOP_ONLY, 2,
    *self.d.number > DIAMOND_ABOVE_NUMBER_OF_BURNING90_PERCENT_TX_FEES,
    [],
    {
        d: DiamondMintData
    },
    (self, format!("Mint diamond <{}> number {}", self.d.diamond.to_readable(), *self.d.number)),
    (self, ctx, _gas {
        diamond_mint(self, ctx)
    })
}

//
impl DiamondMint {
    pub fn with(diamond: DiamondName, number: DiamondNumber) -> Self {
        Self {
            kind: Uint2::from(Self::KIND),
            d: DiamondMintData {
                diamond,
                number,
                prev_hash: Hash::default(),
                nonce: Fixed8::default(),
                address: Address::default(),
                custom_message: Hash::default(),
            },
        }
    }
}

/*

*/
fn diamond_mint(this: &DiamondMint, ctx: &mut dyn Context) -> XRet<Vec<u8>> {
    let act = &this.d;
    act.address.must_privakey()?;
    check_transfer_recipient_allowed(&act.address)?;
    let env = ctx.env().clone();
    check_diamond_mint_tx_type(ctx)?;
    let pending_height = env.block.height;
    let pending_hash = env.block.hash;
    let tx_bid_fee = env.tx.fee.clone();
    let number = act.number;
    let dianum = *number;
    let name = act.diamond;
    let namestr = name.to_readable();
    let prev_hash = act.prev_hash;
    let nonce = act.nonce;
    let address = act.address;
    let mut custom_message = Vec::new();
    if dianum > DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE {
        custom_message = act.custom_message.serialize();
    }
    let tx_bid_burn_238 = if dianum > DIAMOND_ABOVE_NUMBER_OF_BURNING90_PERCENT_TX_FEES {
        Some(diamond_mint_legacy_bid_burn(ctx, &tx_bid_fee)?.to_238_u64()?)
    } else {
        None
    };
    let tx_bid_burn_add_238 = tx_bid_burn_238.map(|v| v as u128);
    let mut state = CoreState::wrap(ctx.state());
    // check mine
    let (sha3hx, mediumhx, diahx) =
        x16rs::mine_diamond(dianum, &prev_hash, &nonce, &address, &custom_message);
    let not_fast_sync = !env.chain.fast_sync;
    if not_fast_sync {
        // check: diamonds may only be minted in blocks whose height is divisible by 5.
        if pending_hash.not_zero() && pending_height % 5 != 0 {
            return xerrf!("diamond must be in a block height that is divisible by 5");
        }
        // number
        let latest_diamond = state.get_latest_diamond();
        let latestdianum = *latest_diamond.number;
        let neednextnumber = latestdianum + 1;
        if dianum != neednextnumber {
            return xerrf!(
                "diamond number expected {} but got {}",
                neednextnumber,
                dianum
            );
        }
        // check prev hash
        if dianum > 1 && latest_diamond.born_hash != prev_hash {
            return xerrf!(
                "diamond prev hash expected {} but got {}",
                latest_diamond.born_hash,
                prev_hash
            );
        }
        if dianum != 1 + latestdianum {
            return xerrf!(
                "latest diamond number expected {} but got {}",
                dianum - 1,
                latestdianum
            );
        }
        // check difficulty
        let diffok = x16rs::check_diamond_difficulty(dianum, &sha3hx, &mediumhx);
        if !diffok {
            return xerrf!("diamond difficulty does not match");
        }
        // name
        let dianame = x16rs::check_diamond_hash_result(diahx);
        let Some(dianame) = dianame else {
            let dhx = match String::from_utf8(diahx.to_vec()) {
                Err(_) => hex::encode(diahx),
                Ok(d) => d,
            };
            return xerrf!("diamond hash result {} is not a valid diamond name", dhx);
        };
        let dianame = Fixed6::from(dianame);
        if name != dianame {
            return xerrf!(
                "diamond name expected {} but got {}",
                dianame.to_readable(),
                namestr
            );
        }
        // exist
        let hav = state.diamond(&name);
        if let Some(_) = hav {
            return xerrf!("diamond {} already exists", namestr);
        }
    }
    // Build the post-mint total-count view locally so we can derive
    // `average_bid_burn` without committing a snapshot that might later
    // overwrite nested updates such as blackhole HACD burn accounting.
    let ttcount_next = preview_total_count(&state, |ttcount| {
        total_add_diamond_number(&mut ttcount.minted_diamond, 1, "minted_diamond")?;
        if let Some(burn_238) = tx_bid_burn_add_238 {
            // Diamond bid burn accounting must follow the actual legacy fee split,
            // not an independent percentage approximation.
            total_add_u12(&mut ttcount.hacd_bid_burn_238, burn_238, "hacd_bid_burn_238")?;
        }
        Ok(())
    })?;
    // gene
    let life_gene = calculate_diamond_life_gene(dianum, &mediumhx, &pending_hash, &tx_bid_fee);
    // The running average here uses cumulative burned bid fee that already
    // includes the current diamond update in the projected total count.
    let average_bid_burn =
        calculate_diamond_average_bid_burn(dianum, *ttcount_next.hacd_bid_burn_238)?;
    // save diamond smelt
    let diasmelt = DiamondSmelt {
        diamond: name.clone(),
        number: number.clone(),
        born_height: BlockHeight::from(pending_height),
        born_hash: pending_hash.clone(),
        prev_hash: prev_hash.clone(),
        miner_address: act.address.clone(),
        bid_fee: tx_bid_fee.clone(),
        nonce: nonce.clone(),
        average_bid_burn: average_bid_burn,
        life_gene: life_gene,
    };
    state.set_latest_diamond(&diasmelt);
    state.diamond_smelt_set(&name, &diasmelt);
    // save diamond
    let diaitem = DiamondSto {
        status: DIAMOND_STATUS_NORMAL,
        address: act.address.clone(),
        prev_engraved_height: BlockHeight::default(), // 0
        inscripts: Inscripts::default(),              // none
    };
    state.diamond_set(&name, &diaitem);
    state.diamond_name_set(&number, &name);
    // add diamond belong
    if env.chain.diamond_form {
        diamond_owned_push_one(&mut state, &address, &name);
    }
    // add balance
    hacd_add(&mut state, &act.address, &DiamondNumber::from(1))?;
    // save count
    with_total_count(&mut state, |ttcount| {
        total_add_diamond_number(&mut ttcount.minted_diamond, 1, "minted_diamond")?;
        if let Some(burn_238) = tx_bid_burn_add_238 {
            total_add_u12(&mut ttcount.hacd_bid_burn_238, burn_238, "hacd_bid_burn_238")?;
        }
        Ok(())
    })?;
    // ok
    Ok(vec![])
}

fn check_diamond_mint_tx_type(ctx: &dyn Context) -> Rerr {
    if ctx.env().tx.ty != protocol::transaction::TransactionType2::TYPE {
        return errf!("DiamondMint can only be executed in tx type 2");
    }
    Ok(())
}

fn diamond_mint_legacy_bid_burn(ctx: &dyn Context, tx_bid_fee: &Amount) -> Ret<Amount> {
    check_diamond_mint_tx_type(ctx)?;
    tx_bid_fee.sub_mode_u128(&ctx.tx().fee_got())
}

#[cfg(test)]
mod diamond_mint_tests {
    use super::*;
    use basis::component::{Env, MemMap};
    use basis::interface::{ActExec, State, Transaction};
    use protocol::context::{ContextInst, EmptyState};
    use protocol::state::EmptyLogs;
    use protocol::transaction::{TransactionType2, TransactionType3};
    use std::sync::Weak;

    fn scoped_protocol_setup() -> protocol::setup::TestSetupScopeGuard {
        let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
        crate::setup::register_protocol_extensions(&mut setup);
        protocol::setup::install_test_scope(setup)
    }

    #[derive(Default, Clone)]
    struct FlatMemState {
        mem: MemMap,
    }

    impl State for FlatMemState {
        fn fork_sub(&self, _: Weak<Box<dyn State>>) -> Box<dyn State> {
            Box::new(Self::default())
        }

        fn merge_sub(&mut self, sta: Box<dyn State>) {
            self.mem.extend(sta.as_mem().clone());
        }

        fn detach(&mut self) {}

        fn clone_state(&self) -> Box<dyn State> {
            Box::new(self.clone())
        }

        fn as_mem(&self) -> &MemMap {
            &self.mem
        }

        fn get(&self, k: Vec<u8>) -> Option<Vec<u8>> {
            self.mem.get(&k).and_then(|v| v.clone())
        }

        fn set(&mut self, k: Vec<u8>, v: Vec<u8>) {
            self.mem.insert(k, Some(v));
        }

        fn del(&mut self, k: Vec<u8>) {
            self.mem.insert(k, None);
        }
    }

    fn diamond_mint_action(number: u32) -> DiamondMint {
        let mut act = DiamondMint::with(DiamondName::from(*b"WTYUIA"), DiamondNumber::from(number));
        act.d.address = Address::create_privakey([7u8; 20]);
        act
    }

    fn env_for_tx(tx: &dyn TransactionRead) -> Env {
        let mut env = Env::default();
        env.chain.fast_sync = true;
        env.block.height = 1;
        env.tx.ty = tx.ty();
        env.tx.main = tx.main();
        env.tx.addrs = tx.addrs();
        env.tx.fee = tx.fee().clone();
        env
    }

    #[test]
    fn diamond_mint_rejects_type3() {
        let main = Address::create_privakey([1u8; 20]);
        let fee = Amount::coin(1000, UNIT_238);
        let act = diamond_mint_action(DIAMOND_ABOVE_NUMBER_OF_BURNING90_PERCENT_TX_FEES + 1);
        let mut tx = TransactionType3::new_by(main, fee, 1);
        tx.push_action(Box::new(act.clone())).unwrap();
        let env = env_for_tx(&tx);
        let mut ctx = ContextInst::new(env, Box::new(EmptyState {}), Box::new(EmptyLogs {}), &tx);

        let err = act.execute(&mut ctx).unwrap_err();
        assert!(err.to_string().contains("tx type 2"), "{err}");
    }

    #[test]
    fn diamond_mint_type2_bid_burn_uses_actual_fee_delta() {
        let main = Address::create_privakey([1u8; 20]);
        let fee = Amount::coin(1000, UNIT_238);
        let act = diamond_mint_action(DIAMOND_ABOVE_NUMBER_OF_BURNING90_PERCENT_TX_FEES + 1);
        let mut tx = TransactionType2::new_by(main, fee.clone(), 1);
        tx.push_action(Box::new(act)).unwrap();
        let env = env_for_tx(&tx);
        let ctx = ContextInst::new(
            env,
            Box::new(FlatMemState::default()),
            Box::new(EmptyLogs {}),
            &tx,
        );

        let expected = fee.sub_mode_u128(&tx.fee_got()).unwrap();
        let burn = diamond_mint_legacy_bid_burn(&ctx, tx.fee()).unwrap();
        assert_eq!(burn, expected);
    }

    #[test]
    fn diamond_mint_blackhole_hacd_count_is_not_overwritten() {
        let _guard = scoped_protocol_setup();
        let main = Address::create_privakey([1u8; 20]);
        let fee = Amount::coin(1000, UNIT_238);
        let mut act = diamond_mint_action(DIAMOND_ABOVE_NUMBER_OF_BURNING90_PERCENT_TX_FEES + 1);
        act.d.address = BLACKHOLE_ADDR;
        act.d.nonce = Fixed8::from([1u8; 8]);

        let mut tx = TransactionType2::new_by(main, fee, 1);
        tx.push_action(Box::new(act.clone())).unwrap();
        let env = env_for_tx(&tx);
        let mut ctx = ContextInst::new(
            env,
            Box::new(FlatMemState::default()),
            Box::new(EmptyLogs {}),
            &tx,
        );

        act.execute(&mut ctx).unwrap();

        let supply = CoreState::wrap(ctx.state()).get_total_count();
        assert_eq!(supply.minted_diamond.uint(), 1);
        assert_eq!(*supply.blackhole_hacd_burn_count, 1);
    }

    #[test]
    fn diamond_mint_rejects_non_blackhole_system_recipient() {
        let _guard = scoped_protocol_setup();
        let main = Address::create_privakey([1u8; 20]);
        let fee = Amount::coin(1000, UNIT_238);
        let mut act = diamond_mint_action(DIAMOND_ABOVE_NUMBER_OF_BURNING90_PERCENT_TX_FEES + 1);
        act.d.address = ADDRESS_ONEX;

        let mut tx = TransactionType2::new_by(main, fee, 1);
        tx.push_action(Box::new(act.clone())).unwrap();
        let env = env_for_tx(&tx);
        let mut ctx = ContextInst::new(
            env,
            Box::new(FlatMemState::default()),
            Box::new(EmptyLogs {}),
            &tx,
        );

        let err = act.execute(&mut ctx).unwrap_err();
        assert!(err.to_string().contains("system address"), "{err}");
    }
}
