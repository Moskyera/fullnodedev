use std::collections::HashMap;

use basis::method::{hybrid_sign_address, verify_hybrid_signature};
use field::{Address, Hash, HybridSign};

pub fn verify_tx_hybrid_signature(tx: &dyn TransactionRead) -> Rerr {
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let signs = tx.hybrid_signs();
    let addrs = tx.req_sign()?;
    let main_addr = tx.main();
    for adr in addrs {
        let ckhx = if adr == main_addr {
            &hxwf
        } else {
            &hx
        };
        verify_one_hybrid_sign(ckhx, &adr, signs)?;
    }
    Ok(())
}

pub fn verify_target_hybrid_signature(adr: &Address, tx: &dyn TransactionRead) -> Ret<bool> {
    if adr.is_privakey_unknown() {
        return errf!(
            "address {} is a system address (value < u32::MAX) with unknown private key, cannot sign",
            adr
        );
    }
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let signs = tx.hybrid_signs();
    let main_addr = tx.main();
    let ckhx = if *adr == main_addr {
        &hxwf
    } else {
        &hx
    };
    verify_one_hybrid_sign(ckhx, adr, signs)
}

pub fn verify_one_hybrid_sign(hash: &Hash, addr: &Address, signs: &Vec<HybridSign>) -> Ret<bool> {
    if addr.is_privakey_unknown() {
        return errf!(
            "address {} is a system address (value < u32::MAX) with unknown private key, cannot sign",
            addr
        );
    }
    if !addr.is_user_signing_address() {
        return errf!("address {} is not a user signing address", addr);
    }
    for sig in signs {
        if verify_hybrid_signature(hash, addr, sig) {
            return Ok(true);
        }
    }
    errf!("{} hybrid signature verification failed", addr)
}

pub fn check_tx_hybrid_signature(tx: &dyn TransactionRead) -> Ret<HashMap<Address, bool>> {
    let hx = tx.hash();
    let hxwf = tx.hash_with_fee();
    let signs = tx.hybrid_signs();
    let addrs = tx.req_sign()?;
    let main_addr = tx.main();
    let mut ckres = HashMap::new();
    for sig in signs {
        if let Ok(adr) = hybrid_sign_address(sig) {
            ckres.insert(adr, true);
        }
    }
    for adr in addrs {
        let ckhx = if adr == main_addr {
            &hxwf
        } else {
            &hx
        };
        let mut sigok = false;
        if let Ok(yes) = verify_one_hybrid_sign(ckhx, &adr, signs) {
            if yes {
                sigok = true;
            }
        }
        ckres.insert(adr, sigok);
    }
    Ok(ckres)
}

