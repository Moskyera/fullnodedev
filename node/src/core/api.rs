use super::*;

pub struct HacashNode {
    runtime: Arc<NodeRuntime>,
}

impl HacashNode {
    pub fn open(ini: &IniObj, txpool: Arc<dyn TxPool>, engine: Arc<dyn Engine>) -> Self {
        Self {
            runtime: Arc::new(NodeRuntime::open(ini, txpool, engine)),
        }
    }
}

impl HNoder for HacashNode {
    fn start(&self, worker: Worker) {
        self.runtime.start(worker)
    }

    fn submit_transaction(&self, txpkg: &TxPkg, in_async: bool, only_insert_txpool: bool) -> Rerr {
        self.runtime
            .submit_transaction(txpkg, in_async, only_insert_txpool)
    }

    fn submit_block(&self, blkpkg: &BlkPkg, in_async: bool) -> Rerr {
        self.runtime.submit_block(blkpkg, in_async)
    }

    fn engine(&self) -> Arc<dyn Engine> {
        self.runtime.engine()
    }

    fn txpool(&self) -> Arc<dyn TxPool> {
        self.runtime.txpool()
    }

    fn register_p2p_extension(&self, tys: Vec<u16>, ext: Arc<dyn NodeP2PExtension>) -> Rerr {
        self.runtime.register_p2p_extension(tys, ext)
    }

    fn broadcast_p2p_extension_message(&self, key: Hash, ty: u16, body: Vec<u8>) -> Rerr {
        self.runtime.broadcast_p2p_extension_message(key, ty, body)
    }

    fn all_peer_prints(&self) -> Vec<String> {
        self.runtime.all_peer_prints()
    }

    fn pqc_metrics_prometheus(&self) -> Vec<String> {
        self.runtime.pqc_metrics_snapshot().prometheus_lines()
    }

    fn exit(&self) {
        self.runtime.exit()
    }
}
