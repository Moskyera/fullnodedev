fn create_coin_transfer(_ctx: &ApiExecCtx, mut req: ApiRequest) -> ApiResponse {
    use basis::interface::Transaction;
    use protocol::action::*;
    let fee = q_string(&req, "fee", "");
    let to_address = q_string(&req, "to_address", "");
    let timestamp = req.query_u64("timestamp", 0);
    let hacash = q_string(&req, "hacash", "");
    let tx_type = req.query_u64("tx_type", 2);
    let gas_max = req.query_u64("gas_max", 0) as u8;

    let Ok(toaddr) = Address::from_readable(&to_address) else {
        return api_error("to_address format invalid");
    };
    let Ok(fee) = Amount::from(&fee) else {
        return api_error("fee format invalid");
    };
    if hacash.is_empty() {
        return api_error("hacash amount required");
    }
    let Ok(hac) = Amount::from(&hacash) else {
        return api_error("hacash amount format invalid");
    };

    if tx_type == TransactionType4::TYPE as u64 {
        return create_coin_transfer_type4(&mut req, toaddr, fee, hac, timestamp, gas_max);
    }

    let main_prikey = take_secret_query(&mut req, "main_prikey");
    let from_prikey = take_secret_query(&mut req, "from_prikey");
    let satoshi = req.query_u64("satoshi", 0);
    let diamonds = q_string(&req, "diamonds", "");

    let Ok(main_acc) = Account::create_by(&main_prikey) else {
        return api_error("main_prikey format invalid");
    };

    let mut from_acc = main_acc.clone();
    if !from_prikey.is_empty() {
        let Ok(acc) = Account::create_by(&from_prikey) else {
            return api_error("from_prikey format invalid");
        };
        from_acc = acc;
    }
    let is_from = from_acc != main_acc;
    let addr = Address::from(main_acc.address().clone());
    let fromaddr = Address::from(from_acc.address().clone());

    let mut tx = TransactionType2::new_by(addr, fee, curtimes());
    if timestamp > 0 {
        tx.timestamp = Timestamp::from(timestamp);
    }

    if satoshi > 0 {
        let sat = Satoshi::from(satoshi);
        let act: Box<dyn Action> = if is_from {
            let mut obj = SatFromToTrs::new();
            obj.from = AddrOrPtr::from_addr(fromaddr.clone());
            obj.to = AddrOrPtr::from_addr(toaddr.clone());
            obj.satoshi = sat;
            Box::new(obj)
        } else {
            let mut obj = SatToTrs::new();
            obj.to = AddrOrPtr::from_addr(toaddr.clone());
            obj.satoshi = sat;
            Box::new(obj)
        };
        if tx.push_action(act).is_err() {
            return api_error("push sat action failed");
        }
    }

    if diamonds.len() >= DiamondName::SIZE {
        let Ok(dialist) = DiamondNameListMax200::from_readable(&diamonds) else {
            return api_error("diamonds format invalid");
        };
        let act: Box<dyn Action> = if is_from {
            let mut obj = DiaFromToTrs::new();
            obj.from = AddrOrPtr::from_addr(fromaddr.clone());
            obj.to = AddrOrPtr::from_addr(toaddr.clone());
            obj.diamonds = dialist;
            Box::new(obj)
        } else if dialist.length() == 1 {
            let mut obj = DiaSingleTrs::new();
            obj.to = AddrOrPtr::from_addr(toaddr.clone());
            obj.diamond = DiamondName::from(*dialist.as_list()[0]);
            Box::new(obj)
        } else {
            let mut obj = DiaToTrs::new();
            obj.to = AddrOrPtr::from_addr(toaddr.clone());
            obj.diamonds = dialist;
            Box::new(obj)
        };
        if tx.push_action(act).is_err() {
            return api_error("push diamond action failed");
        }
    }

    let act: Box<dyn Action> = if is_from {
        let mut obj = HacFromToTrs::new();
        obj.from = AddrOrPtr::from_addr(fromaddr.clone());
        obj.to = AddrOrPtr::from_addr(toaddr.clone());
        obj.hacash = hac;
        Box::new(obj)
    } else {
        let mut obj = HacToTrs::new();
        obj.to = AddrOrPtr::from_addr(toaddr.clone());
        obj.hacash = hac;
        Box::new(obj)
    };
    if tx.push_action(act).is_err() {
        return api_error("push hac action failed");
    }

    if let Err(e) = tx.fill_sign(&main_acc) {
        return api_error(&format!("fill main sign failed: {}", e));
    }
    if is_from {
        if let Err(e) = tx.fill_sign(&from_acc) {
            return api_error(&format!("fill from sign failed: {}", e));
        }
    }

    api_data(serde_json::Map::from_iter([
        ("hash".to_owned(), json!(tx.hash().to_hex())),
        (
            "hash_with_fee".to_owned(),
            json!(tx.hash_with_fee().to_hex()),
        ),
        ("timestamp".to_owned(), json!(tx.timestamp().uint())),
        ("type".to_owned(), json!(tx.ty())),
        ("body".to_owned(), json!(tx.serialize().to_hex())),
    ]))
}

fn create_coin_transfer_type4(
    req: &mut ApiRequest,
    toaddr: Address,
    fee: Amount,
    hac: Amount,
    timestamp: u64,
    gas_max: u8,
) -> ApiResponse {
    use protocol::action::*;
    let keystore = take_hybrid_keystore_from_req(req);
    let pass = take_secret_query(req, "keystore_pass");
    if keystore.is_empty() {
        return api_error("type 4 transfer requires hybrid_keystore (query param or JSON body)");
    }
    let Ok(blob) = sdk::keystore_unlock_blob(&keystore, &pass) else {
        return api_error("hybrid keystore unlock failed");
    };
    let Ok(hybrid) = HybridAccount::from_key_blob(&blob) else {
        return api_error("hybrid key material invalid");
    };
    let mainaddr = Address::from(*hybrid.address());
    if !mainaddr.is_pqckey() && !mainaddr.is_hybrid() {
        return api_error("main keystore address must be pqckey or hybrid");
    }

    let ts = if timestamp > 0 { timestamp } else { curtimes() };
    let mut tx = TransactionType4::new_by(mainaddr, fee, ts);
    tx.gas_max = Uint1::from(gas_max);
    if let Err(e) = tx.push_action(Box::new(HacToTrs::create_by(toaddr, hac))) {
        return api_error(&format!("push hac action failed: {}", e));
    }
    if let Err(e) = tx.fill_hybrid_sign(&hybrid) {
        return api_error(&format!("fill hybrid sign failed: {}", e));
    }

    api_data(serde_json::Map::from_iter([
        ("hash".to_owned(), json!(tx.hash().to_hex())),
        (
            "hash_with_fee".to_owned(),
            json!(tx.hash_with_fee().to_hex()),
        ),
        ("timestamp".to_owned(), json!(tx.timestamp().uint())),
        ("type".to_owned(), json!(tx.ty())),
        ("main_address".to_owned(), json!(mainaddr.to_readable())),
        ("address_version".to_owned(), json!(mainaddr.version())),
        ("body".to_owned(), json!(tx.serialize().to_hex())),
    ]))
}

fn create_coin_transfer_v4(_ctx: &ApiExecCtx, mut req: ApiRequest) -> ApiResponse {
    let fee = q_string(&req, "fee", "");
    let to_address = q_string(&req, "to_address", "");
    let timestamp = req.query_u64("timestamp", 0);
    let hacash = q_string(&req, "hacash", "");
    let gas_max = req.query_u64("gas_max", 0) as u8;

    let Ok(toaddr) = Address::from_readable(&to_address) else {
        return api_error("to_address format invalid");
    };
    let Ok(fee) = Amount::from(&fee) else {
        return api_error("fee format invalid");
    };
    if hacash.is_empty() {
        return api_error("hacash amount required");
    }
    let Ok(hac) = Amount::from(&hacash) else {
        return api_error("hacash amount format invalid");
    };

    create_coin_transfer_type4(&mut req, toaddr, fee, hac, timestamp, gas_max)
}
