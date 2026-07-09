
// Hacash node
pub trait HNoder: Send + Sync {

    fn start(&self, _: Worker) {}

    fn submit_transaction(&self, _: &TxPkg, _is_async: bool, _only_insert_txpool: bool) -> Rerr { never!() }
    fn submit_block(&self, _: &BlkPkg, _is_async: bool) -> Rerr { never!() }

    fn engine(&self) -> Arc<dyn Engine> { never!() }
    fn txpool(&self) -> Arc<dyn TxPool> { never!() }

    fn register_p2p_extension(&self, _: Vec<u16>, _: Arc<dyn NodeP2PExtension>) -> Rerr {
        errf!("p2p extension registration not supported")
    }

    fn broadcast_p2p_extension_message(&self, _: Hash, _: u16, _: Vec<u8>) -> Rerr {
        errf!("p2p extension broadcast not supported")
    }

    fn all_peer_prints(&self) -> Vec<String> { never!() }

    /// Prometheus-style lines for post-quantum metrics (empty when unsupported).
    fn pqc_metrics_prometheus(&self) -> Vec<String> { Vec::new() }

    fn exit(&self) {}
    
}

