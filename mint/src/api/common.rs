struct MintApiService {}

pub fn service() -> Arc<dyn ApiService> {
    Arc::new(MintApiService {})
}

impl ApiService for MintApiService {
    fn name(&self) -> &'static str {
        "mint"
    }

    fn routes(&self) -> Vec<ApiRoute> {
        routes()
    }
}

#[allow(dead_code)]
pub struct MinerBlockStuff {
    height: BlockHeight,
    block_nonce: Uint4,
    coinbase_nonce: Hash,
    target_hash: Hash,
    coinbase_tx: crate::TransactionCoinbase,
    block: BlockV1,
    mrklrts: Vec<Hash>,
}

static MINER_PENDING_BLOCK: LazyLock<Arc<Mutex<VecDeque<MinerBlockStuff>>>> =
    LazyLock::new(|| Arc::default());

static MINER_PACKING_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn api_error(errmsg: &str) -> ApiResponse {
    ApiResponse::json(json!({"ret":1,"err":errmsg}).to_string())
}

fn api_ok(data: Vec<(&str, Value)>) -> ApiResponse {
    let mut out = serde_json::Map::new();
    out.insert("ret".to_owned(), json!(0));
    for (k, v) in data {
        out.insert(k.to_owned(), v);
    }
    ApiResponse::json(Value::Object(out).to_string())
}

fn q_bool(req: &ApiRequest, key: &str, dv: bool) -> bool {
    let Some(v) = req.query(key) else {
        return dv;
    };
    match v {
        "false" | "False" | "FALSE" | "none" | "None" | "NONE" | "null" | "Null" | "NULL" | "0"
        | "_" | "" => false,
        _ => true,
    }
}

fn q_string(req: &ApiRequest, key: &str, dv: &str) -> String {
    req.query(key)
        .map_or_else(|| dv.to_owned(), |s| s.to_owned())
}

/// Type 4 keystore: query `hybrid_keystore` or raw JSON POST body (browser/wallet friendly).
fn hybrid_keystore_from_req(req: &ApiRequest) -> String {
    let from_query = q_string(req, "hybrid_keystore", "");
    if !from_query.is_empty() {
        return from_query;
    }
    if req.body.is_empty() {
        return String::new();
    }
    String::from_utf8_lossy(&req.body).trim().to_string()
}

fn q_u32(req: &ApiRequest, key: &str, dv: u32) -> u32 {
    req.query(key)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(dv)
}

fn q_i64(req: &ApiRequest, key: &str, dv: i64) -> i64 {
    req.query(key)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(dv)
}

fn q_f64(req: &ApiRequest, key: &str, dv: f64) -> f64 {
    req.query(key)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(dv)
}

fn q_coinkind_hsd(req: &ApiRequest) -> Ret<(bool, bool, bool)> {
    let raw = q_string(req, "coinkind", "hsd");
    let mut s = raw.to_lowercase();
    s.retain(|c| !c.is_whitespace() && c != ',' && c != ';' && c != '|');
    if s.is_empty() || s == "all" || s == "hsda" {
        return Ok((true, true, true));
    }
    if !s
        .chars()
        .all(|c| c == 'h' || c == 's' || c == 'd' || c == 'a')
    {
        return errf!("coinkind format invalid");
    }
    Ok((s.contains('h'), s.contains('s'), s.contains('d')))
}

fn api_html(s: String) -> ApiResponse {
    ApiResponse {
        status: 200,
        headers: vec![(
            "content-type".to_owned(),
            "text/html; charset=utf-8".to_owned(),
        )],
        body: s.into_bytes(),
    }
}

fn api_data(data: serde_json::Map<String, Value>) -> ApiResponse {
    let mut out = serde_json::Map::new();
    out.insert("ret".to_owned(), json!(0));
    for (k, v) in data {
        out.insert(k, v);
    }
    ApiResponse::json(Value::Object(out).to_string())
}

fn api_data_list(list: Vec<Value>) -> ApiResponse {
    ApiResponse::json(json!({"ret":0,"list":list}).to_string())
}

fn body_data_may_hex(req: &ApiRequest) -> Ret<Vec<u8>> {
    if !q_bool(req, "hexbody", false) {
        return Ok(req.body.clone());
    }
    hex::decode(&req.body).map_err(|_| "hex format invalid".to_owned())
}

fn right_00_to_ff(hx: &mut [u8]) {
    if hx.is_empty() || *hx.last().unwrap() != 0 {
        return;
    }
    for i in (0..hx.len()).rev() {
        if hx[i] > 0 {
            hx[i] -= 1;
            break;
        }
        hx[i] = 255;
    }
}

fn encode_bytes(v: Vec<u8>, is_base64: bool) -> String {
    maybe!(is_base64, v.to_base64(), v.to_hex())
}

fn read_mint_state(ctx: &ApiExecCtx) -> Arc<Box<dyn State>> {
    ctx.engine.state()
}


fn update_miner_pending_block(block: BlockV1, cbtx: crate::TransactionCoinbase) {
    let mkrluphxs = calculate_mrkl_prelude_modify(&block.transaction_hash_list(true));
    let mut stfs = MINER_PENDING_BLOCK.lock().unwrap();
    stfs.push_front(MinerBlockStuff {
        height: block.height().clone(),
        block_nonce: Uint4::default(),
        coinbase_nonce: Hash::default(),
        target_hash: Hash::from(u32_to_hash(block.difficulty().uint())),
        coinbase_tx: cbtx,
        block,
        mrklrts: mkrluphxs,
    });
    if stfs.len() > 3 {
        stfs.pop_back();
    }
}

fn miner_reset_next_new_block(engine: Arc<dyn Engine>, txpool: &dyn TxPool) {
    let block = engine.minter().packing_next_block(engine.as_read(), txpool);
    let block = *block.downcast::<BlockV1>().unwrap();
    let cbtx: Box<dyn Transaction> = block.transactions()[0].clone();
    let cbtx: crate::TransactionCoinbase = maybe!(
        cbtx.ty() == 0,
        crate::TransactionCoinbase::must(&cbtx.serialize()),
        never!()
    );
    update_miner_pending_block(block, cbtx);
}

fn get_miner_pending_block_stuff(
    is_detail: bool,
    is_transaction: bool,
    is_stuff: bool,
    is_base64: bool,
) -> ApiResponse {
    let mut stuff = MINER_PENDING_BLOCK.lock().unwrap();
    if stuff.is_empty() {
        return api_error("pending block not ready");
    }
    let stuff = &mut stuff[0];

    if let Err(e) = stuff.coinbase_nonce.increase() {
        return api_error(&e)
    }
    stuff.coinbase_tx.set_mining_nonce(stuff.coinbase_nonce);
    let cbhx = stuff.coinbase_tx.hash();
    let mkrl = calculate_mrkl_prelude_update(cbhx, &stuff.mrklrts);
    stuff.block.set_mrklroot(mkrl);
    stuff.block.replace_transaction(0, Box::new(stuff.coinbase_tx.clone())).unwrap();
    let intro_data = stuff.block.intro.serialize().to_hex();

    let mut tg_hash = stuff.target_hash.to_vec();
    right_00_to_ff(&mut tg_hash);

    let mut data = serde_json::Map::new();
    data.insert("height".to_owned(), json!(*stuff.height));
    data.insert(
        "coinbase_nonce".to_owned(),
        json!(encode_bytes(stuff.coinbase_nonce.to_vec(), is_base64)),
    );
    data.insert("block_intro".to_owned(), json!(intro_data));
    data.insert(
        "target_hash".to_owned(),
        json!(encode_bytes(tg_hash, is_base64)),
    );

    if is_detail {
        data.insert("version".to_owned(), json!(stuff.block.version().uint()));
        data.insert(
            "prevhash".to_owned(),
            json!(encode_bytes(stuff.block.prevhash().to_vec(), is_base64)),
        );
        data.insert(
            "timestamp".to_owned(),
            json!(stuff.block.timestamp().uint()),
        );
        data.insert(
            "transaction_count".to_owned(),
            json!(stuff.block.transaction_count().uint().saturating_sub(1)),
        );
        data.insert(
            "reward_address".to_owned(),
            json!(stuff.coinbase_tx.author().unwrap_or_default().to_readable()),
        );
    }

    if is_transaction {
        let tx_raws: Vec<String> = stuff
            .block
            .transactions()
            .iter()
            .map(|tx| encode_bytes(tx.serialize(), is_base64))
            .collect();
        data.insert("transaction_body_list".to_owned(), json!(tx_raws));
    }

    if is_stuff {
        data.insert(
            "coinbase_body".to_owned(),
            json!(encode_bytes(stuff.coinbase_tx.serialize(), is_base64)),
        );
        let mhxs: Vec<String> =
            calculate_mrkl_prelude_modify(&stuff.block.transaction_hash_list(true))
                .into_iter()
                .map(|hx| encode_bytes(hx.serialize(), is_base64))
                .collect();
        data.insert("mkrl_modify_list".to_owned(), json!(mhxs));
    }

    let mut out = serde_json::Map::new();
    out.insert("ret".to_owned(), json!(0));
    for (k, v) in data {
        out.insert(k, v);
    }
    ApiResponse::json(Value::Object(out).to_string())
}



fn hash_diff(dst: &Hash, tar: &Hash) -> i8 {
    for i in 0..Hash::SIZE {
        if dst[i] > tar[i] {
            return 1;
        } else if dst[i] < tar[i] {
            return -1;
        }
    }
    0
}





fn load_block_by_height(ctx: &ApiExecCtx, height: u64) -> Ret<Arc<BlkPkg>> {
    let store = ctx.engine.store();
    let Some((_, blkdts)) = store.block_data_by_height(&BlockHeight::from(height)) else {
        return errf!("block not found");
    };
    let Ok(blkpkg) = build_block_package(blkdts) else {
        return errf!("block parse failed");
    };
    Ok(Arc::new(blkpkg))
}

fn query_hashrate(ctx: &ApiExecCtx) -> serde_json::Map<String, Value> {
    let mtcnf = ctx.engine.minter().config().downcast::<MintConf>().unwrap();
    let btt = mtcnf.each_block_target_time as f64;
    let lastblk = ctx.engine.latest_block();
    let curhei = lastblk.height().uint();
    let tg_difn = lastblk.difficulty().uint();
    let mut tg_hash = u32_to_hash(tg_difn);
    let tg_rate = hash_to_rates(&tg_hash, btt);
    let tg_show = rates_to_show(tg_rate);

    let mut rt_rate = tg_rate;
    let mut rt_show = tg_show.clone();
    let ltc = 100u64;
    if curhei > ltc {
        if let Ok(pblk) = load_block_by_height(ctx, curhei - ltc) {
            let p100t = pblk.block().timestamp().uint();
            let cttt = (lastblk.timestamp().uint() - p100t) / ltc;
            if cttt > 0 {
                rt_rate = rt_rate * btt / cttt as f64;
                rt_show = rates_to_show(rt_rate);
            }
        }
    }

    right_00_to_ff(&mut tg_hash);
    let mut data = serde_json::Map::new();
    data.insert(
        "target".to_owned(),
        json!({
            "rate": tg_rate,
            "show": tg_show,
            "unit": "H/s",
            "hash": hex::encode(&tg_hash),
            "difn": tg_difn,
        }),
    );
    data.insert(
        "realtime".to_owned(),
        json!({
            "rate": rt_rate,
            "show": rt_show,
            "unit": "H/s",
        }),
    );
    data
}


fn get_blk_rate(ctx: &ApiExecCtx, hei: u64) -> Ret<u128> {
    let difn = load_block_by_height(ctx, hei)?.block().difficulty().uint();
    let mtcnf = ctx.engine.minter().config().downcast::<MintConf>().unwrap();
    let secs = mtcnf.each_block_target_time as f64;
    Ok(u32_to_rates(difn, secs) as u128)
}







fn get_id_range(max: i64, page: i64, limit: i64, instart: i64, desc: bool) -> Vec<i64> {
    let mut start = 1;
    if instart != i64::MAX {
        start = instart;
    }
    if desc && instart == i64::MAX {
        start = max;
    }
    if page > 1 {
        if desc {
            start -= (page - 1) * limit;
        } else {
            start += (page - 1) * limit;
        }
    }
    let mut end = start + limit;
    if desc {
        end = start - limit;
    }
    let mut rng: Vec<_> = (start..end).collect();
    if desc {
        rng = (end + 1..start + 1).rev().collect();
    }
    rng.retain(|&x| x >= 1 || x <= max);
    rng
}
