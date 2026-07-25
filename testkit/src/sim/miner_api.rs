use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use basis::interface::{Transaction, TransactionRead};
use field::*;
use mint::TransactionCoinbase;
use protocol::block::*;
use serde_json::{Value as JV, json};
use sys::ToHex;
use tokio::runtime::Builder;
use tokio::sync::oneshot;

fn hash_more_power(dst: &[u8], src: &[u8]) -> bool {
    let mut ln = dst.len();
    let l2 = src.len();
    if l2 < ln {
        ln = l2;
    }
    for i in 0..ln {
        let (l, r) = (dst[i], src[i]);
        if l < r {
            return true;
        } else if l > r {
            return false;
        }
    }
    false
}

#[derive(Clone)]
pub struct MinerPendingStuff {
    pub height: u64,
    pub target_hash: Hash,
    pub block_intro: BlockIntro,
    pub coinbase_tx: TransactionCoinbase,
    pub mkrl_modify_list: Vec<Hash>,
}

impl MinerPendingStuff {
    pub fn easy_for_test(height: u64) -> Self {
        let mut intro = BlockIntro::default();
        // A real node always packs the height into the header it hands out, and the
        // miner now rejects a template whose JSON height disagrees with its intro
        // (the JSON height picks the x16rs repeat count while consensus reads the
        // header, so a mismatch mines at the wrong repeat). Keep the fixture
        // self-consistent instead of leaving the defaulted height 0.
        intro.head.height = BlockHeight::from(height);

        let coinbase_tx = TransactionCoinbase::default();

        Self {
            height,
            target_hash: Hash::from([0xFF; 32]),
            block_intro: intro,
            coinbase_tx,
            mkrl_modify_list: vec![],
        }
    }
}

#[derive(Clone)]
struct MinerApiState {
    pending: Arc<Mutex<MinerPendingStuff>>,
    submit_count: Arc<AtomicUsize>,
    last_submit: Arc<Mutex<HashMap<String, String>>>,
}

pub struct MinerApiSim {
    rpcaddr: String,
    submit_count: Arc<AtomicUsize>,
    last_submit: Arc<Mutex<HashMap<String, String>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl MinerApiSim {
    pub fn start(pending: MinerPendingStuff) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind miner api sim listener");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let rpcaddr = listener.local_addr().expect("read local addr").to_string();

        let state = MinerApiState {
            pending: Arc::new(Mutex::new(pending)),
            submit_count: Arc::new(AtomicUsize::new(0)),
            last_submit: Arc::new(Mutex::new(HashMap::new())),
        };

        let submit_count = state.submit_count.clone();
        let last_submit = state.last_submit.clone();

        let app = Router::new()
            .route("/query/miner/pending", get(handle_pending))
            .route("/query/miner/notice", get(handle_notice))
            .route("/submit/miner/success", get(handle_success))
            .with_state(state.clone());

        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let handle = thread::spawn(move || {
            let runtime = Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime for miner api sim");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("convert tokio tcp listener");
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = stop_rx.await;
                    })
                    .await
                    .expect("run miner api sim server");
            });
        });

        Self {
            rpcaddr,
            submit_count,
            last_submit,
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        }
    }

    pub fn rpcaddr(&self) -> &str {
        &self.rpcaddr
    }

    pub fn submit_count(&self) -> usize {
        self.submit_count.load(Ordering::SeqCst)
    }

    pub fn last_submit(&self) -> HashMap<String, String> {
        self.last_submit.lock().unwrap().clone()
    }

    pub fn wait_for_submit(&self, at_least: usize, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.submit_count() >= at_least {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub fn stop(mut self) {
        if let Some(stop) = self.stop_tx.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MinerApiSim {
    fn drop(&mut self) {
        if let Some(stop) = self.stop_tx.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

async fn handle_pending(
    State(state): State<MinerApiState>,
    _query: Query<HashMap<String, String>>,
) -> Json<JV> {
    let pending = state.pending.lock().unwrap().clone();
    let data = json!({
        "ret": 0,
        "height": pending.height,
        "target_hash": pending.target_hash.to_vec().to_hex(),
        "block_intro": pending.block_intro.serialize().to_hex(),
        "coinbase_body": pending.coinbase_tx.serialize().to_hex(),
        "mkrl_modify_list": pending.mkrl_modify_list.iter().map(|h| h.to_vec().to_hex()).collect::<Vec<String>>(),
    });
    Json(data)
}

async fn handle_notice(
    State(state): State<MinerApiState>,
    _query: Query<HashMap<String, String>>,
) -> Json<JV> {
    let height = state.pending.lock().unwrap().height;
    Json(json!({ "ret": 0, "height": height }))
}

async fn handle_success(
    State(state): State<MinerApiState>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<JV> {
    let pending = state.pending.lock().unwrap().clone();

    let Some(height_text) = query.get("height") else {
        return Json(json!({"ret": 1, "err": "height missing"}));
    };
    let Ok(height) = height_text.parse::<u64>() else {
        return Json(json!({"ret": 1, "err": "height format invalid"}));
    };
    if height != pending.height {
        return Json(json!({"ret": 1, "err": "height mismatch"}));
    }

    let Some(block_nonce_text) = query.get("block_nonce") else {
        return Json(json!({"ret": 1, "err": "block_nonce missing"}));
    };
    let Ok(block_nonce) = block_nonce_text.parse::<u32>() else {
        return Json(json!({"ret": 1, "err": "block_nonce format invalid"}));
    };

    let Some(coinbase_nonce_text) = query.get("coinbase_nonce") else {
        return Json(json!({"ret": 1, "err": "coinbase_nonce missing"}));
    };
    let Ok(coinbase_nonce_bytes) = hex::decode(coinbase_nonce_text) else {
        return Json(json!({"ret": 1, "err": "coinbase_nonce decode error"}));
    };
    if coinbase_nonce_bytes.len() != Hash::SIZE {
        return Json(json!({"ret": 1, "err": "coinbase_nonce length invalid"}));
    }

    let mut coinbase_tx = pending.coinbase_tx.clone();
    coinbase_tx.set_nonce(Hash::from(coinbase_nonce_bytes.clone().try_into().unwrap()));

    let prelude_hash = coinbase_tx.hash();
    let mkrl = calculate_mrkl_prelude_update(prelude_hash, &pending.mkrl_modify_list);
    let mut intro = pending.block_intro.clone();
    intro.set_mrklroot(mkrl);
    intro.meta.nonce = Uint4::from(block_nonce);
    let blkhx = x16rs::block_hash(pending.height, &intro.serialize());
    let target = pending.target_hash.to_vec();
    let pass = hash_more_power(&blkhx, &target) || blkhx.as_slice() == target.as_slice();
    if !pass {
        return Json(json!({"ret": 1, "err": "difficulty check failed"}));
    }

    state.submit_count.fetch_add(1, Ordering::SeqCst);
    *state.last_submit.lock().unwrap() = query.clone();
    Json(json!({"ret": 0, "ok": true}))
}
