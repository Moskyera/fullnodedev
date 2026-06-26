

#[derive(Default)]
#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct CoinTransferParam {
    pub main_prikey: String,
    pub from_prikey: String,
    pub fee:         String,
    pub to_address:  String,
    pub timestamp:   u64,
    // coin
    pub hacash:      String,
    pub satoshi:     u64,
    pub diamonds:    String,
    // util
    pub chain_id:    u64,
}



#[wasm_bindgen]
impl CoinTransferParam {

    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }
}



#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct CoinTransferResult {
    pub hash:              String,
    pub hash_with_fee:     String,
    pub body:          String, // tx body with signature
    pub timestamp:     u64,
}







/*
    stuff is private key or password
*/
#[wasm_bindgen]
pub fn create_coin_transfer(param: CoinTransferParam) -> Ret<CoinTransferResult> {

    use basis::interface::*;
    use protocol::transaction::*;
    use protocol::action::*;

    let main = q_acc!(param.main_prikey);
    let mainaddr = Address::from(main.address().clone());
    let mut from = main.clone();
    if ! param.from_prikey.is_empty() {
        from = q_acc!(param.from_prikey);
    }
    let fromaddr = Address::from(from.address().clone());
    let other_from = from != main;
    // let _from = q_acc!(param.from_prikey);
    let fee = q_amt!(param.fee);
    let toaddr = q_adr!(param.to_address);
    let ts = if param.timestamp == 0 {
        curtimes()
    } else {
        param.timestamp
    };

    // create trs
    let mut trsobj = TransactionType2::new_by(mainaddr, fee, ts);
    // append action
    // hac
    if ! param.hacash.is_empty() {
        let hac = match Amount::from(&param.hacash) {
            Err(e) => return errf!("hacash amount {} invalid: {}", param.hacash, &e),
            Ok(h) => h,
        };
        let act: Box<dyn Action> = maybe!(other_from, {
            let mut obj = HacFromToTrs::new();
            obj.from = AddrOrPtr::from_addr(fromaddr);
            obj.to = AddrOrPtr::from_addr(toaddr);
            obj.hacash = hac;
            Box::new(obj)
        }, {
            let mut obj = HacToTrs::new();
            obj.to = AddrOrPtr::from_addr(toaddr);
            obj.hacash = hac;
            Box::new(obj)
        });
        if let Err(e) = trsobj.push_action(act) {
            return errf!("push hac transfer action failed: {}", e);
        }
    }
    // sat
    if param.satoshi > 0 {
        let sat = Satoshi::from(param.satoshi);
        let act: Box<dyn Action> = maybe!(other_from, {
            let mut obj = SatFromToTrs::new();
            obj.from = AddrOrPtr::from_addr(fromaddr);
            obj.to = AddrOrPtr::from_addr(toaddr);
            obj.satoshi = sat;
            Box::new(obj)
        }, {
            let mut obj = SatToTrs::new();
            obj.to = AddrOrPtr::from_addr(toaddr);
            obj.satoshi = sat;
            Box::new(obj)
        });
        if let Err(e) = trsobj.push_action(act) {
            return errf!("push sat transfer action failed: {}", e);
        }
    }
    // hacd
    if param.diamonds.len() >= DiamondName::SIZE {
        let dialist = match DiamondNameListMax200::from_readable(&param.diamonds) {
            Err(e) => return errf!("diamonds invalid: {}", &e),
            Ok(d) => d,
        };
        let act: Box<dyn Action> = maybe!(other_from, {
                let mut obj = DiaFromToTrs::new();
                obj.from = AddrOrPtr::from_addr(fromaddr);
                obj.to = AddrOrPtr::from_addr(toaddr);
                obj.diamonds = dialist;
                Box::new(obj)
            }, maybe!(dialist.length() == 1, {
                    let mut obj = DiaSingleTrs::new();
                    obj.to = AddrOrPtr::from_addr(toaddr);
                    obj.diamond = DiamondName::from(*dialist.as_list()[0]);
                    Box::new(obj)
                }, {
                    let mut obj = DiaToTrs::new();
                    obj.to = AddrOrPtr::from_addr(toaddr);
                    obj.diamonds = dialist;
                    Box::new(obj)
                }
            )
        );
        if let Err(e) = trsobj.push_action(act) {
            return errf!("push diamond transfer action failed: {}", e);
        }
    }
    // do sign
    if let Err(e) = trsobj.fill_sign(&main) {
        return errf!("fill main sign failed: {}", e)
    }
    if other_from {
        if let Err(e) = trsobj.fill_sign(&from) {
            return errf!("fill from sign failed: {}", e)
        }
    }
    // finish
    Ok(CoinTransferResult{
        hash: trsobj.hash().to_hex(),
        hash_with_fee: trsobj.hash_with_fee().to_hex(),
        body: trsobj.serialize().to_hex(),
        timestamp: ts,
    })
}

#[derive(Default)]
#[wasm_bindgen(getter_with_clone, inspectable)]
pub struct CoinTransferV4Param {
    pub main_keystore: String,
    pub keystore_pass: String,
    pub fee: String,
    pub to_address: String,
    pub timestamp: u64,
    pub hacash: String,
    pub gas_max: u8,
}

#[wasm_bindgen]
impl CoinTransferV4Param {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }
}

#[wasm_bindgen]
pub fn create_coin_transfer_v4(param: CoinTransferV4Param) -> Ret<CoinTransferResult> {
    use basis::interface::{Transaction, TransactionRead};
    use protocol::action::HacToTrs;
    use protocol::transaction::TransactionType4;

    let main = q_hybrid_acc!(param.main_keystore, param.keystore_pass);
    let mainaddr = Address::from(*main.address());
    if !mainaddr.is_pqckey() && !mainaddr.is_hybrid() {
        return errf!("type 4 transfer main address must be pqckey or hybrid");
    }
    let fee = q_amt!(param.fee);
    let toaddr = q_adr!(param.to_address);
    let ts = if param.timestamp == 0 {
        curtimes()
    } else {
        param.timestamp
    };

    let mut tx = TransactionType4::new_by(mainaddr, fee, ts);
    tx.gas_max = Uint1::from(param.gas_max);

    if param.hacash.is_empty() {
        return errf!("hacash amount required for type 4 transfer");
    }
    let hac = Amount::from(&param.hacash).map_err(|e| format!("hacash invalid: {e}"))?;
    tx.push_action(Box::new(HacToTrs::create_by(toaddr, hac)))?;
    tx.fill_hybrid_sign(&main)?;

    Ok(CoinTransferResult {
        hash: tx.hash().to_hex(),
        hash_with_fee: tx.hash_with_fee().to_hex(),
        body: tx.serialize().to_hex(),
        timestamp: ts,
    })
}
