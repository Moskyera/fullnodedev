use field::sign_alg;
use field::{Address, Hash, HybridSign};
use sys::Account;

pub fn verify_hybrid_signature(hash: &Hash, addr: &Address, sign: &HybridSign) -> bool {
    if sign.check_wire().is_err() {
        return false;
    }
    let alg = sign.alg_id();
    let body = sign.body_bytes();
    let Ok(curaddr) = sys::address_from_hybrid_sign_body(alg, body) else {
        return false;
    };
    if addr.as_array() != &curaddr {
        return false;
    }
    let msg = hash.as_array();
    match (alg, addr.version()) {
        (sign_alg::LEGACY_SECP, v) if v == Address::PRIVAKEY => {
            if body.len() != sign_alg::BODY_LEGACY_SECP {
                return false;
            }
            let mut pk = [0u8; sign_alg::SECP_PK_SIZE];
            pk.copy_from_slice(&body[..sign_alg::SECP_PK_SIZE]);
            let sig: [u8; sign_alg::SECP_SIG_SIZE] = body[sign_alg::SECP_PK_SIZE..]
                .try_into()
                .unwrap_or([0u8; sign_alg::SECP_SIG_SIZE]);
            Account::verify_signature(msg, &pk, &sig)
        }
        (sign_alg::MLDSA65, v) if v == Address::PQCKEY => {
            let pk = &body[..sign_alg::MLDSA65_PK_SIZE];
            let sig = &body[sign_alg::MLDSA65_PK_SIZE..];
            sys::verify_mldsa65_detached(msg, pk, sig)
        }
        (sign_alg::HYBRID_SECP_MLDSA65, v) if v == Address::HYBRID => {
            let secp_pk: [u8; sign_alg::SECP_PK_SIZE] =
                match body[..sign_alg::SECP_PK_SIZE].try_into() {
                    Ok(v) => v,
                    Err(_) => return false,
                };
            let secp_sig: [u8; sign_alg::SECP_SIG_SIZE] = match body
                [sign_alg::SECP_PK_SIZE..sign_alg::SECP_PK_SIZE + sign_alg::SECP_SIG_SIZE]
                .try_into()
            {
                Ok(v) => v,
                Err(_) => return false,
            };
            if !Account::verify_signature(msg, &secp_pk, &secp_sig) {
                return false;
            }
            let mldsa_off = sign_alg::SECP_PK_SIZE + sign_alg::SECP_SIG_SIZE;
            let mldsa_pk = &body[mldsa_off..mldsa_off + sign_alg::MLDSA65_PK_SIZE];
            let mldsa_sig = &body[mldsa_off + sign_alg::MLDSA65_PK_SIZE..];
            sys::verify_mldsa65_detached(msg, mldsa_pk, mldsa_sig)
        }
        _ => false,
    }
}

pub fn hybrid_sign_address(sign: &HybridSign) -> Ret<Address> {
    sign.check_wire()?;
    let addr = sys::address_from_hybrid_sign_body(sign.alg_id(), sign.body_bytes())?;
    Ok(Address::from(addr))
}