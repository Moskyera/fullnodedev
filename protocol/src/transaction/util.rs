
pub fn create_tx_info(tx: &dyn TransactionRead) -> TxInfo {
    TxInfo {
        ty: tx.ty(),
        main: tx.main(),
        addrs: tx.addrs(),
        fee: tx.fee_pay(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxSignatureReport {
    pub required: Vec<Address>,
    pub present: Vec<Address>,
    pub valid: Vec<Address>,
    pub missing: Vec<Address>,
    pub invalid: Vec<Address>,
}

fn sort_addresses(addrs: &mut Vec<Address>) {
    addrs.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    addrs.dedup();
}

fn signature_present_for(addr: &Address, signs: &[Sign]) -> bool {
    signs.iter().any(|sig| {
        Address::from(Account::get_address_by_public_key(*sig.publickey)) == *addr
    })
}

pub fn signature_report(tx: &dyn TransactionRead) -> Ret<TxSignatureReport> {
    let mut required: Vec<_> = tx.req_sign()?.into_iter().collect();
    sort_addresses(&mut required);
    let mut present: Vec<_> = tx
        .signs()
        .iter()
        .map(|sig| Address::from(Account::get_address_by_public_key(*sig.publickey)))
        .collect();
    sort_addresses(&mut present);

    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let main_addr = tx.main();
    let txty = tx.ty();
    let signs = tx.signs();
    let mut valid = Vec::new();
    let mut missing = Vec::new();
    let mut invalid = Vec::new();
    for adr in &required {
        let mut ckhx = &hx;
        if *adr == main_addr && txty != TransactionType1::TYPE {
            ckhx = &hxwf;
        }
        match verify_one_sign(ckhx, adr, signs) {
            Ok(true) => valid.push(*adr),
            Ok(false) => invalid.push(*adr),
            Err(_) if !signature_present_for(adr, signs) => missing.push(*adr),
            Err(_) => invalid.push(*adr),
        }
    }
    sort_addresses(&mut valid);
    sort_addresses(&mut missing);
    sort_addresses(&mut invalid);
    Ok(TxSignatureReport {
        required,
        present,
        valid,
        missing,
        invalid,
    })
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
