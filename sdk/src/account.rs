
#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct Account {
    pub prikey: String,
    pub pubkey: String,
    pub address: String,
    pub address_hex: String,
}

/*
    stuff is private key or password
*/
#[wasm_bindgen]
pub fn create_account(pass: &str) -> Ret<Account> {
    SysAccount::create_by(pass).map(|acc| Account {
        prikey: hex::encode(&acc.secret_key().serialize()),
        pubkey: hex::encode(&acc.public_key().serialize_compressed()),
        address_hex: hex::encode(acc.address()),
        address: acc.readable().to_owned(),
    })
}

#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct VerifyAddressResult {
    pub ok: bool,
    pub error: String,
    pub version: u8,
    pub version_label: String,
}

#[wasm_bindgen]
pub fn verify_address(pass: &str) -> VerifyAddressResult {
    let re = |e: String| VerifyAddressResult {
        ok: false,
        error: e,
        version: 255,
        version_label: String::new(),
    };

    let addr = match Address::from_readable(pass) {
        Ok(a) => a,
        Err(e) => return re(e),
    };

    if let Err(e) = addr.check_version() {
        return re(e);
    }

    let version = addr.version();
    let version_label = match version {
        Address::PRIVAKEY => "privakey (v0)",
        Address::PQCKEY => "pqckey (v6)",
        Address::HYBRID => "hybrid (v7)",
        Address::CONTRACT => "contract (v1)",
        Address::SCRIPTMH => "scriptmh (v5)",
        v => return re(format!("unknown address version {v}")),
    }
    .to_owned();

    VerifyAddressResult {
        ok: true,
        error: String::new(),
        version,
        version_label,
    }
}