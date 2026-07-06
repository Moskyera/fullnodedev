
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxExecutionReport {
    pub tx_hash: Hash,
    pub height: u64,
    pub action_count: usize,
    pub success: bool,
    pub error: Option<String>,
    pub gas: Option<ctx::GasDiag>,
    pub state_merged: bool,
}

impl TxExecutionReport {
    fn success(tx: &dyn TransactionRead, height: u64, gas: ctx::GasDiag) -> Self {
        Self {
            tx_hash: tx.hash(),
            height,
            action_count: tx.action_count(),
            success: true,
            error: None,
            gas: Some(gas),
            state_merged: true,
        }
    }

    fn failure(
        tx: &dyn TransactionRead,
        height: u64,
        error: String,
        gas: Option<ctx::GasDiag>,
    ) -> Self {
        Self {
            tx_hash: tx.hash(),
            height,
            action_count: tx.action_count(),
            success: false,
            error: Some(error),
            gas,
            state_merged: false,
        }
    }

    pub fn into_result(self) -> Rerr {
        if self.success {
            Ok(())
        } else {
            Err(self.error.unwrap_or_else(|| "tx execution failed".to_owned()))
        }
    }
}

fn try_execute_tx_by_author(
    this: &ChainEngine,
    tx: &dyn TransactionRead,
    pd_hei: u64,
    sub_state: &mut Box<dyn State>,
    author: Address,
) -> Rerr {
    try_execute_tx_report_by_author(this, tx, pd_hei, sub_state, author).into_result()
}

fn try_execute_tx_report_by_author(
    this: &ChainEngine,
    tx: &dyn TransactionRead,
    pd_hei: u64,
    sub_state: &mut Box<dyn State>,
    author: Address,
) -> TxExecutionReport {
    let cnf = &this.cnf;
    if protocol::transaction::is_prelude_tx_type(tx.ty()) {
        return TxExecutionReport::failure(tx, pd_hei, "cannot submit author tx".to_owned(), None);
    }
    let an = tx.action_count();
    if an != tx.actions().len() {
        return TxExecutionReport::failure(
            tx,
            pd_hei,
            "tx action count does not match".to_owned(),
            None,
        );
    }
    if an > cnf.max_tx_actions {
        return TxExecutionReport::failure(
            tx,
            pd_hei,
            format!("tx action count cannot exceed {}", cnf.max_tx_actions),
            None,
        );
    }
    if tx.size() as usize > cnf.max_tx_size {
        return TxExecutionReport::failure(
            tx,
            pd_hei,
            format!("tx size cannot exceed {} bytes", cnf.max_tx_size),
            None,
        );
    }
    let cur_time = curtimes();
    if tx.timestamp().uint() > cur_time {
        return TxExecutionReport::failure(
            tx,
            pd_hei,
            format!("tx timestamp {} cannot exceed now {}", tx.timestamp(), cur_time),
            None,
        );
    }
    let hash = Hash::from([0u8; 32]);
    let env = Env {
        chain: ChainInfo {
            id: this.cnf.chain_id,
            diamond_form: this.cnf.diamond_form,
            fast_sync: false,
        },
        block: BlkInfo {
            height: pd_hei,
            hash,
            author,
        },
        tx: create_tx_info(tx),
    };
    // Isolate execution per tx:
    // - build an internal sub-state fork from current accumulated `sub_state`
    // - merge on success
    // - discard on failure
    let parent: Arc<Box<dyn State>> = sub_state.clone_state().into();
    let sub = parent.fork_sub(Arc::downgrade(&parent));
    let log = this.logs.next(0);
    let mut ctxobj = ctx::ContextInst::new(env, sub, Box::new(log), tx);
    let exec_res = tx.execute(&mut ctxobj);
    let gas = ctxobj.gas_diag();
    let (sta, _) = ctxobj.release();
    match exec_res {
        Ok(()) => {
            sub_state.merge_sub(sta);
            TxExecutionReport::success(tx, pd_hei, gas)
        }
        Err(e) => TxExecutionReport::failure(tx, pd_hei, e, Some(gas)),
    }
}
