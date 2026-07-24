use sys::HybridKeyBlob;
use zeroize::Zeroizing;

pub const KEYSTORE_VERSION: u32 = 3;

const KEYSTORE_MAX_JSON_BYTES: usize = 256 * 1024;
const KEYSTORE_MAX_PASSWORD_BYTES: usize = 1024;
const KEYSTORE_SALT_MIN_BYTES: usize = 8;
const KEYSTORE_SALT_MAX_BYTES: usize = 64;
const KEYSTORE_NONCE_BYTES: usize = 12;
const KEYSTORE_GCM_TAG_BYTES: usize = 16;
const KEYSTORE_ARGON2_MAX_M_COST_KB: u32 = 256 * 1024;
const KEYSTORE_ARGON2_MAX_T_COST: u32 = 16;
const KEYSTORE_ARGON2_MAX_P_COST: u32 = 16;

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
#[derive(Clone)]
pub struct HybridKeystoreV3 {
    pub json: String,
}

impl std::fmt::Debug for HybridKeystoreV3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HybridKeystoreV3")
            .field("json", &"[REDACTED]")
            .field("json_len", &self.json.len())
            .finish()
    }
}

pub fn keystore_export_blob(
    blob: &HybridKeyBlob,
    address: &str,
    pass: &str,
) -> Ret<HybridKeystoreV3> {
    if pass.len() < 8 {
        return errf!("keystore password must be at least 8 characters");
    }
    validate_password_size(pass)?;
    validate_blob_shape(blob)?;
    let kind = match blob.kind {
        1 => "pqckey",
        3 => "hybrid",
        _ => return errf!("unsupported hybrid key kind {}", blob.kind),
    };
    let mut plain = Zeroizing::new(Vec::with_capacity(1 + blob.mldsa_sk.len() + 32));
    plain.push(blob.kind);
    plain.extend_from_slice(&blob.mldsa_sk);
    if let Some(sk) = blob.secp_sk.as_ref() {
        plain.extend_from_slice(sk);
    }
    let salt = random_bytes(16)?;
    let key = derive_key_argon2id(pass, &salt)?;
    let nonce = random_bytes(12)?;
    let ciphertext = aes_gcm_encrypt(&key, &nonce, &plain)?;
    let secp_pubkey = blob.secp_sk.as_ref().map(|sk| {
        let sk = Zeroizing::new(*sk);
        SysAccount::create_by_secret_key_value(*sk)
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
    if json.len() > KEYSTORE_MAX_JSON_BYTES {
        return errf!("keystore JSON too large");
    }
    validate_password_size(pass)?;
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
    if v["kdf"].as_str() != Some("argon2id") {
        return errf!("keystore kdf invalid");
    }
    if v["cipher"].as_str() != Some("aes-256-gcm") {
        return errf!("keystore cipher invalid");
    }
    let salt = hex_field(&v, "kdf_salt")?;
    if !(KEYSTORE_SALT_MIN_BYTES..=KEYSTORE_SALT_MAX_BYTES).contains(&salt.len()) {
        return errf!("keystore salt size invalid");
    }
    let m_cost = bounded_u32_field(&v, "kdf_m_cost_kb", 19456, KEYSTORE_ARGON2_MAX_M_COST_KB)?;
    let t_cost = bounded_u32_field(&v, "kdf_t_cost", 2, KEYSTORE_ARGON2_MAX_T_COST)?;
    let p_cost = bounded_u32_field(&v, "kdf_p_cost", 1, KEYSTORE_ARGON2_MAX_P_COST)?;
    let nonce = hex_field(&v, "cipher_nonce")?;
    if nonce.len() != KEYSTORE_NONCE_BYTES {
        return errf!("keystore nonce size invalid");
    }
    let ciphertext = hex_field(&v, "ciphertext")?;
    let expected_plain_len = expected_plaintext_len(kind)?;
    if ciphertext.len() != expected_plain_len + KEYSTORE_GCM_TAG_BYTES {
        return errf!("keystore ciphertext size invalid");
    }
    let key = derive_key_argon2id_params(pass, &salt, m_cost, t_cost, p_cost)?;
    let plain = aes_gcm_decrypt(&key, &nonce, &ciphertext)?;
    if plain.len() != expected_plain_len {
        return errf!("keystore plaintext size invalid");
    }
    let blob_kind = plain[0];
    if blob_kind != kind {
        return errf!("keystore kind mismatch");
    }
    let sk_len = mldsa65_secret_key_size();
    let mldsa_sk = plain[1..1 + sk_len].to_vec();
    let secp_sk = if kind == 3 {
        let mut sk = Zeroizing::new([0u8; 32]);
        sk.copy_from_slice(&plain[1 + sk_len..1 + sk_len + 32]);
        Some(*sk)
    } else {
        None
    };
    let mldsa_pk = hex_field(&v, "mldsa_pk")?;
    if mldsa_pk.len() != mldsa65_public_key_size() {
        return errf!("keystore mldsa public key size invalid");
    }
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

fn bounded_u32_field(v: &serde_json::Value, key: &str, default: u32, max: u32) -> Ret<u32> {
    let raw = v
        .get(key)
        .and_then(|value| value.as_u64())
        .unwrap_or(default as u64);
    let value = u32::try_from(raw).map_err(|_| format!("keystore field {key} invalid"))?;
    if value == 0 || value > max {
        return errf!("keystore field {} outside safe bounds", key);
    }
    Ok(value)
}

fn validate_password_size(pass: &str) -> Rerr {
    if pass.len() > KEYSTORE_MAX_PASSWORD_BYTES {
        return errf!("keystore password too large");
    }
    Ok(())
}

fn expected_plaintext_len(kind: u8) -> Ret<usize> {
    match kind {
        1 => Ok(1 + mldsa65_secret_key_size()),
        3 => Ok(1 + mldsa65_secret_key_size() + 32),
        _ => errf!("unsupported hybrid key kind {}", kind),
    }
}

fn validate_blob_shape(blob: &HybridKeyBlob) -> Rerr {
    if blob.mldsa_sk.len() != mldsa65_secret_key_size() {
        return errf!("hybrid key blob mldsa secret size invalid");
    }
    if blob.mldsa_pk.len() != mldsa65_public_key_size() {
        return errf!("hybrid key blob mldsa public size invalid");
    }
    match (blob.kind, blob.secp_sk.is_some()) {
        (1, false) | (3, true) => Ok(()),
        (1, true) => errf!("pqc key blob must not contain a secp secret"),
        (3, false) => errf!("hybrid key blob missing secp secret"),
        (kind, _) => errf!("unsupported hybrid key kind {}", kind),
    }
}

fn derive_key_argon2id(pass: &str, salt: &[u8]) -> Ret<Zeroizing<[u8; 32]>> {
    derive_key_argon2id_params(pass, salt, 19456, 2, 1)
}

fn derive_key_argon2id_params(
    pass: &str,
    salt: &[u8],
    m_cost_kb: u32,
    t_cost: u32,
    p_cost: u32,
) -> Ret<Zeroizing<[u8; 32]>> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(m_cost_kb, t_cost, p_cost, Some(32))
        .map_err(|e: argon2::Error| e.to_string())?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(pass.as_bytes(), salt, &mut *key)
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

fn aes_gcm_decrypt(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Ret<Zeroizing<Vec<u8>>> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    if nonce.len() != 12 {
        return errf!("aes-gcm nonce must be 12 bytes");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce = Nonce::from_slice(nonce);
    cipher
        .decrypt(nonce, ciphertext)
        .map(Zeroizing::new)
        .map_err(|_error: aes_gcm::Error| {
            "keystore decrypt failed: bad password or corrupted data".to_string()
        })
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
    fn keystore_v3_rejects_bad_password() {
        let acc = HybridAccount::create_pqc_randomly(&|_| Ok(())).unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "correct-password").unwrap();
        assert!(keystore_unlock_blob(&ks.json, "wrong-password").is_err());
    }

    #[test]
    fn keystore_debug_redacts_password_verifier_material() {
        let acc = HybridAccount::create_pqc_randomly(&|_| Ok(())).unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "correct-password").unwrap();
        let debug = format!("{ks:?}");

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("ciphertext"));
        assert!(!debug.contains(acc.readable()));
    }

    #[test]
    fn keystore_v3_rejects_malformed_nonce_without_panicking() {
        let acc = HybridAccount::create_pqc_randomly(&|_| Ok(())).unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "correct-password").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&ks.json).unwrap();
        value["cipher_nonce"] = serde_json::json!("00");

        let result = std::panic::catch_unwind(|| {
            keystore_unlock_blob(&value.to_string(), "correct-password")
        });
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn keystore_v3_rejects_unbounded_kdf_cost_before_derivation() {
        let acc = HybridAccount::create_pqc_randomly(&|_| Ok(())).unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "correct-password").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&ks.json).unwrap();
        value["kdf_m_cost_kb"] = serde_json::json!(u64::MAX);

        let error = keystore_unlock_blob(&value.to_string(), "correct-password").unwrap_err();
        assert!(error.contains("kdf_m_cost_kb"));
    }

    #[test]
    fn keystore_v3_rejects_malformed_ciphertext_size_before_derivation() {
        let acc = HybridAccount::create_hybrid_randomly(&|b| {
            for (i, x) in b.iter_mut().enumerate() {
                *x = (i as u8).wrapping_add(7);
            }
            Ok(())
        })
        .unwrap();
        let blob = acc.export_key_blob().unwrap();
        let ks = keystore_export_blob(&blob, acc.readable(), "correct-password").unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&ks.json).unwrap();
        value["ciphertext"] = serde_json::json!("00");

        let error = keystore_unlock_blob(&value.to_string(), "correct-password").unwrap_err();
        assert!(error.contains("ciphertext size"));
    }

    #[test]
    fn secret_intermediates_have_drop_cleanup_types() {
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}

        assert_zeroize_on_drop::<Zeroizing<[u8; 32]>>();
        assert_zeroize_on_drop::<Zeroizing<Vec<u8>>>();
        assert_zeroize_on_drop::<aes::Aes256>();

        let key = derive_key_argon2id("correct-password", &[7u8; 16]).unwrap();
        let nonce = [9u8; KEYSTORE_NONCE_BYTES];
        let ciphertext = aes_gcm_encrypt(&key, &nonce, b"secret plaintext").unwrap();
        let plaintext = aes_gcm_decrypt(&key, &nonce, &ciphertext).unwrap();
        assert_eq!(&*plaintext, b"secret plaintext");
    }
}
