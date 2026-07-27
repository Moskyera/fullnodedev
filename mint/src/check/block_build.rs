fn impl_packing_next_block(
    this: &HacashMinter,
    engine: &dyn EngineRead,
    txpool: &dyn TxPool,
) -> Box<dyn Any> {
    let engcnf = engine.config();

    let mtcnf = this.cnf;
    let oldblk = engine.latest_block();

    let prevhash = oldblk.hash();
    let mut newdifn = oldblk.difficulty().clone();
    if *newdifn == 0 {
        newdifn = Uint4::from(LOWEST_DIFFICULTY);
    }
    let nexthei = oldblk.height().uint() + 1;
    let prev_ts = oldblk.timestamp().uint();
    // Release chain rejects blk_time <= prev_ts; always strictly increase.
    let nextts = std::cmp::max(curtimes(), prev_ts.saturating_add(1));
    if this.difficulty.is_upgrade_height(nexthei) || nexthei % mtcnf.difficulty_adjust_blocks == 0 {
        let sto = engine.store();
        let src = StoreBlockIntroSource::new(sto.as_ref());
        let (difn, ..) = this.next_difficulty(oldblk.as_read(), nextts, &src);
        newdifn = Uint4::from(difn);
    }
    // create coinbase tx
    let cbtx = create_coinbase_tx(
        nexthei,
        engcnf.miner_message.clone(),
        engcnf.miner_reward_address.clone(),
    );
    let block_author = cbtx.author().unwrap_or_default();
    // create block v1
    let mut intro = BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(nexthei),
            timestamp: Timestamp::from(nextts),
            prevhash: prevhash,
            mrklroot: Hash::default(),
            transaction_count: Uint4::default(),
        },
        meta: BlockMeta {
            nonce: Uint4::default(),
            difficulty: newdifn,
            witness_stage: Fixed2::default(),
        },
    };
    /* debug test
    // intro.head.timestamp = Timestamp::from(1723385108);
    // intro.meta.nonce = Uint4::from(4191621845);
    // cbtx.message = StringTrim16::must(&hex::decode("62616f6b756169000000000000006f56").unwrap());
    // test end*/
    // trs with cbtx
    let mut trslen: usize = 1;
    // MerkleRoot consensus uses tx hash WITH fee.
    // Must keep consistent with chain-side verification (transaction_hash_list(true)).
    let mut trshxs: Vec<Hash> = vec![cbtx.hash_with_fee()];
    // trs
    let mut transactions = DynVecTransaction::default();
    transactions.push(Box::new(cbtx.clone())).unwrap();

    append_valid_tx_pick_from_txpool(
        nexthei,
        &mut trslen,
        &mut trshxs,
        &mut transactions,
        cbtx.size(),
        block_author,
        engine,
        txpool,
    );

    // set mrkl & trs count
    intro.head.mrklroot = calculate_mrklroot(&trshxs);
    intro.head.transaction_count = Uint4::from(trslen as u32);

    // ok
    let block = BlockV1 {
        intro,
        transactions,
    };

    Box::new(block)
}

pub fn create_coinbase_tx(hei: u64, msg: Fixed16, adr: Address) -> crate::TransactionCoinbase {
    // miner reward address must be PRIVAKEY type
    if !adr.is_privakey() {
        panic!("miner reward address {} must be PRIVAKEY type but got version {}",
            adr.to_readable(), adr.version())
    }
    let rwdamt = genesis::block_reward(hei);
    crate::TransactionCoinbase {
        ty: Uint1::from(0), // ccoinbase type = 0
        address: adr,
        reward: rwdamt,
        message: msg,
        extend: CoinbaseExtend::must(CoinbaseExtendDataV1 {
            miner_nonce: Hash::default(),
            witness_count: Uint1::from(0),
        }),
    }
}

fn append_valid_tx_pick_from_txpool(
    pending_hei: u64,
    trslen: &mut usize,
    trshxs: &mut Vec<Hash>,
    trs: &mut DynVecTransaction,
    base_tx_size: usize,
    block_author: Address,
    engine: &dyn EngineRead,
    txpool: &dyn TxPool,
) {
    let engcnf = engine.config();
    let txmaxn = engcnf.max_block_txs;
    let txmaxsz = engcnf.max_block_size;
    let mut allfee = Amount::zero();
    let mut txallsz: usize = base_tx_size;
    let txallsz = &mut txallsz;
    let mut invalidtxhxs = Vec::new();

    let mut sub_state = engine.fork_sub_state();

    macro_rules! ok_push_one_tx {
        ($a: expr, $txsz: expr) => {
            if trs.push($a.tx_clone()).is_err() {
                return false;
            }
            trshxs.push($a.tx_read().hash_with_fee());
            *trslen += 1;
            *txallsz += $txsz;
        };
    }

    macro_rules! check_pick_one_tx {
        ($a: expr) => {
            let txr = $a.tx_read();
            if let Err(..) = engine.try_execute_tx_by_author(txr, pending_hei, &mut sub_state, block_author.clone()) {
                invalidtxhxs.push(txr.hash());
                return true; // execute fail, ignore, next
            };
            let Ok(nf) = allfee.add_mode_u64(&$a.tx().fee_got()) else {
                invalidtxhxs.push(txr.hash());
                return true; // fee size err, ignore, next
            };
            allfee = nf;
        };
    }

    // pick one diamond mint tx, only in a block whose height is divisible by 5
    // (the consensus rule diamond mints are validated against).
    if pending_hei % 5 == 0 {
        let mut pick_dmint = |a: &TxPkg| {
            let txsz = a.data().len();
            if txsz + *txallsz > txmaxsz {
                return true; // try next one
            }
            if *trslen >= txmaxn {
                return false; // end
            }
            // check tx
            check_pick_one_tx!(a);
            // ok push
            ok_push_one_tx!(a, txsz);
            false // end
        };
        txpool.iter_at(TXGID_DIAMINT, &mut pick_dmint).unwrap();
    }

    // pick normal tx
    let mut pick_normal_tx = |a: &TxPkg| {
        let txsz = a.data().len();
        if *trslen >= txmaxn {
            return false; // end, num enough
        }
        if txsz + *txallsz > txmaxsz {
            return true; // skip oversize tx and continue
        }
        // check tx
        check_pick_one_tx!(a);
        ok_push_one_tx!(a, txsz);
        true // next
    };
    txpool.iter_at(TXGID_NORMAL, &mut pick_normal_tx).unwrap();

    // delete invalid txs
    if invalidtxhxs.len() > 0 {
        let _ = txpool.drain(&invalidtxhxs);
    }
}

/********************************************/

#[cfg(test)]
mod tests {
    use super::*;

    use basis::config::EngineConf;

    use std::collections::HashMap;

    fn scoped_protocol_setup() -> protocol::setup::TestSetupScopeGuard {
        let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
        crate::setup::register_protocol_extensions(&mut setup);
        protocol::setup::install_test_scope(setup)
    }

    struct DummyStore;
    impl Store for DummyStore {
        fn status(&self) -> ChainStatus {
            ChainStatus::default()
        }
        fn save_block_data(&self, _: &Hash, _: &Vec<u8>) {}
        fn save_block_hash(&self, _: &BlockHeight, _: &Hash) {}
        fn save_block_hash_path(&self, _: &dyn MemDB) {}
        fn save_batch(&self, _: &dyn MemDB) {}
        fn block_data(&self, _: &Hash) -> Option<Vec<u8>> {
            None
        }
        fn block_hash(&self, _: &BlockHeight) -> Option<Hash> {
            None
        }
        fn block_data_by_height(&self, _: &BlockHeight) -> Option<(Hash, Vec<u8>)> {
            None
        }
    }

    struct DummyState;
    impl State for DummyState {
        fn fork_sub(&self, _: Weak<Box<dyn State>>) -> Box<dyn State> {
            Box::new(DummyState)
        }
        fn merge_sub(&mut self, _: Box<dyn State>) {}
        fn detach(&mut self) {}
        fn clone_state(&self) -> Box<dyn State> {
            Box::new(DummyState)
        }
        fn as_mem(&self) -> &MemMap {
            static EMPTY: std::sync::LazyLock<MemMap> = std::sync::LazyLock::new(HashMap::new);
            &EMPTY
        }
        fn disk(&self) -> Arc<dyn DiskDB> {
            never!()
        }
        fn write_to_disk(&self) {}
        fn get(&self, _: Vec<u8>) -> Option<Vec<u8>> {
            None
        }
        fn set(&mut self, _: Vec<u8>, _: Vec<u8>) {}
        fn del(&mut self, _: Vec<u8>) {}
    }

    struct OneTxPool {
        tx: TxPkg,
    }
    impl TxPool for OneTxPool {
        fn iter_at(&self, gid: usize, each: &mut dyn FnMut(&TxPkg) -> bool) -> Rerr {
            match gid {
                TXGID_NORMAL => {
                    let _ = each(&self.tx);
                }
                TXGID_DIAMINT => {}
                _ => {}
            }
            Ok(())
        }
        fn drain(&self, _: &[Hash]) -> Ret<Vec<TxPkg>> {
            Ok(vec![])
        }
        fn print(&self) -> String {
            s!("OneTxPool")
        }
        fn insert_by(&self, _: TxPkg, _: &dyn Fn(&TxPkg) -> usize) -> Rerr {
            Ok(())
        }
        fn first_at(&self, _: usize) -> Ret<Option<TxPkg>> {
            Ok(None)
        }
        fn retain_at(&self, _: usize, _: &mut dyn FnMut(&TxPkg) -> bool) -> Rerr {
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    struct DummyBlock {
        intro: BlockIntro,
    }
    impl Serialize for DummyBlock {
        fn serialize(&self) -> Vec<u8> {
            self.intro.serialize()
        }
        fn size(&self) -> usize {
            self.intro.size()
        }
    }
    impl Parse for DummyBlock {
        fn parse(&mut self, _: &[u8]) -> Ret<usize> {
            errf!("none")
        }
    }
    impl ToJSON for DummyBlock {
        fn to_json_fmt(&self, _: &JSONFormater) -> String {
            s!("{}")
        }
    }
    impl FromJSON for DummyBlock {
        fn from_json(&mut self, _: &str) -> Ret<()> {
            errf!("none")
        }
    }
    impl BlockExec for DummyBlock {}
    impl BlockRead for DummyBlock {
        fn hash(&self) -> Hash {
            self.intro.hash()
        }
        fn version(&self) -> &Uint1 {
            self.intro.version()
        }
        fn height(&self) -> &BlockHeight {
            self.intro.height()
        }
        fn timestamp(&self) -> &Timestamp {
            self.intro.timestamp()
        }
        fn nonce(&self) -> &Uint4 {
            self.intro.nonce()
        }
        fn difficulty(&self) -> &Uint4 {
            self.intro.difficulty()
        }
        fn prevhash(&self) -> &Hash {
            self.intro.prevhash()
        }
        fn mrklroot(&self) -> &Hash {
            self.intro.mrklroot()
        }
        fn prelude_transaction(&self) -> Ret<&dyn TransactionRead> {
            errf!("none")
        }
        fn transaction_count(&self) -> &Uint4 {
            static ZERO: std::sync::LazyLock<Uint4> = std::sync::LazyLock::new(Uint4::default);
            &ZERO
        }
        fn transactions(&self) -> &Vec<Box<dyn Transaction>> {
            static EMPTY: std::sync::LazyLock<Vec<Box<dyn Transaction>>> = std::sync::LazyLock::new(Vec::new);
            &EMPTY
        }
        fn transaction_hash_list(&self, _: bool) -> Vec<Hash> {
            vec![]
        }
    }
    impl Field for DummyBlock {
        fn new() -> Self {
            never!()
        }
    }
    impl Block for DummyBlock {
        fn as_read(&self) -> &dyn BlockRead {
            self
        }
    }

    struct TestEngine {
        cnf: EngineConf,
        latest: Arc<dyn Block>,
        store: Arc<dyn Store>,
    }
    impl EngineRead for TestEngine {
        fn config(&self) -> &EngineConf {
            &self.cnf
        }
        fn latest_block(&self) -> Arc<dyn Block> {
            self.latest.clone()
        }
        fn store(&self) -> Arc<dyn Store> {
            self.store.clone()
        }
        fn fork_sub_state(&self) -> Box<dyn State> {
            Box::new(DummyState)
        }
        fn try_execute_tx_by(
            &self,
            _: &dyn TransactionRead,
            _: u64,
            _: &mut Box<dyn State>,
        ) -> Rerr {
            Ok(())
        }
        fn try_execute_tx_by_author(
            &self,
            _: &dyn TransactionRead,
            _: u64,
            _: &mut Box<dyn State>,
            author: Address,
        ) -> Rerr {
            if author != self.cnf.miner_reward_address {
                return errf!("unexpected pre-exec block author");
            }
            Ok(())
        }
    }

    fn make_prev_blk(height: u64, timestamp: u64, difficulty: u32) -> DummyBlock {
        DummyBlock {
            intro: BlockIntro {
                head: BlockHead {
                    version: Uint1::from(1),
                    height: BlockHeight::from(height),
                    timestamp: Timestamp::from(timestamp),
                    prevhash: Hash::default(),
                    mrklroot: Hash::default(),
                    transaction_count: Uint4::default(),
                },
                meta: BlockMeta {
                    nonce: Uint4::default(),
                    difficulty: Uint4::from(difficulty),
                    witness_stage: Fixed2::default(),
                },
            },
        }
    }

    fn build_engine(prev_blk: DummyBlock) -> (TestEngine, OneTxPool) {
        let mut tx = TransactionType3::default();
        tx.timestamp = Timestamp::from(1u64);
        tx.fee = Amount::small_mei(1);
        let txp = TxPkg::create(Box::new(tx));
        let tp = OneTxPool { tx: txp };

        let mut cnf = EngineConf::new(&IniObj::default());
        cnf.max_block_txs = 10;
        cnf.max_block_size = 1_000_000;
        cnf.max_tx_size = 100_000;
        cnf.max_tx_actions = 16;
        cnf.lowest_fee_purity = 0;
        cnf.contract_cache_size = 0.0;
        cnf.miner_message = Fixed16::default();
        cnf.miner_reward_address =
            Address::from_readable("1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9").unwrap();

        (
            TestEngine {
                cnf,
                latest: Arc::new(prev_blk),
                store: Arc::new(DummyStore),
            },
            tp,
        )
    }

    #[test]
    fn packing_next_block_merkle_matches_hash_with_fee() {
        let _setup = scoped_protocol_setup();

        let (engine, tp) = build_engine(make_prev_blk(99, 1, LOWEST_DIFFICULTY));
        let minter = HacashMinter::create(&IniObj::default());
        let blk_any = minter.packing_next_block(&engine, &tp);
        let blk = *blk_any.downcast::<BlockV1>().unwrap();

        let want = calculate_mrklroot(&blk.transaction_hash_list(true));
        assert_eq!(*blk.mrklroot(), want);
    }

    #[test]
    fn packing_next_block_keeps_legacy_non_boundary_difficulty_before_upgrade() {
        let _setup = scoped_protocol_setup();
        let prevdiff = LOWEST_DIFFICULTY - 123;
        let prev_blk = make_prev_blk(100, 10_000, prevdiff);
        let (engine, tp) = build_engine(prev_blk);
        let minter = HacashMinter::create(&IniObj::default());
        let blk_any = minter.packing_next_block(&engine, &tp);
        let blk = *blk_any.downcast::<BlockV1>().unwrap();
        assert_eq!(blk.difficulty().uint(), prevdiff);
    }
}
