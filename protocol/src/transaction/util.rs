
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
    pub undeclared: Vec<Address>,
    pub duplicate: Vec<Address>,
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
    if tx.ty() == TransactionType3::TYPE {
        return signature_report_type3(tx);
    }
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
        undeclared: Vec::new(),
        duplicate: Vec::new(),
    })
}

fn signature_report_type3(tx: &dyn TransactionRead) -> Ret<TxSignatureReport> {
    let mut required: Vec<_> = tx.req_sign()?.into_iter().collect();
    sort_addresses(&mut required);
    let required_set: HashSet<Address> = required.iter().copied().collect();

    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let main_addr = tx.main();
    let signs = tx.signs();

    let mut present = Vec::new();
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut undeclared = Vec::new();
    let mut duplicate = Vec::new();
    let mut seen_pubkeys = HashSet::new();
    let mut seen_addrs = HashSet::new();

    for sig in signs {
        if sig.size() != TransactionType3::SIGN_ITEM_SIZE {
            return errf!(
                "Type3 Sign encoding size must be {}, got {}",
                TransactionType3::SIGN_ITEM_SIZE,
                sig.size()
            );
        }
        let pk = sig.publickey.as_bytes().to_vec();
        let adr = Address::from(Account::get_address_by_public_key(*sig.publickey));
        present.push(adr);
        if !seen_pubkeys.insert(pk) || !seen_addrs.insert(adr) {
            duplicate.push(adr);
            continue;
        }
        if !required_set.contains(&adr) {
            undeclared.push(adr);
            continue;
        }
        let ckhx = if adr == main_addr { &hxwf } else { &hx };
        if basis::method::verify_signature(ckhx, &adr, sig) {
            valid.push(adr);
        } else {
            invalid.push(adr);
        }
    }

    let mut missing = Vec::new();
    for adr in &required {
        if !seen_addrs.contains(adr) {
            missing.push(*adr);
        }
    }

    sort_addresses(&mut present);
    sort_addresses(&mut valid);
    sort_addresses(&mut missing);
    sort_addresses(&mut invalid);
    sort_addresses(&mut undeclared);
    sort_addresses(&mut duplicate);
    Ok(TxSignatureReport {
        required,
        present,
        valid,
        missing,
        invalid,
        undeclared,
        duplicate,
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

/// Exact Type3 signature verification: SignW2 must match D exactly.
pub fn verify_type3_signatures_exact(tx: &TransactionType3) -> Rerr {
    let d = tx.deterministic_signers()?;
    let signs = tx.signs();
    if signs.len() != d.len() {
        return errf!(
            "Type3 SignW2 length {} != deterministic signer count {}",
            signs.len(),
            d.len()
        );
    }

    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let main_addr = tx.main();
    let mut seen_pubkeys = HashSet::new();
    let mut seen_addrs = HashSet::new();

    for sig in signs {
        if sig.size() != TransactionType3::SIGN_ITEM_SIZE {
            return errf!(
                "Type3 Sign encoding size must be {}, got {}",
                TransactionType3::SIGN_ITEM_SIZE,
                sig.size()
            );
        }
        let pk = sig.publickey.as_bytes().to_vec();
        if !seen_pubkeys.insert(pk) {
            return errf!("Type3 SignW2 contains duplicate public key");
        }
        let adr = Address::from(Account::get_address_by_public_key(*sig.publickey));
        if !seen_addrs.insert(adr) {
            return errf!(
                "Type3 SignW2 contains duplicate signer address {}",
                adr.to_readable()
            );
        }
        if !d.contains(&adr) {
            return errf!("undeclared Type3 signer {}", adr.to_readable());
        }
        let ckhx = if adr == main_addr { &hxwf } else { &hx };
        if !basis::method::verify_signature(ckhx, &adr, sig) {
            return errf!("{} signature verification failed", adr.to_readable());
        }
    }

    if seen_addrs != d {
        return errf!("Type3 signer address set does not equal deterministic set D");
    }
    Ok(())
}


pub fn check_tx_signature(tx: &dyn TransactionRead) -> Ret<HashMap<Address, bool>> {
    if tx.ty() == TransactionType3::TYPE {
        return check_tx_signature_type3(tx);
    }
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

fn check_tx_signature_type3(tx: &dyn TransactionRead) -> Ret<HashMap<Address, bool>> {
    let required = tx.req_sign()?;
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let main_addr = tx.main();
    let signs = tx.signs();
    let mut ckres = HashMap::new();
    let mut seen_pubkeys = HashSet::new();
    let mut seen_addrs = HashSet::new();

    for sig in signs {
        let pk = sig.publickey.as_bytes().to_vec();
        let adr = Address::from(Account::get_address_by_public_key(*sig.publickey));
        if !seen_pubkeys.insert(pk) || !seen_addrs.insert(adr) {
            ckres.insert(adr, false);
            continue;
        }
        if !required.contains(&adr) {
            ckres.insert(adr, false);
            continue;
        }
        let ckhx = if adr == main_addr { &hxwf } else { &hx };
        ckres.insert(adr, basis::method::verify_signature(ckhx, &adr, sig));
    }
    for adr in required {
        ckres.entry(adr).or_insert(false);
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
