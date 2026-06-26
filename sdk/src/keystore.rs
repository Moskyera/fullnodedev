use sys::HybridKeyBlob;

pub const KEYSTORE_VERSION: u32 = 3;

/// JSON keystore v3 (browser-wallet friendly).
///
/// ```json
/// {
///   "version": 3,
///   "kind": "pqckey" | "hybrid",
///   "address": "base58check",
///   "mldsa_pk": "hex",
///   "secp_pubkey": "hex|null",
///   "kdf": "argon2id",
///   "kdf_salt": "hex",
///   "kdf_m_cost_kb": 19456,
///   "kdf_t_cost": 2,
///   "kdf_p_cost": 1,
///   "cipher": "aes-256-gcm",
///   "cipher_nonce": "hex",
///   "ciphertext": "hex"
/// }
/// ```
#[derive(Debug, Clone)]
pub struct HybridKeystoreV3 {
    pub json: String,
}

pub fn keystore_export_blob(blob: &HybridKeyBlob, address: &str, pass: &str) -> Ret<HybridKeystoreV3> {
    if pass.len() < 8 {
        return errf!("keystore password must be at least 8 characters");
    }
    let kind = match blob.kind {
        1 => "pqckey",
        3 => "hybrid",
        _ => return errf!("unsupported hybrid key kind {}", blob.kind),
    };
    let mut plain = Vec::with_capacity(1 + blob.mldsa_sk.len() + 33);
    plain.push(blob.kind);
    plain.extend_from_slice(&blob.mldsa_sk);
    if let Some(sk) = blob.secp_sk {
        plain.extend_from_slice(&sk);
    }
    let salt = random_bytes(16)?;
    let key = derive_key_argon2id(pass, &salt)?;
    let nonce = random_bytes(12)?;
    let ciphertext = aes_gcm_encrypt(&key, &nonce, &plain)?;
    let secp_pubkey = blob.secp_sk.map(|sk| {
        SysAccount::create_by_secret_key_value(sk)
            .map(|a| hex::encode(a.public_key().serialize_compressed()))
            .unwrap_or_default()
    });
    let json = serde_json::json!({
        "version": KEYSTORE_VERSION,
        "kind": kind,
        "address": address,
        "mldsa_pk": hex::encode(&blob.mldsa_pk),
        "secp_pubkey": secp_pubkey,
        "kdf": "argon2id",
        "kdf_salt": hex::encode(&salt),
        "kdf_m_cost_kb": 19456u32,
        "kdf_t_cost": 2u32,
        "kdf_p_cost": 1u32,
        "cipher": "aes-256-gcm",
        "cipher_nonce": hex::encode(&nonce),
        "ciphertext": hex::encode(&ciphertext),
    });
    Ok(HybridKeystoreV3 {
        json: json.to_string(),
    })
}

pub fn keystore_unlock_blob(json: &str, pass: &str) -> Ret<HybridKeyBlob> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e: serde_json::Error| e.to_string())?;
    if v["version"].as_u64() != Some(KEYSTORE_VERSION as u64) {
        return errf!("keystore version must be {}", KEYSTORE_VERSION);
    }
    let kind = match v["kind"].as_str() {
        Some("pqckey") => 1u8,
        Some("hybrid") => 3u8,
        _ => return errf!("keystore kind invalid"),
    };
    let salt = hex_field(&v, "kdf_salt")?;
    let m_cost = v["kdf_m_cost_kb"].as_u64().unwrap_or(19456) as u32;
    let t_cost = v["kdf_t_cost"].as_u64().unwrap_or(2) as u32;
    let p_cost = v["kdf_p_cost"].as_u64().unwrap_or(1) as u32;
    let nonce = hex_field(&v, "cipher_nonce")?;
    let ciphertext = hex_field(&v, "ciphertext")?;
    let key = derive_key_argon2id_params(pass, &salt, m_cost, t_cost, p_cost)?;
    let plain = aes_gcm_decrypt(&key, &nonce, &ciphertext)?;
    if plain.is_empty() {
        return errf!("keystore plaintext empty");
    }
    let blob_kind = plain[0];
    if blob_kind != kind {
        return errf!("keystore kind mismatch");
    }
    let sk_len = mldsa65_secret_key_size();
    if plain.len() < 1 + sk_len {
        return errf!("keystore plaintext too short");
    }
    let mldsa_sk = plain[1..1 + sk_len].to_vec();
    let secp_sk = if kind == 3 {
        if plain.len() != 1 + sk_len + 32 {
            return errf!("hybrid keystore plaintext size invalid");
        }
        let mut sk = [0u8; 32];
        sk.copy_from_slice(&plain[1 + sk_len..1 + sk_len + 32]);
        Some(sk)
    } else {
        None
    };
    let mldsa_pk = hex_field(&v, "mldsa_pk")?;
    Ok(HybridKeyBlob {
        kind,
        mldsa_sk,
        secp_sk,
        mldsa_pk,
    })
}

fn hex_field(v: &serde_json::Value, key: &str) -> Ret<Vec<u8>> {
    let s = v
        .get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("keystore field {} missing", key))?;
    hex::decode(s).map_err(|e: hex::FromHexError| e.to_string())
}

fn derive_key_argon2id(pass: &str, salt: &[u8]) -> Ret<[u8; 32]> {
    derive_key_argon2id_params(pass, salt, 19456, 2, 1)
}

fn derive_key_argon2id_params(
    pass: &str,
    salt: &[u8],
    m_cost_kb: u32,
    t_cost: u32,
    p_cost: u32,
) -> Ret<[u8; 32]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(m_cost_kb, t_cost, p_cost, Some(32))
        .map_err(|e: argon2::Error| e.to_string())?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(pass.as_bytes(), salt, &mut key)
        .map_err(|e: argon2::Error| e.to_string())?;
    Ok(key)
}

fn aes_gcm_encrypt(key: &[u8; 32], nonce: &[u8], plain: &[u8]) -> Ret<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    if nonce.len() != 12 {
        return errf!("aes-gcm nonce must be 12 bytes");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce = Nonce::from_slice(nonce);
    cipher
        .encrypt(nonce, plain)
        .map_err(|e: aes_gcm::Error| e.to_string())
}

fn aes_gcm_decrypt(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Ret<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    if nonce.len() != 12 {
        return errf!("aes-gcm nonce must be 12 bytes");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce = Nonce::from_slice(nonce);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e: aes_gcm::Error| "keystore decrypt failed: bad password or corrupted data".to_string())
}

pub fn random_bytes(n: usize) -> Ret<Vec<u8>> {
    let mut buf = vec![0u8; n];
    random_fill(&mut buf)?;
    Ok(buf)
}

pub fn random_fill(buf: &mut [u8]) -> Rerr {
    getrandom::fill(buf).map_err(|e: getrandom::Error| e.to_string())
}

#[cfg(test)]
mod keystore_tests {
    use super::*;
    use sys::HybridAccount;

    #[test]
    fn keystore_v3_roundtrip_pqc() {
        let acc = HybridAccount::create_pqc_randomly(&|b| {
            for (i, x) in b.iter_mut().enumerate() {
                *x = i as u8;
            }
            Ok(())
        })
        .unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "test-password-123").unwrap();
        let got = keystore_unlock_blob(&ks.json, "test-password-123").unwrap();
        let acc2 = HybridAccount::from_key_blob(&got).unwrap();
        assert_eq!(acc.address(), acc2.address());
    }

    #[test]
    fn keystore_v3_roundtrip_hybrid() {
        let acc = HybridAccount::create_hybrid_randomly(&|b| {
            for (i, x) in b.iter_mut().enumerate() {
                *x = (i as u8).wrapping_add(9);
            }
            Ok(())
        })
        .unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "hybrid-pass-12345").unwrap();
        assert!(ks.json.contains("\"kind\":\"hybrid\""));
        let got = keystore_unlock_blob(&ks.json, "hybrid-pass-12345").unwrap();
        let acc2 = HybridAccount::from_key_blob(&got).unwrap();
        assert_eq!(acc.address(), acc2.address());
        assert!(acc2.is_hybrid());
    }

    #[test]
    #[ignore = "demo: cargo test -p sdk demo_print_hybrid_keystore -- --ignored --exact --nocapture"]
    fn demo_print_hybrid_keystore() {
        let acc = HybridAccount::create_hybrid_randomly(&|b| {
            for (i, x) in b.iter_mut().enumerate() {
                *x = (i as u8).wrapping_add(7);
            }
            Ok(())
        })
        .unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "hybrid-pass-12345").unwrap();
        println!("HYBRID_ADDRESS={}", acc.readable());
        println!("KEYSTORE_JSON={}", ks.json);
    }

    #[test]
    fn keystore_v3_rejects_bad_password() {
        let acc = HybridAccount::create_pqc_randomly(&|_| Ok(())).unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "correct-password").unwrap();
        assert!(keystore_unlock_blob(&ks.json, "wrong-password").is_err());
    }
}