
impl Engine for ChainEngine {
    fn as_read(&self) -> &dyn EngineRead { self }
    
    fn discover(&self, blk: BlkPkg) -> Rerr {
        let _isrtlock = inserting_lock(self, ISRT_STAT_DISCOVER,
            "the blockchain is syncing and cannot insert newly discovered block"
        )?;
        let _lk = self.syncing.lock().unwrap();
        let mut tree = self.tree.write().unwrap();
        let rid = insert_by(self, tree.deref_mut(), blk)?;
        let recent_ctx = maybe!(
            self.cnf.recent_blocks,
            Some((rid.block.block_clone(), tree.root_height())),
            None
        );
        let became_head = rid.head_change.is_some();
        let origin = rid.block.origin();
        let root_roll_blk = rid.root_change.as_ref().map(|root| root.block());
        roll_by(self, rid)?;

        // Update runtime caches only after the block is fully accepted and rolled.
        if let Some((blk, root_height)) = recent_ctx {
            record_recent(self, blk.as_read(), root_height);
        }
        if let Some(root_blk) = root_roll_blk {
            if origin == BlkOrigin::Discover || origin == BlkOrigin::Sync {
                self.minter.blk_root_roll(root_blk.as_read(), self.store.as_ref());
            }
        }
        if became_head {
            let head = tree.head().block();
            let head = head.as_read();
            if self.cnf.average_fee_purity {
                record_avgfee(self, head);
            }
            
            // Notify miner workers that a new block has been accepted
            #[cfg(feature = "mint")]
            {
                // We need to notify the condition variable in mint/src/api/common.rs
                // Since we can't easily access it directly from chain crate, 
                // we'll rely on the miner_notice loop to pick up the new height
                // The Condvar approach in miner_notice will handle this automatically
                // when it checks the latest height.
            }
        }
        Ok(())
    }

    fn synchronize(&self, datas: Vec<u8>) -> Rerr {
        let _isrtlock = inserting_lock(self, ISRT_STAT_SYNCING,
            "the blockchain is syncing and must wait"
        )?;
        let _lk = self.syncing.lock().unwrap();
        do_synchronize(self, datas.into(), BlkOrigin::Sync)
    }

    fn exit(&self) { 
        let _lk = self.syncing.lock().unwrap();
        self.minter.exit();
        self.scaner.exit();
        println!("[Engine] exit.");
    }
}

impl EngineRead for ChainEngine {

    fn config(&self) -> &EngineConf { &self.cnf }

    fn latest_block(&self) -> Arc<dyn Block> {
        self.tree.read().unwrap().head().block()
    }
    
    fn store(&self) -> Arc<dyn Store> { self.store.clone() }
    
    fn state(&self) -> Arc<Box<dyn State>> {
        self.tree.read().unwrap().head().state()
    }

    fn minter(&self) -> &dyn Minter { self.minter.as_ref() }
    
    fn logs(&self) -> Arc<dyn Logs> { self.logs.clone() }
    
    fn fork_sub_state(&self) -> Box<dyn State> {
         let tree = self.tree.read().unwrap();
         let state = tree.head().state();
         state.fork_sub(Arc::downgrade(&state))
    }

    fn recent_blocks(&self) -> Vec<Arc<RecentBlockInfo>> { 
        self.recent_blocks.lock().unwrap().iter().cloned().collect()
    }
    
    fn try_execute_tx(&self, tx: &dyn TransactionRead) -> Rerr {
        let height = self.latest_block().height().uint() + 1;
        self.try_execute_tx_by(tx, height, &mut self.fork_sub_state())
    }

    fn try_execute_tx_by(&self, tx: &dyn TransactionRead, pd_hei: u64, sub_state: &mut Box<dyn State>) -> Rerr {
        self.try_execute_tx_by_author(tx, pd_hei, sub_state, self.cnf.external_exec_author())
    }

    fn try_execute_tx_by_author(
        &self,
        tx: &dyn TransactionRead,
        pd_hei: u64,
        sub_state: &mut Box<dyn State>,
        author: Address,
    ) -> Rerr {
        try_execute_tx_by_author(self, tx, pd_hei, sub_state, author)
    }

    fn average_fee_purity(&self) -> u64 {
        let avgfs = self.avgfees.lock().unwrap();
        let al = avgfs.len();
        if al == 0 {
            return self.cnf.lowest_fee_purity
        }
        let ttn: u128 = avgfs.iter().map(|v| *v as u128).sum();
        (ttn / avgfs.len() as u128) as u64
    }
}

impl ChainEngine {
    pub fn try_execute_tx_report(&self, tx: &dyn TransactionRead) -> TxExecutionReport {
        let height = self.latest_block().height().uint() + 1;
        let mut sub_state = self.fork_sub_state();
        self.try_execute_tx_report_by(tx, height, &mut sub_state)
    }

    pub fn try_execute_tx_report_by(
        &self,
        tx: &dyn TransactionRead,
        pd_hei: u64,
        sub_state: &mut Box<dyn State>,
    ) -> TxExecutionReport {
        self.try_execute_tx_report_by_author(
            tx,
            pd_hei,
            sub_state,
            self.cnf.external_exec_author(),
        )
    }

    pub fn try_execute_tx_report_by_author(
        &self,
        tx: &dyn TransactionRead,
        pd_hei: u64,
        sub_state: &mut Box<dyn State>,
        author: Address,
    ) -> TxExecutionReport {
        try_execute_tx_report_by_author(self, tx, pd_hei, sub_state, author)
    }
}
