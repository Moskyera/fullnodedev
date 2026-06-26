/// Effective mempool / RPC wire-size cap for a transaction type.
#[inline]
pub fn effective_max_tx_wire_size(engine_max_tx_size: usize, tx_type: u8) -> usize {
    if tx_type == TransactionType4::TYPE {
        engine_max_tx_size.min(TransactionType4::MAX_WIRE_SIZE)
    } else {
        engine_max_tx_size
    }
}

pub fn create_tx_info(tx: &dyn TransactionRead) -> TxInfo {
    TxInfo {
        ty: tx.ty(),
        main: tx.main(),
        addrs: tx.addrs(),
        fee: tx.fee_pay(),
    }
}


/**
* verify tx all needs signature
*/
pub fn verify_tx_signature(tx: &dyn TransactionRead) -> Rerr {
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let signs = tx.signs();
    let addrs = tx.req_sign()?;
    let main_addr = tx.main();
    let txty = tx.ty();
    for adr in addrs {
        let mut ckhx = &hx;
        if adr == main_addr && txty != TransactionType1::TYPE {
            ckhx = &hxwf;
        }
        verify_one_sign(ckhx, &adr, signs)?;
    }
    Ok(())
}


pub fn check_tx_signature(tx: &dyn TransactionRead) -> Ret<HashMap<Address, bool>> {
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let signs = tx.signs();
    let addrs = tx.req_sign()?;
    let main_addr = tx.main();
    let txty = tx.ty();
    let mut ckres = HashMap::new();
    for sig in signs {
        let adr = Address::from(Account::get_address_by_public_key(*sig.publickey));
        ckres.insert(adr, true);
    }
    for adr in addrs {
        let mut ckhx = &hx;
        if adr == main_addr && txty != TransactionType1::TYPE {
            ckhx = &hxwf;
        }
        let mut sigok = false;
        if let Ok(yes) = verify_one_sign(ckhx, &adr, signs) {
            if yes {
                sigok = true;
            }
        }
        ckres.insert(adr, sigok);
    }
    Ok(ckres)
}


pub fn verify_target_signature(adr: &Address, tx: &dyn TransactionRead) -> Ret<bool> {
    if adr.is_privakey_unknown() {
        return errf!(
            "address {} is a system address (value < u32::MAX) with unknown private key, cannot sign",
            adr
        );
    }
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let signs = tx.signs();
    // let ddrs = tx.req_sign();
    let main_addr = tx.main();
    let mut ckhx = &hx;
    if *adr == main_addr && tx.ty() != TransactionType1::TYPE {
        ckhx = &hxwf;
    }
    verify_one_sign(ckhx, adr, signs)
}


pub fn verify_one_sign(hash: &Hash, addr: &Address, signs: &Vec<Sign>) -> Ret<bool> {
    if addr.is_privakey_unknown() {
        return errf!(
            "address {} is a system address (value < u32::MAX) with unknown private key, cannot sign",
            addr
        );
    }
    for sig in signs {
        if basis::method::verify_signature(hash, addr, sig) {
            return Ok(true)
        }
    }
    errf!("{} signature verification failed", addr)
}
