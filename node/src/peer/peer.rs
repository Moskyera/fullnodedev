use tokio::sync::Notify;

static PEER_AUTO_ID_INCREASE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
enum PeerWriterCmd {
    Send(Vec<u8>),
    Close,
}

#[derive(Debug)]
pub struct Peer {
    pub id: u64,
    pub key: PeerKey,
    pub name: String,
    pub is_public: bool,
    pub is_cntome: bool,
    pub addr: SocketAddr,
    pub active: StdMutex<SystemTime>,
    writer_tx: Sender<PeerWriterCmd>,
    pub writer_closed: StdMutex<bool>,
    pub close_notify: Arc<Notify>,
    pub knows: Knowledge,
}

impl Peer {

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn nick(&self) -> String {
        let mut nick = self.name.clone();
        if self.is_public {
            nick += format!("<{}>", self.addr).as_str();
        }
        nick
    }

    pub fn update_active(&self) {
        *self.active.lock().unwrap() = SystemTime::now();
    }

    pub fn is_writer_closed(&self) -> bool {
        *self.writer_closed.lock().unwrap()
    }

    fn mark_writer_closed(&self) {
        *self.writer_closed.lock().unwrap() = true;
    }

    pub fn disconnect(&self) {
        self.mark_writer_closed();
        let _ = self.writer_tx.try_send(PeerWriterCmd::Close);
        self.close_notify.notify_waiters();
    }

    pub async fn create_with_msg(mut stream: TcpStream, ty: u8, msg: Vec<u8>, mynodeinfo: Vec<u8>) -> Ret<(Arc<Peer>, OwnedReadHalf)> {
        let mut mykeyname = mynodeinfo;
        if mykeyname.len() > PEER_KEY_SIZE*2 {
            mykeyname = mykeyname[4..].to_vec();
        }
        let conn  = &mut stream;
        let mut addr = conn.peer_addr().unwrap();
        let mut is_public = false;
        let mut is_cntome = false;
        let idnamebts: &[u8];
        let mut oginport: u16 = 0;
        if msg.len() < 4 {
            return errf!("msg length too short (min 4)")
        }
        if MSG_REPORT_PEER == ty {
            is_cntome = true;
            oginport = u16::from_be_bytes( bufcut!(msg, 2, 4) );
            idnamebts = &msg[4..];
        }else if MSG_ANSWER_PEER == ty {
            is_public = !addr.ip().is_loopback();
            idnamebts = &msg[..];
        }else{
            return errf!("unsupported msg type {}", ty)
        }
        if idnamebts.len() < 32 {
            return errf!("msg length too short (min 32)")
        }
        let peerkey = bufcut!(idnamebts, 0, PEER_KEY_SIZE);
        let name = Fixed16::from( bufcut!(idnamebts, PEER_KEY_SIZE, PEER_KEY_SIZE*2) ).to_readable().replace(" ", "");
        if peerkey == mykeyname[0..PEER_KEY_SIZE] {
            return  errf!("cannot connect to self")
        }
        if MSG_REPORT_PEER == ty {
            tcp_send_msg(conn, MSG_ANSWER_PEER, mykeyname.clone()).await?;
            if oginport > 0 {
                let mut pubaddr = addr.clone();
                pubaddr.set_port(oginport);
                if let Ok(pb) = tcp_dial_to_check_is_public_id(pubaddr, &peerkey, 3).await {
                    if pb && !addr.ip().is_loopback() {
                        is_public = true;
                        addr.set_port(oginport);
                    }
                }
            }
        }

        let (read_half, write_half) = stream.into_split();
        let (writer_tx, writer_rx) = mpsc::channel(128);

        let atid = PEER_AUTO_ID_INCREASE.fetch_add(1, Ordering::Relaxed) + 1;

        let peer = Peer {
            id: atid,
            key: peerkey,
            name: name,
            is_cntome: is_cntome,
            is_public: is_public,
            addr: addr,
            active: SystemTime::now().into(),
            writer_tx,
            writer_closed: false.into(),
            close_notify: Arc::new(Notify::new()),
            knows: Knowledge::new(500),
        };
        let pptr = Arc::new(peer);
        Peer::spawn_writer(pptr.clone(), write_half, writer_rx);

        Ok((pptr, read_half))
    }

    fn spawn_writer(peer: Arc<Peer>, mut write_half: OwnedWriteHalf, mut writer_rx: Receiver<PeerWriterCmd>) {
        tokio::spawn(async move {
            while let Some(cmd) = writer_rx.recv().await {
                match cmd {
                    PeerWriterCmd::Send(buf) => {
                        // One failed write retires this peer's writer for the
                        // rest of the connection's life, while its reader keeps
                        // running on the other half of the socket. The node then
                        // downloads blocks and transactions perfectly and can
                        // never send anything to that peer again.
                        //
                        // Nothing said so. `Peer::send` returns "peer may be
                        // closed" and the broadcast path discarded that result
                        // with `let _ =`, so a node could hold a transaction in
                        // its pool for an hour, look completely healthy, and be
                        // mute to the whole network. That happened here three
                        // times before anybody could see it.
                        //
                        // Still retire the writer: a half-open socket is not
                        // worth retrying blindly. But say so once, with the peer
                        // named, so the silence has a first line.
                        if let Err(error) = tcp_send(&mut write_half, &buf).await {
                            peer.mark_writer_closed();
                            println!(
                                "[P2P] writer retired for {}: {}. This node can no longer SEND to that peer, though it will keep receiving from it.",
                                peer.nick(),
                                error
                            );
                            break;
                        }
                    }
                    PeerWriterCmd::Close => {
                        let close_msg = tcp_create_msg(MSG_CLOSE, vec![]);
                        let _ = tcp_send(&mut write_half, &close_msg).await;
                        break;
                    }
                }
            }
            peer.mark_writer_closed();
            peer.close_notify.notify_waiters();
        });
    }

}
