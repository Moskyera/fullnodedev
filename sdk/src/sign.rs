
#[derive(Default)]
#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct SignTxParam {
    pub prikey: String,
    pub body: String,
    pub hybrid_keystore: String,
    pub keystore_pass: String,
}

#[wasm_bindgen]
impl SignTxParam {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Default)]
#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct SignTxV4Param {
    pub body: String,
    pub hybrid_keystore: String,
    pub keystore_pass: String,
}

#[wasm_bindgen]
impl SignTxV4Param {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }
}

#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct SignTxResult {
    pub hash: String,
    pub hash_with_fee: String,
    pub body: String,
    pub signature: String,
    pub timestamp: u64,
    pub tx_type: u8,
    pub sign_alg: u8,
}

fn parse_tx_body(body_hex: &str) -> Ret<(Box<dyn basis::interface::Transaction>, usize)> {
    let Ok(body) = hex::decode(body_hex) else {
        return errf!("tx body hex decode failed");
    };
    protocol::transaction::transaction_create(&body).map_err(|e| e.to_string())
}

fn finish_sign_result(
    trs: &dyn basis::interface::TransactionRead,
    body_hex: String,
    signature_hex: String,
    sign_alg: u8,
) -> SignTxResult {
    SignTxResult {
        hash: trs.hash().to_hex(),
        hash_with_fee: trs.hash_with_fee().to_hex(),
        body: body_hex,
        signature: signature_hex,
        timestamp: trs.timestamp().uint(),
        tx_type: trs.ty(),
        sign_alg,
    }
}

/*
    sign one tx (legacy Types 1–3, or Type 4 when hybrid_keystore is set)
*/
#[wasm_bindgen]
pub fn sign_transaction(param: SignTxParam) -> Ret<SignTxResult> {
    use protocol::transaction::TransactionType4;

    let (mut trs, _) = parse_tx_body(&param.body)?;
    if trs.ty() == TransactionType4::TYPE {
        if param.hybrid_keystore.is_empty() {
            return errf!("type 4 transaction requires hybrid_keystore");
        }
        return sign_transaction_v4(SignTxV4Param {
            body: param.body,
            hybrid_keystore: param.hybrid_keystore,
            keystore_pass: param.keystore_pass,
        });
    }
    let acc = q_acc!(param.prikey);
    let signature = trs.fill_sign(&acc)?;
    let body_hex = trs.serialize().to_hex();
    Ok(finish_sign_result(
        trs.as_read(),
        body_hex,
        signature.signature.to_hex(),
        0,
    ))
}

#[wasm_bindgen]
pub fn sign_transaction_v4(param: SignTxV4Param) -> Ret<SignTxResult> {
    use basis::interface::Transaction;
    use protocol::transaction::TransactionType4;

    if param.hybrid_keystore.is_empty() {
        return errf!("hybrid_keystore required");
    }
    let (mut trs, _) = parse_tx_body(&param.body)?;
    if trs.ty() != TransactionType4::TYPE {
        return errf!("sign_transaction_v4 requires transaction type 4");
    }
    let hybrid = q_hybrid_acc!(param.hybrid_keystore, param.keystore_pass);
    let signobj = trs.fill_hybrid_sign(&hybrid)?;
    let body_hex = trs.serialize().to_hex();
    Ok(finish_sign_result(
        trs.as_read(),
        body_hex,
        signobj.body.to_hex(),
        signobj.alg_id(),
    ))
}