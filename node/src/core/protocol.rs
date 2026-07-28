use super::*;
use ::protocol;

pub(super) struct ProtocolAdapter {
    handler: Arc<MsgHandler>,
}

impl ProtocolAdapter {
    pub(super) fn new(handler: Arc<MsgHandler>) -> Self {
        Self { handler }
    }

    pub(super) fn start_loop(&self, tasks: &Arc<TaskGroup>, worker: Worker) {
        let hdl = self.handler.clone();
        let nwkr = worker.fork();
        tasks.spawn_thread("node-msg-handler", move || {
            let rt = new_current_thread_tokio_rt();
            rt.block_on(async move { MsgHandler::start(hdl, nwkr).await });
        });
    }

    pub(super) fn submit_transaction(
        &self,
        txpkg: &TxPkg,
        engine: Arc<dyn Engine>,
        txpool: Arc<dyn TxPool>,
        in_async: bool,
        only_insert_txpool: bool,
    ) -> Rerr {
        let txread = txpkg.tx_read();
        engine.try_execute_tx(txread)?;
        if only_insert_txpool {
            let minter = engine.minter();
            minter.tx_submit(engine.as_read(), txpkg)?;
            txpool.insert_by(txpkg.clone(), &|tx| minter.tx_pool_group(tx))?;
            return Ok(());
        }
        let handler = self.handler.clone();
        let txbody = txpkg.data().to_vec();
        if in_async {
            let runobj = async move {
                handler.submit_transaction(txbody).await;
            };
            tokio::spawn(runobj);
            Ok(())
        } else {
            if handler.is_loop_started() {
                handler.submit_transaction_wait(txbody)
            } else {
                new_current_thread_tokio_rt().block_on(handle_new_tx(handler, None, txbody))
            }
        }
    }

    pub(super) fn submit_block(&self, blkpkg: &BlkPkg, in_async: bool) -> Rerr {
        let handler = self.handler.clone();
        let blkbody = blkpkg.data().to_vec();
        if in_async {
            let runobj = async move {
                handler.submit_block(blkbody).await;
            };
            tokio::spawn(runobj);
            Ok(())
        } else {
            if handler.is_loop_started() {
                handler.submit_block_wait(blkbody)
            } else {
                new_current_thread_tokio_rt().block_on(handle_new_block(handler, None, blkbody))
            }
        }
    }

    pub(super) fn exit(&self) {
        self.handler.exit();
    }
}

pub(crate) async fn get_status_try_sync_blocks(
    hdl: &MsgHandler,
    peer: Arc<Peer>,
    starthei: u64,
    remote_height: u64,
) {
    let prevdo = hdl.doing_sync.load(Ordering::Relaxed);
    if prevdo + 2 > curtimes() {
        if !hdl
            .sync_tracker
            .begin_or_refresh(&peer, starthei, remote_height)
        {
            return;
        }
    } else if !hdl
        .sync_tracker
        .begin_or_refresh(&peer, starthei, remote_height)
    {
        return;
    }
    send_req_block_msg(hdl, peer, starthei).await;
}

pub(crate) async fn send_req_block_msg(hdl: &MsgHandler, peer: Arc<Peer>, starthei: u64) {
    hdl.doing_sync.store(curtimes(), Ordering::Relaxed);
    let hei = Uint8::from(starthei);
    let _ = peer.send_msg(MSG_REQ_BLOCK, hei.serialize()).await;
    flush!("sync blocks from {} {}...", peer.name(), starthei);
}

pub(crate) async fn send_req_block_hash_msg(peer: Arc<Peer>, num: u8, starthei: u64) {
    let hei = Uint8::from(starthei);
    let buf = vec![vec![num], hei.serialize()].concat();
    let _ = peer.send_msg(MSG_REQ_BLOCK_HASH, buf).await;
}

fn create_status(hdl: &MsgHandler) -> HandshakeStatus {
    let latest = hdl.engine.latest_block();
    let mintck = hdl.engine.minter();
    HandshakeStatus {
        genesis_hash: mintck.genesis_block().hash(),
        block_version: Uint1::from(1),
        transaction_type: Uint1::from(2),
        action_kind: Uint2::from(12),
        repair_serial: Uint2::from(1),
        __mark: Uint3::from(0),
        latest_height: *latest.height(),
        latest_hash: latest.hash(),
    }
}

pub(crate) async fn send_status(hdl: &MsgHandler, peer: Arc<Peer>) {
    let my_status = create_status(hdl);
    let msgbuf = my_status.serialize();
    let _ = peer.send_msg(MSG_STATUS, msgbuf).await;
}

pub(crate) async fn receive_status(hdl: &MsgHandler, peer: Arc<Peer>, buf: Vec<u8>) {
    let status = HandshakeStatus::create(&buf);
    if status.is_err() {
        peer.disconnect();
        return;
    }
    let (status, _) = status.unwrap();
    let my_status = create_status(hdl);
    if status.genesis_hash != my_status.genesis_hash {
        peer.disconnect();
        return;
    }
    let tar_hei = *status.latest_height;
    let my_hei = *my_status.latest_height;
    if my_hei == 0 && tar_hei > 0 {
        let start_hei = 1;
        get_status_try_sync_blocks(hdl, peer, start_hei, tar_hei).await;
        return;
    }
    if my_hei < tar_hei {
        let mut ubh = hdl.engine.config().unstable_block;
        if ubh > 255 {
            ubh = 255
        }
        // A backlog deeper than `unstable_block` cannot be closed by asking for
        // `unstable_block` hashes.
        //
        // That request exists to find the common ancestor of a SHALLOW fork, and
        // it was the only thing on offer once a node was past height zero. Used
        // on a real backlog it advances a handful of blocks per announcement,
        // which on mainnet means four blocks every five minutes: a node three
        // thousand blocks short of the tip would need days, while looking
        // completely idle. Observed exactly that, twice, on a node that had just
        // finished its history sync a little under the tip and then sat there.
        //
        // Batch sync is the same path a node takes from height zero and closes
        // that gap in minutes, so use it whenever the gap is too deep for the
        // hash walk to be the right tool.
        if tar_hei.saturating_sub(my_hei) > ubh {
            get_status_try_sync_blocks(hdl, peer, my_hei + 1, tar_hei).await;
            return;
        }
        let diff_hei = my_hei;
        let _ = hdl
            .sync_tracker
            .begin_or_refresh(&peer, diff_hei + 1, tar_hei);
        send_req_block_hash_msg(peer, ubh as u8, diff_hei).await;
    }
}

pub(crate) async fn send_hashs(hdl: &MsgHandler, peer: Arc<Peer>, buf: Vec<u8>) {
    if buf.len() != 1 + 8 {
        return;
    }
    let hnum = buf[0] as u64;
    if hnum > 80 {
        return;
    }
    let endhei = u64::from_be_bytes(bufcut!(buf, 1, 9));
    let latest = hdl.engine.latest_block();
    let lathei = latest.height().uint();
    if endhei > lathei {
        return;
    }
    let mut starthei = endhei - hnum;
    if hnum >= endhei {
        starthei = 1;
    }
    let store = hdl.engine.store();
    let mut reshxs = Vec::with_capacity((hnum + 8) as usize);
    reshxs.push(buf[1..9].to_vec());
    for hei in (starthei..=endhei).rev() {
        let curhx = store.block_hash(&BlockHeight::from(hei));
        if curhx.is_none() {
            return;
        }
        reshxs.push(curhx.unwrap().to_vec());
    }
    let _ = peer.send_msg(MSG_BLOCK_HASH, reshxs.concat()).await;
}

pub(crate) async fn receive_hashs(hdl: &MsgHandler, peer: Arc<Peer>, mut buf: Vec<u8>) {
    if buf.len() < 8 {
        return;
    }
    let hashs = buf.split_off(8);
    let end_hei = u64::from_be_bytes(bufcut!(buf, 0, 8));
    let hash_len = hashs.len();
    if hash_len == 0 || hash_len % 32 != 0 {
        return;
    }
    let mut hash_num = hash_len as u64 / 32;
    let latest = hdl.engine.latest_block();
    let lathei = latest.height().uint();
    if end_hei > lathei {
        return;
    }
    let dfhmax = hdl.engine.config().unstable_block as u64 + 1;
    if hash_num > dfhmax {
        hash_num = dfhmax;
    }
    let mut start_hei = end_hei - hash_num;
    if end_hei <= hash_num {
        start_hei = 0;
    }
    let store = hdl.engine.store();
    let mut hi = 0;
    for hei in ((start_hei + 1)..=end_hei).rev() {
        let myhx = store.block_hash(&BlockHeight::from(hei));
        if myhx.is_none() {
            return;
        }
        let myhx = myhx.unwrap();
        let hx = Fixed32::from(bufcut!(hashs, hi, hi + 32));
        if hx == myhx {
            get_status_try_sync_blocks(hdl, peer, hei + 1, end_hei).await;
            return;
        }
        hi += 32;
    }
}

pub(crate) async fn send_blocks(hdl: &MsgHandler, peer: Arc<Peer>, buf: Vec<u8>) {
    if buf.len() != 8 {
        return;
    }
    let starthei = u64::from_be_bytes(bufcut!(buf, 0, 8));
    let latest = hdl.engine.latest_block();
    let lathei = latest.height().uint();
    let maxsendsize = 1024 * 1024 * 20usize;
    let maxsendnum = 10000usize;
    let mut totalsize = 0;
    let mut totalnum = 0;
    let mut endhei = 0;
    let store = hdl.engine.store();
    let mut blkdtsary = vec![];
    for hei in starthei..=lathei {
        let Some((_, blkdts)) = store.block_data_by_height(&BlockHeight::from(hei)) else {
            return;
        };
        totalsize += blkdts.len();
        totalnum += 1;
        endhei = hei;
        blkdtsary.push(blkdts);
        if totalnum >= maxsendnum || totalsize >= maxsendsize {
            break;
        }
    }
    let resblkdts = blkdtsary.concat();
    let msgbody = vec![
        lathei.to_be_bytes().to_vec(),
        starthei.to_be_bytes().to_vec(),
        endhei.to_be_bytes().to_vec(),
        resblkdts,
    ]
    .concat();
    let _ = peer.send_msg(MSG_BLOCK, msgbody).await;
}

pub(crate) async fn receive_blocks(hdl: &MsgHandler, peer: Arc<Peer>, mut buf: Vec<u8>) {
    if buf.len() < 3 * 8 {
        println!("data check failed");
        return;
    }
    let blocks = buf.split_off(3 * 8);
    let latest_hei = u64::from_be_bytes(bufcut!(buf, 0, 8));
    let _start_hei = u64::from_be_bytes(bufcut!(buf, 8, 16));
    let end_hei = u64::from_be_bytes(bufcut!(buf, 16, 24));
    let persent = end_hei as f64 / latest_hei as f64 * 100.0;
    let eng = hdl.engine.clone();
    let inserting = hdl.inserting.clone();
    let res = tokio::task::spawn_blocking(move || {
        let _lk = inserting.lock().unwrap();
        flush!("{}({:.2}%) inserting...", end_hei, persent);
        eng.synchronize(blocks)
    })
    .await
    .unwrap();
    if let Err(e) = res {
        // A failed batch used to end the sync outright: print, return, and never
        // ask for anything again. The node then sat at that height forever,
        // answering its RPC and looking healthy, while the chain moved on. That
        // is how a single
        //   [Block Sync Warning] insert N failed: diamond status HTAKES not found
        // turned into a node that was dead for hours without saying so.
        //
        // One failure is not a reason to stop asking. Resume from where the
        // chain ACTUALLY is rather than from this batch's end, because a partial
        // batch may have inserted some of its blocks, and a wrong start height
        // is refused by do_synchronize and would fail again immediately.
        //
        // The pause matters: without it a permanently bad block becomes a tight
        // loop that floods the peer and the log. With it, a transient failure
        // recovers on its own and a permanent one keeps saying so out loud,
        // which is the outcome to prefer over silence.
        println!("{}", e);
        let head = hdl.engine.latest_block().height().uint();
        println!(
            "[P2P] sync failed at height {}; retrying from {} in 10s",
            end_hei,
            head + 1
        );
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        send_req_block_msg(hdl, peer, head + 1).await;
        return;
    }
    println!("ok.");
    if end_hei >= latest_hei {
        hdl.sync_tracker
            .finish_if_done(&peer, end_hei + 1, latest_hei);
        println!("all blocks sync finished.");
        return;
    }
    hdl.sync_tracker
        .finish_if_done(&peer, end_hei + 1, latest_hei);
    let peer = hdl.switch_peer(peer);
    send_req_block_msg(hdl, peer, end_hei + 1).await;
}

pub(crate) async fn handle_new_tx(
    hdl: Arc<MsgHandler>,
    peer: Option<Arc<Peer>>,
    body: Vec<u8>,
) -> Rerr {
    let engcnf = hdl.engine.config();
    let minter = hdl.engine.minter();
    let Ok(txpkg) = protocol::transaction::build_tx_package(body) else {
        return errf!("tx parse failed");
    };
    let hxfe = txpkg.tx().hash_with_fee();
    // println!("handle_new_tx: {:?}", hxfe.to_hex());
    let (already, knowkey) = check_know(&hdl.knows, &hxfe, peer.clone());
    if already {
        if peer.is_some() || hdl.txpool.find(&txpkg.hash()).is_some() {
            return Ok(());
        }
    }
    if txpkg.fpur() < engcnf.lowest_fee_purity {
        return errf!(
            "tx fee purity {} too low, need at least {}",
            txpkg.fpur(),
            engcnf.lowest_fee_purity
        );
    }
    let txpr = txpkg.tx_read();
    let max_wire = protocol::transaction::effective_max_tx_wire_size(engcnf.max_tx_size, txpr.ty());
    if txpkg.data().len() > max_wire {
        if txpr.ty() == protocol::transaction::TransactionType4::TYPE {
            protocol::metrics::emit(protocol::metrics::PqcMetricEvent::Type4MempoolRejected);
            println!(
                "[p2p] type4 tx rejected: wire size {} > cap {}",
                txpkg.data().len(),
                max_wire
            );
        }
        return errf!("tx size exceeds max_tx_size");
    }
    let txdatas = txpkg.data().to_vec();
    hdl.engine.try_execute_tx(txpr)?;
    minter.tx_submit(hdl.engine.as_read(), &txpkg)?;
    hdl.txpool
        .insert_by(txpkg, &|tx| minter.tx_pool_group(tx))?;
    let p2p = hdl.p2pmng.lock().unwrap();
    if let Some(p2p) = p2p.as_ref() {
        p2p.broadcast_message(0, knowkey, MSG_TX_SUBMIT, txdatas);
    }
    Ok(())
}

pub(crate) async fn handle_new_block(
    hdl: Arc<MsgHandler>,
    peer: Option<Arc<Peer>>,
    body: Vec<u8>,
) -> Rerr {
    let eng = hdl.engine.clone();
    let engcnf = eng.config();
    if body.len() > engcnf.max_block_size + 100 {
        return errf!("block size exceeds max_block_size");
    }
    let mut blkhead = protocol::block::BlockIntro::default();
    if let Err(..) = blkhead.parse(&body) {
        return errf!("block intro parse failed");
    }
    let blkhei = blkhead.height().uint();
    let blkhx = blkhead.hash();
    let sto = eng.store();
    let (already, knowkey) = check_know(&hdl.knows, &blkhx, peer.clone());
    // println!("knows: {:?}", hdl.knows);
    // println!("handle_new_block {} already: {}", blkhx.to_hex(), already);
    if already {
        if peer.is_none() {
            let inserted = sto
                .block_hash(&BlockHeight::from(blkhei))
                .map(|hx| hx == blkhx)
                .unwrap_or(false);
            if inserted {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    }
    let mintckr = eng.minter();
    if let Some(ret) = mintckr.blk_found(&blkhead, &body, sto.as_ref()) {
        match ret {
            RetBlkFound::Reject => return errf!("block rejected by blk_found"),
            RetBlkFound::PendingCached => {
                let p2p = hdl.p2pmng.lock().unwrap();
                if let Some(p2p) = p2p.as_ref() {
                    p2p.broadcast_message(0, knowkey, MSG_BLOCK_DISCOVER, body);
                }
                return Ok(());
            }
            RetBlkFound::Normal => {}
        }
    }
    let status = sto.status();
    let root_hei = status.root_height.uint();
    let heispan = engcnf.unstable_block;
    let latest = eng.latest_block();
    let lathei = latest.height().uint();
    if blkhei <= root_hei {
        return errf!(
            "block height {} is not above root height {}",
            blkhei,
            root_hei
        );
    }
    if blkhei > lathei + 1 {
        if let Some(ref pr) = peer {
            if lathei + heispan + 1 < blkhei {
                // Too far behind for the hash walk to be the right tool: it
                // would crawl `unstable_block` blocks per announcement. Ask this
                // peer, which demonstrably has the newer chain since it just
                // sent one, for a real batch instead.
                get_status_try_sync_blocks(&hdl, pr.clone(), lathei + 1, blkhei).await;
                println!(
                    "[P2P] {} blocks behind at height {}, catching up from {}",
                    blkhei - lathei,
                    lathei,
                    pr.name(),
                );
            } else {
                send_req_block_hash_msg(pr.clone(), (heispan + 1) as u8, lathei).await;
            }
        }
        // The old message here said "ignore future block ... during history
        // sync" and was printed once per arriving block while nothing advanced.
        // It read as a node refusing work when in truth it was asking for four
        // hashes at a time and getting nowhere. The line above now reports the
        // gap and the catch-up, which is the fact an operator needs.
        return errf!(
            "future block height {} exceeds local head {}",
            blkhei,
            lathei
        );
    }
    if let Err(..) = mintckr.blk_arrive(&blkhead, &body, sto.as_ref()) {
        return errf!("block arrive check failed");
    }
    let blkpkg = protocol::block::build_block_package(body.clone());
    let mut blkp = blkpkg.map_err(|e| format!("block parse failed: {}", e))?;
    blkp.set_origin(BlkOrigin::Discover);
    let hxstrt = blkhx.as_bytes()[4..12].to_vec();
    let hxtail = blkhx.as_bytes()[30..].to_vec();
    let txs = blkp.block().transaction_count().uint() - 1;
    let _blkts = &timeshow(blkp.block().timestamp().uint())[14..];
    let mshow = may_show_miner_detail(&engcnf, &blkp);
    let thsx = blkp.block().transaction_hash_list(false);
    let engptr = eng.clone();
    let txpool = hdl.txpool.clone();
    let inserting = hdl.inserting.clone();
    let res = tokio::task::spawn_blocking(move || {
        let _lk = inserting.lock().unwrap();
        print!(
            "block {} ...{}...{} txs{:2} insert at {} {}",
            blkhei,
            hex::encode(&hxstrt),
            hex::encode(&hxtail),
            txs,
            &ctshow()[11..],
            mshow
        );
        let r = engptr.discover(blkp);
        if let Err(e) = &r {
            println!("Error: {}", e);
        } else {
            println!("ok.");
            let mintckr2 = engptr.minter();
            mintckr2.tx_pool_refresh(engptr.as_ref().as_read(), txpool.as_ref(), thsx, blkhei);
        }
        r
    })
    .await
    .unwrap();
    if res.is_err() {
        return res;
    }
    let p2p = hdl.p2pmng.lock().unwrap();
    if let Some(p2p) = p2p.as_ref() {
        p2p.broadcast_message(0, knowkey, MSG_BLOCK_DISCOVER, body);
    }
    Ok(())
}

fn check_know(mine: &Knowledge, hxkey: &Hash, peer: Option<Arc<Peer>>) -> (bool, KnowKey) {
    let knowkey: [u8; KNOWLEDGE_SIZE] = hxkey.clone().into_array();
    if let Some(ref pr) = peer {
        pr.knows.add(knowkey.clone());
    }
    if mine.check(&knowkey) {
        return (true, knowkey);
    }
    mine.add(knowkey.clone());
    (false, knowkey)
}

fn may_show_miner_detail(engcnf: &EngineConf, blkp: &BlkPkg) -> String {
    if !engcnf.show_miner_name {
        return s!("");
    }
    let Ok(ptx) = blkp.block().prelude_transaction() else {
        return s!("");
    };
    let Some(author) = ptx.author() else {
        return s!("");
    };
    let adrt = author.to_readable().drain(..9).collect::<String>();
    let message = ptx
        .block_message()
        .map(|msg| msg.to_readable_left())
        .unwrap_or_else(|| s!(""));
    format!("miner: {}...<{}> ", adrt, message)
}
