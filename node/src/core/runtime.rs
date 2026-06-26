use super::*;
use crate::core::protocol::ProtocolAdapter;
use crate::core::transport::TransportAdapter;

pub struct NodeRuntime {
    pub(super) engine: Arc<dyn Engine>,
    pub(super) txpool: Arc<dyn TxPool>,
    pub(super) msghdl: Arc<MsgHandler>,
    pub(super) protocol: ProtocolAdapter,
    pub(super) transport: TransportAdapter,
    pub(super) tasks: Arc<TaskGroup>,
    pub(super) metrics: Arc<StdMutex<RuntimeMetrics>>,
    pub(super) pqc_metrics: Arc<StdMutex<PqcMetrics>>,
    pub(super) exited: AtomicBool,
}

impl NodeRuntime {
    pub fn open(ini: &IniObj, txpool: Arc<dyn TxPool>, engine: Arc<dyn Engine>) -> Self {
        let cnf = NodeConf::new(ini);
        let msghdl = Arc::new(MsgHandler::new(engine.clone(), txpool.clone()));
        let p2p = Arc::new(P2PManage::new(&cnf, msghdl.clone()));
        msghdl.set_p2p_mng(Box::new(PeerMngInst::new(p2p.clone())));
        let protocol = ProtocolAdapter::new(msghdl.clone());
        let transport = TransportAdapter::new(&cnf, p2p.clone());
        let pqc_metrics = Arc::new(StdMutex::new(PqcMetrics::default()));
        install_global_pqc_metrics(pqc_metrics.clone());
        install_pqc_metrics_hook();
        Self {
            engine,
            txpool,
            msghdl,
            protocol,
            transport,
            tasks: TaskGroup::new(),
            metrics: Arc::new(StdMutex::new(RuntimeMetrics::default())),
            pqc_metrics,
            exited: AtomicBool::new(false),
        }
    }

    pub fn pqc_metrics_snapshot(&self) -> PqcMetrics {
        self.pqc_metrics
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    pub fn start(&self, worker: Worker) {
        self.start_network(worker)
    }

    pub fn submit_transaction(
        &self,
        txpkg: &TxPkg,
        in_async: bool,
        only_insert_txpool: bool,
    ) -> Rerr {
        self.submit_transaction_inner(txpkg, in_async, only_insert_txpool)
    }

    pub fn submit_block(&self, blkpkg: &BlkPkg, in_async: bool) -> Rerr {
        self.submit_block_inner(blkpkg, in_async)
    }

    pub fn engine(&self) -> Arc<dyn Engine> {
        self.engine.clone()
    }

    pub fn txpool(&self) -> Arc<dyn TxPool> {
        self.txpool.clone()
    }

    pub fn register_p2p_extension(&self, tys: Vec<u16>, ext: Arc<dyn NodeP2PExtension>) -> Rerr {
        self.msghdl.register_p2p_extension(tys, ext)
    }

    pub fn broadcast_p2p_extension_message(&self, key: Hash, ty: u16, body: Vec<u8>) -> Rerr {
        self.msghdl.broadcast_p2p_extension_message(key, ty, body)
    }

    pub fn all_peer_prints(&self) -> Vec<String> {
        self.transport.peer_prints()
    }

    pub fn running_task_count(&self) -> usize {
        self.tasks.running()
    }

    pub fn exit(&self) {
        self.stop_network()
    }
}
