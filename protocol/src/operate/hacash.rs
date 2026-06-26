use std::vec;

macro_rules! check_amount_is_positive {
    ($amt:expr) => {
        if ! $amt.is_positive() {
            return xerrf!("amount {} value is not positive", $amt)
        }
    };
}


macro_rules! amount_op_func_define {
    ($fn:ident, $hac:ident, $addr:ident, $amt:ident, $exec:block) => (

        fn $fn(ctx: &mut dyn Context, $addr: &Address, $amt: &Amount) -> XRet<Amount> {
            $addr.check_version()?;
            let state = &mut CoreState::wrap(ctx.state());
            let mut bls = state.balance( $addr ).unwrap_or_default();
            let $hac = bls.hacash;
            let newhac = $exec; // do add or sub
            newhac.check_store_long()?;
            bls.hacash = newhac.clone();
            state.balance_set($addr, &bls);
            Ok(newhac)
        }

    )

}

amount_op_func_define!{do_hac_sub, hac, addr, amt, {
    if hac < *amt {
        return xerr_rf!("address {} balance {} is insufficient, at least {}", 
            addr, hac, amt)
    }
    hac.sub_mode_u128(amt)?
}}

amount_op_func_define!{do_hac_add, hac, addr, amt, {
    hac.add_mode_u128(amt)?
}}


pub fn hac_transfer(ctx: &mut dyn Context, from: &Address, to: &Address, amt: &Amount) -> XRet<Vec<u8>> {
    crate::upgrade::check_transfer_addr_online_open(
        ctx.env().chain.id,
        ctx.env().block.height,
        from,
        to,
    )?;
    // is to self
    if from == to {
        if !from.is_user_signing_address() {
            return xerrf!("non-signing address cannot transfer HAC to self")
        }
        // historical compatibility: legacy blocks (<200000) allow self-transfer fast path
        if ctx.env().block.height >= 20_0000 {
            // you can transfer it to yourself without changing the status, which is a waste of service fees
            hac_check(ctx, from, amt)?;
        }
        return Ok(vec![]);
    }
    /*test debug
    let tadr = Address::from_readable("1EuGe2GU8tDKnHLNfBsgyffx66buK7PP6g").unwrap();
    if *from == tadr || *to == tadr {
        println!("-------- {} ---- {} => {}  {}", ctx.env().block.height, from.to_readable(), to.to_readable(), amt);
    }*/
    // do trs
    check_amount_is_positive!(amt);
    do_hac_sub(ctx, from, amt)?;
    hac_add(ctx, to, amt)?; // hac_add includes blackhole_engulf
    Ok(vec![])
}



pub fn hac_check(ctx: &mut dyn Context, addr: &Address, amt: &Amount) -> XRet<Amount> {
    check_amount_is_positive!(amt);
    addr.check_version()?;
    let state = CoreState::wrap(ctx.state());
    if let Some(bls) = state.balance( addr ) {
        // println!("address {} balance {}", addr.to_readable(), bls.hacash );
        if bls.hacash >= *amt {
            return Ok(bls.hacash)
        }
    }
    xerr_rf!("address {} balance is insufficient, at least {}", addr, amt)
}


pub fn hac_add(ctx: &mut dyn Context, addr: &Address, amt: &Amount) -> XRet<Vec<u8>> {
    check_amount_is_positive!(amt);
    do_hac_add(ctx, addr, amt)?;
    let state = &mut CoreState::wrap(ctx.state());
    if blackhole_engulf(state, addr) {
        total_record_blackhole_hac(state, amt)?;
    }
    Ok(vec![])
}


pub fn hac_sub(ctx: &mut dyn Context, addr: &Address, amt: &Amount) -> XRet<Vec<u8>> {
    check_amount_is_positive!(amt);
    do_hac_sub(ctx, addr, amt)?;
    Ok(vec![])
}
