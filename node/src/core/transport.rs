use super::*;

pub(super) struct TransportAdapter {
    p2p: Arc<P2PManage>,
    cnf: NodeConf,
}

impl TransportAdapter {
    pub(super) fn new(cnf: &NodeConf, p2p: Arc<P2PManage>) -> Self {
        Self {
            p2p,
            cnf: cnf.clone(),
        }
    }

    pub(super) fn start(&self, worker: Worker) {
        let p2p = self.p2p.clone();
        let is_multi_thread = self.cnf.multi_thread;
        let mut imtip = ".";
        if is_multi_thread {
            imtip = " with multi thread."
        }
        println!("[P2P] Start and listening on {}{}", self.cnf.listen, imtip);
        let _ = new_tokio_rt(is_multi_thread)
            .block_on(async move { P2PManage::start(p2p, worker).await });
    }

    pub(super) fn peer_prints(&self) -> Vec<String> {
        self.p2p.all_peer_prints()
    }

    pub(super) fn peer_connectivity(&self) -> PeerConnectivity {
        self.p2p.peer_connectivity()
    }

    pub(super) fn exit(&self) {
        self.p2p.exit();
    }
}

pub(crate) async fn broadcast_unaware(p2p: &P2PManage, key: &KnowKey, ty: u16, body: Vec<u8>) {
    let mut resps = vec![];
    let peers = vec![p2p.backbones(), p2p.offshoots()].concat();
    for peer in peers {
        if !peer.knows.check(key) {
            peer.knows.add(key.clone());
            resps.push(peer);
        }
    }
    let msgbody = vec![ty.to_be_bytes().to_vec(), body].concat();
    let msgbuf = tcp_create_msg(MSG_CUSTOMER, msgbody);
    for peer in resps {
        let _ = peer.send(&msgbuf).await;
    }
}

pub(crate) async fn connect_stable_then_boot(p2p: &P2PManage) -> Rerr {
    if p2p.cnf.use_stable_nodes {
        let _ = connect_stable_nodes(p2p).await;
        if p2p.backbones().len() < p2p.cnf.backbone_peers {
            let _ = connect_boot_nodes(p2p).await;
        }
    } else {
        let _ = connect_boot_nodes(p2p).await;
    }
    Ok(())
}

pub(crate) async fn connect_stable_nodes(p2p: &P2PManage) -> Rerr {
    if !p2p.cnf.use_stable_nodes {
        return Ok(());
    }
    let addrs = p2p.load_stable_nodes();
    if addrs.is_empty() {
        return Ok(());
    }
    print!("[P2P] Connect {} stable nodes", addrs.len());
    for ndip in &addrs {
        print!(", {}", &ndip);
    }
    println!(".");
    for addr in addrs {
        if let Err(e) = connect_node(p2p, addr).await {
            println!("[P2P Error] Connect to {}, {}", &addr, e);
        }
    }
    Ok(())
}

pub(crate) async fn connect_boot_nodes(p2p: &P2PManage) -> Rerr {
    print!("[P2P] Connect {} boot nodes", p2p.cnf.boot_nodes.len());
    for ndip in &p2p.cnf.boot_nodes {
        print!(", {}", &ndip);
    }
    if !p2p.cnf.find_nodes {
        print!(", don't search nodes");
    }
    println!(".");
    for addr in &p2p.cnf.boot_nodes {
        if let Err(e) = connect_node(p2p, *addr).await {
            println!("[P2P Error] Connect to {}, {}", &addr, e);
        }
    }
    Ok(())
}

pub(crate) async fn connect_node(p2p: &P2PManage, addr: SocketAddr) -> Ret<Arc<Peer>> {
    let conn = tcp_dial_connect(addr, 6).await?;
    handle_conn(p2p, conn, true).await
}

pub(crate) async fn handle_conn(
    p2p: &P2PManage,
    mut conn: TcpStream,
    report_me: bool,
) -> Ret<Arc<Peer>> {
    tcp_check_handshake(&mut conn, 5).await?;
    let mynodeinfo = p2p.pick_my_node_info();
    let mndf = mynodeinfo.clone();
    if report_me {
        tcp_send_msg(&mut conn, MSG_REPORT_PEER, mndf).await?;
    }
    insert_peer(p2p, conn, mynodeinfo).await
}

pub(crate) async fn insert_peer(
    p2p: &P2PManage,
    conn: TcpStream,
    mynodeinfo: Vec<u8>,
) -> Ret<Arc<Peer>> {
    let (peer, conn_read) = try_create_peer(p2p, conn, mynodeinfo).await?;
    let dropeds = p2p.insert(peer.clone()).await?;
    p2p.delay_close_peers(dropeds, 15).await;
    handle_peer_message(p2p, peer.clone(), conn_read).await?;
    Ok(peer)
}

pub(crate) async fn try_create_peer(
    p2p: &P2PManage,
    mut stream: TcpStream,
    mynodeinfo: Vec<u8>,
) -> Ret<(Arc<Peer>, OwnedReadHalf)> {
    let conn = &mut stream;
    let (ty, body) = tcp_read_msg(conn, 5).await?;
    if MSG_REMIND_ME_IS_PUBLIC == ty {
        return errf!("ok");
    } else if MSG_REQUEST_NODE_KEY_FOR_PUBLIC_CHECK == ty {
        let buf = p2p.cnf.node_key.clone();
        let _ = AsyncWriteExt::write_all(conn, &buf).await;
        return errf!("ok");
    } else if MSG_REQUEST_NEAREST_PUBLIC_NODES == ty {
        let peerlist = p2p.publics();
        let (count, adrbts) = serialize_public_nodes(&peerlist, 100);
        let retbts = vec![vec![count as u8], adrbts].concat();
        let _ = AsyncWriteExt::write_all(conn, &retbts).await;
        return errf!("ok");
    }
    Peer::create_with_msg(stream, ty, body, mynodeinfo).await
}

pub(crate) async fn handle_peer_message(
    p2p: &P2PManage,
    peer: Arc<Peer>,
    conn_read: OwnedReadHalf,
) -> Rerr {
    let peer1 = peer.clone();
    let peer2 = peer.clone();
    let peer3 = peer.clone();
    let peersnaprx = p2p.peersnaprx.clone();
    let peertabletx = p2p.peertabletx.clone();
    let hdl1 = p2p.msghandler.clone();
    let hdl2 = p2p.msghandler.clone();
    let hdl3 = p2p.msghandler.clone();
    tokio::spawn(async move {
        do_handle_pmsg(peersnaprx, peertabletx, hdl2, peer2, conn_read).await;
        let hdlcp = hdl3;
        tokio::spawn(async move { hdlcp.on_disconnect(peer3).await });
    });
    tokio::spawn(async move {
        hdl1.on_connect(peer1).await;
    });
    Ok(())
}

pub(crate) async fn do_handle_pmsg(
    peersnaprx: PeerSnapRx,
    peertabletx: PeerTableCmdTx,
    msghdl: Arc<MsgHandler>,
    peer: Arc<Peer>,
    mut conn_read: OwnedReadHalf,
) {
    {
        let peersnap = peersnaprx.borrow().clone();
        println!(
            "[Peer] {} connected, total {} public {} subnet.",
            peer.nick(),
            peersnap.backbones.len(),
            peersnap.offshoots.len()
        );
    }
    loop {
        let rdres = tokio::select! {
            _ = peer.close_notify.notified() => {
                break
            }
            rd = tcp_read_msg(&mut conn_read, 0) => rd,
        };
        if let Err(_) = rdres {
            break;
        }
        peer.update_active();
        let (ty, msg) = rdres.unwrap();
        if MSG_CUSTOMER == ty {
            if msg.len() < 2 {
                continue;
            }
            let prcp = peer.clone();
            let ty = u16::from_be_bytes(bufcut!(msg, 0, 2));
            let body = msg[2..].to_vec();
            let msghd1 = msghdl.clone();
            tokio::spawn(async move {
                msghd1.on_message(prcp, ty, body).await;
            });
            continue;
        } else if MSG_PING == ty {
            let _ = peer.send_p2p_msg(MSG_PONG, vec![]).await;
        } else if MSG_PONG == ty {
        } else if MSG_CLOSE == ty {
            break;
        } else {
        }
    }
    peer.disconnect();
    let _ = peertabletx.send(PeerTableCmd::Remove(peer.clone())).await;
}

pub(crate) async fn event_loop(p2p: Arc<P2PManage>, mut worker: Worker) -> Rerr {
    let mut printpeer_tkr = new_ticker(60 * 97).await;
    let mut reconnect_tkr = new_ticker(51 * 33).await;
    let mut findnodes_tkr = new_ticker(52 * 60 * 4).await;
    let mut checkpeer_tkr = new_ticker(53 * 3).await;
    let mut boostndes_tkr = new_ticker(54 * 5).await;
    let server_listener = match p2p.server().await {
        Ok(l) => l,
        Err(ref e) => {
            let e = format!("p2p failed to bind port {}: {}", p2p.cnf.listen, e);
            println!("\n[P2P Error] {}\n", e);
            return Err(e);
        }
    };
    let shutdown = p2p.shutdown.clone();
    loop {
        tokio::select! {
            _ = worker.wait() => {
                break
            }
            _ = shutdown.notified() => {
                break
            }
            _ = printpeer_tkr.tick() => {
                p2p.print_conn_peers();
            },
            _ = reconnect_tkr.tick() => {
                let no_nodes = p2p.backbones().len() < 2;
                if no_nodes && p2p.cnf.find_nodes {
                    let _ = connect_stable_then_boot(&p2p).await;
                }
            },
            _ = findnodes_tkr.tick() => {
                if p2p.cnf.find_nodes {
                    p2p.find_nodes().await;
                }
            },
            _ = checkpeer_tkr.tick() => {
                p2p.check_active_nodes().await;
                p2p.ping_nodes().await;
            },
            _ = boostndes_tkr.tick() => {
                p2p.boost_public().await;
                if p2p.backbones().len() == 0 {
                    let _ = connect_stable_then_boot(&p2p).await;
                }
            },
            client = server_listener.accept() => {
                let Ok((client, _)) = terrunbox!( client ) else {
                    continue
                };
                if !p2p.cnf.accept_nodes {
                    continue
                }
                let tobj = p2p.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(&tobj, client, false).await;
                });
            },
            else => break
        }
    }
    p2p.disconnect_all_peers().await;
    println!("[P2P] Event loop end.");
    Ok(())
}
