use sys::{HybridAccount, HybridAccountKind};

#[derive(Clone)]
#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct HybridAccountInfo {
    pub kind: String,
    pub address: String,
    pub address_hex: String,
    pub address_version: u8,
    pub mldsa_pubkey: String,
    pub secp_pubkey: String,
    pub alg_id: u8,
}

fn hybrid_info_from(acc: &HybridAccount) -> HybridAccountInfo {
    let kind = match acc.kind() {
        HybridAccountKind::PqcOnly => "pqckey",
        HybridAccountKind::Hybrid => "hybrid",
    };
    let secp_pubkey = acc
        .secp_account()
        .map(|a| hex::encode(a.public_key().serialize_compressed()))
        .unwrap_or_default();
    HybridAccountInfo {
        kind: kind.to_owned(),
        address: acc.readable().to_owned(),
        address_hex: hex::encode(acc.address()),
        address_version: acc.address()[0],
        mldsa_pubkey: hex::encode(acc.mldsa_public_key_bytes()),
        secp_pubkey,
        alg_id: acc.sign_alg_id(),
    }
}

pub fn sdk_random_fill(buf: &mut [u8]) -> Rerr {
    random_fill(buf)
}

#[wasm_bindgen]
pub fn create_pqc_account() -> Ret<HybridAccountInfo> {
    let acc = HybridAccount::create_pqc_randomly(&sdk_random_fill)?;
    Ok(hybrid_info_from(&acc))
}

#[wasm_bindgen]
pub fn create_hybrid_account() -> Ret<HybridAccountInfo> {
    let acc = HybridAccount::create_hybrid_randomly(&sdk_random_fill)?;
    Ok(hybrid_info_from(&acc))
}

#[wasm_bindgen]
pub fn create_hybrid_from_privakey(prikey_hex: &str) -> Ret<HybridAccountInfo> {
    let secp = q_acc!(prikey_hex);
    let acc = HybridAccount::create_hybrid_from_secp(secp)?;
    Ok(hybrid_info_from(&acc))
}

#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct HybridKeystoreExport {
    pub json: String,
    pub address: String,
    pub kind: String,
}

#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct HybridAccountWithKeystore {
    pub info: HybridAccountInfo,
    pub keystore: String,
}

#[wasm_bindgen]
pub fn create_pqc_account_keystore(password: &str) -> Ret<HybridAccountWithKeystore> {
    let acc = HybridAccount::create_pqc_randomly(&sdk_random_fill)?;
    let blob = acc.export_key_blob()?;
    let ks = keystore_export_blob(&blob, acc.readable(), password)?;
    Ok(HybridAccountWithKeystore {
        info: hybrid_info_from(&acc),
        keystore: ks.json,
    })
}

#[wasm_bindgen]
pub fn create_hybrid_account_keystore(password: &str, prikey_hex: &str) -> Ret<HybridAccountWithKeystore> {
    let acc = if prikey_hex.len() == 64 {
        let secp = q_acc!(prikey_hex);
        HybridAccount::create_hybrid_from_secp(secp)?
    } else {
        HybridAccount::create_hybrid_randomly(&sdk_random_fill)?
    };
    let blob = acc.export_key_blob()?;
    let ks = keystore_export_blob(&blob, acc.readable(), password)?;
    Ok(HybridAccountWithKeystore {
        info: hybrid_info_from(&acc),
        keystore: ks.json,
    })
}

#[wasm_bindgen]
pub fn export_hybrid_keystore(json: &str, password: &str, new_password: &str) -> Ret<HybridKeystoreExport> {
    let acc = hybrid_account_from_keystore(json, password)?;
    let blob = acc.export_key_blob()?;
    let ks = keystore_export_blob(&blob, acc.readable(), new_password)?;
    let info = hybrid_info_from(&acc);
    Ok(HybridKeystoreExport {
        json: ks.json,
        address: info.address,
        kind: info.kind,
    })
}

pub fn hybrid_account_from_keystore(json: &str, password: &str) -> Ret<HybridAccount> {
    let blob = keystore_unlock_blob(json, password)?;
    HybridAccount::from_key_blob(&blob)
}

#[wasm_bindgen]
pub fn unlock_hybrid_keystore(json: &str, password: &str) -> Ret<HybridAccountInfo> {
    let acc = hybrid_account_from_keystore(json, password)?;
    Ok(hybrid_info_from(&acc))
}

#[wasm_bindgen]
pub fn address_version_label(address: &str) -> Ret<String> {
    let addr = q_adr!(address);
    Ok(match addr.version() {
        Address::PRIVAKEY => "privakey (v0)".to_owned(),
        Address::PQCKEY => "pqckey (v6)".to_owned(),
        Address::HYBRID => "hybrid (v7)".to_owned(),
        Address::CONTRACT => "contract (v1)".to_owned(),
        Address::SCRIPTMH => "scriptmh (v5)".to_owned(),
        v => format!("unknown (v{v})"),
    })
}