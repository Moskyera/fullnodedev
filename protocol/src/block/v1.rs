


// BlockV1
combi_struct_with_parse!{ BlockV1, 
    (self, buf, {
        // intro
        let mut intro = BlockIntro::default();
        let mut seek = intro.parse(buf)?;
        let trslen = *intro.head.transaction_count;
        self.intro = intro;
        // body
        self.transactions.set_count(trslen.into());
        seek += self.transactions.parse(&buf[seek..])?;
        Ok(seek)
    }),
    // head meta
	intro : BlockIntro
	// trs body
	transactions : DynVecTransaction
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTxExecutionReport {
    pub index: usize,
    pub tx_hash: Hash,
    pub tx_type: u8,
    pub fee_got: Amount,
    pub gas_used: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockExecutionReport {
    pub height: u64,
    pub hash: Hash,
    pub tx_count: usize,
    pub total_fee: Amount,
    pub fee_receiver: Option<Address>,
    pub txs: Vec<BlockTxExecutionReport>,
}

pub struct BlockExecutionOutput {
    pub state: Box<dyn State>,
    pub logs: Box<dyn Logs>,
    pub report: BlockExecutionReport,
}



/********************/



macro_rules! block_intro_fn_mount{
    ($fname:ident, $rty:ty) => (
        fn $fname(&self) -> &$rty {
            self.intro.$fname()
        }
    )
}


impl BlockRead for BlockV1 {

    fn hash(&self) -> Hash {
        self.intro.hash()
    }

    block_intro_fn_mount!{version, Uint1}
    block_intro_fn_mount!{height, BlockHeight}
    block_intro_fn_mount!{timestamp, Timestamp}
    block_intro_fn_mount!{difficulty, Uint4}
    block_intro_fn_mount!{nonce, Uint4}
    block_intro_fn_mount!{prevhash, Hash}
    block_intro_fn_mount!{mrklroot, Hash}
    block_intro_fn_mount!{transaction_count, Uint4}

    fn transaction_hash_list(&self, hash_with_fee: bool) -> Vec<Hash> {
        let mut list = vec![];
        // println!("self.transactions.as_list: {}", self.transactions.as_list().len());
        for t in self.transactions.as_list() {
            if hash_with_fee {
                list.push(t.hash_with_fee())
            }else{
                list.push(t.hash())
            }
        }
        list
    }

    fn transactions(&self) -> &Vec<Box<dyn Transaction>> {
        self.transactions.as_list()
    }

    fn prelude_transaction(&self) -> Ret<&dyn TransactionRead> {
        let txs = self.transactions();
        if txs.is_empty() {
            return errf!("block must have prelude tx")
        }
        Ok(txs[0].as_read())
    }
}

impl BlockExec for BlockV1 {
    fn execute(&self, ccnf: ChainInfo, state: Box<dyn State>, logs: Box<dyn Logs>) -> Ret<(Box<dyn State>, Box<dyn Logs>)> {
        let out = self.execute_with_report(ccnf, state, logs)?;
        Ok((out.state, out.logs))
    }
}

impl BlockV1 {
    pub fn execute_with_report(
        &self,
        ccnf: ChainInfo,
        state: Box<dyn State>,
        logs: Box<dyn Logs>,
    ) -> Ret<BlockExecutionOutput> {
        // create env
        let mut env = Env {
            chain: ccnf,
            block: BlkInfo {
                height: self.height().uint(),
                hash: self.hash(),
                author: Address::default(),
            },
            tx: TxInfo::default(),
        };
        let ptx = self.prelude_transaction()?;
        let fee_receiver = ptx.fee_receiver();
        if let Some(author) = ptx.author() {
            env.block.author = author;
        }
        // create ctx
        let mut ctxobj = context::ContextInst::new(env, state, logs, ptx);
        let ctx = &mut ctxobj;
        let txs = self.transactions();
        let mut total_fee = Amount::zero();
        let mut reports = Vec::with_capacity(txs.len());
        // exec each tx
        for (index, tx) in txs.iter().enumerate() {
            ctx.reset_for_new_tx(tx.as_read());
            tx.execute(ctx)?; // do exec
            let gas_used = ctx.gas_diag().used_net;
            total_fee = total_fee.add_mode_u64(&tx.fee_got())?; // add fee
            reports.push(BlockTxExecutionReport {
                index,
                tx_hash: tx.hash(),
                tx_type: tx.ty(),
                fee_got: tx.fee_got(),
                gas_used,
            });
        }
        if let Some(fee_receiver) = fee_receiver.filter(|_| total_fee.is_positive()) {
            operate::hac_add(ctx, &fee_receiver, &total_fee)?;
        }
        let (state, logs) = ctxobj.release();
        Ok(BlockExecutionOutput {
            state,
            logs,
            report: BlockExecutionReport {
                height: self.height().uint(),
                hash: self.hash(),
                tx_count: txs.len(),
                total_fee,
                fee_receiver,
                txs: reports,
            },
        })

    }
}




/********************/



impl Block for BlockV1 {

    fn as_read(&self) -> &dyn BlockRead { 
        self
    }

    fn update_mrklroot(&mut self) {
        let hash_with_fee = true;
        let hxlist = self.transaction_hash_list(hash_with_fee);
        let mrkl = calculate_mrklroot(&hxlist);
        self.set_mrklroot(mrkl);
    }

    fn set_mrklroot(&mut self, mkrt: Hash) {
        self.intro.head.mrklroot = mkrt;
    }

	fn set_nonce(&mut self, nonce: Uint4) {
        self.intro.meta.nonce = nonce;
	}

    fn replace_transaction(&mut self, i: usize, v: Box<dyn Transaction>) -> Rerr {
        self.transactions.replace(i, v)
    }

    fn push_transaction(&mut self, tx: Box<dyn Transaction>) -> Rerr {
        let ct = &mut self.intro.head.transaction_count;
        if ct.uint() + 1 == u32::MAX  {
            return errf!("transaction overflow")
        }
        *ct += 1;
        self.transactions.set_count(*ct);
        self.transactions.push(tx)
    }



    
}



/********************/


impl BlockV1 {

    pub const VERSION: u8 = 1;

    pub fn new() -> BlockV1 {
        let mut blk = <BlockV1 as Field>::new();
        blk.intro.head.version = Uint1::from(Self::VERSION);
        blk 
    }
}


