const HXS: usize = 32; // hash size
const DIFFICULTY_UPGRADE_EPOCH: u64 = 2563; // 738144 height

type CachedBlockIntro = (u64, u32, [u8; HXS]); // (time, diffnum, diffhash)

pub(crate) struct StoreBlockIntroSource<'a> {
    store: &'a dyn Store,
}

impl<'a> StoreBlockIntroSource<'a> {
    pub(crate) fn new(store: &'a dyn Store) -> Self {
        Self { store }
    }
}

impl BlockIntroSource for StoreBlockIntroSource<'_> {
    fn cache_height_limit(&self) -> u64 {
        self.store.status().root_height.uint()
    }

    fn block_intro(&self, hei: u64) -> Option<Box<dyn BlockRead>> {
        if hei == 0 {
            return Some(Box::new(genesis::genesis_block().intro.clone()));
        }
        let (_, blkdts) = self.store.block_data_by_height(&BlockHeight::from(hei))?;
        BlockIntro::build(&blkdts).ok().map(|v| Box::new(v) as Box<dyn BlockRead>)
    }
}


#[derive(Clone)]
pub struct DifficultyGnr {
    cnf: MintConf,
    block_caches: Arc<Mutex<HashMap<u64, CachedBlockIntro>>>,
    last_target_cache: Arc<Mutex<Option<(u64, u32, u64, [u8; HXS], BigUint)>>>, // (next_height, prevdiff, prevtime, tarhash, target)
}

impl DifficultyGnr {
    pub fn new(cnf: MintConf) -> DifficultyGnr {
        DifficultyGnr {
            cnf: cnf,
            block_caches: Arc::default(),
            last_target_cache: Arc::default(),
        }
    }
}

impl DifficultyGnr {
    fn upgrade_height(&self) -> u64 {
        if self.cnf.is_mainnet() {
            DIFFICULTY_UPGRADE_EPOCH * self.cnf.difficulty_adjust_blocks
        } else {
            1
        }
    }

    fn asert_upgrade_height(&self) -> u64 {
        if self.cnf.is_mainnet() {
            ASERT_UPGRADE_HEIGHT
        } else {
            self.window_blocks() + 2
        }
    }

    fn window_blocks(&self) -> u64 {
        self.cnf.difficulty_adjust_blocks
    }

    fn group_blocks(&self) -> u64 {
        self.cnf.difficulty_group_blocks
    }

    fn window_groups(&self) -> u64 {
        self.window_blocks() / self.group_blocks()
    }

    fn cache_blocks(&self) -> u64 {
        self.window_blocks() + self.group_blocks() * 3
    }

    pub fn is_upgrade_height(&self, hei: u64) -> bool {
        hei >= self.upgrade_height()
    }

    pub fn is_asert_height(&self, hei: u64) -> bool {
        hei >= self.asert_upgrade_height()
    }

    pub fn is_historical_lwma_height(&self, hei: u64) -> bool {
        self.cnf.is_mainnet() && self.is_upgrade_height(hei) && !self.is_asert_height(hei)
    }

    fn use_bootstrap_rule(&self, hei: u64) -> bool {
        hei <= self.window_blocks() + 1
    }

    pub fn cache_block_intro(&self, blk: &dyn BlockRead) {
        let item = (blk.timestamp().uint(), blk.difficulty().uint(), u32_to_hash(blk.difficulty().uint()));
        let mut cache = self.block_caches.lock().unwrap();
        cache.insert(blk.height().uint(), item);
        self.prune_cache_after_insert(&mut cache, blk.height().uint());
        let mut last = self.last_target_cache.lock().unwrap();
        *last = None;
    }

    pub fn cache_root_block_intro(&self, blk: &dyn BlockRead) {
        {
            let mut cache = self.block_caches.lock().unwrap();
            cache.clear();
        }
        self.cache_block_intro(blk)
    }

    fn req_block_intro(&self, hei: u64, src: &dyn BlockIntroSource) -> CachedBlockIntro {
        let cacheable = hei <= src.cache_height_limit();
        if cacheable {
            let cache = self.block_caches.lock().unwrap();
            if let Some(blk_time) = cache.get(&hei) {
                return *blk_time;
            }
        }
        let Some(intro) = src.block_intro(hei) else {
            panic!("difficulty block missing: block_height={}", hei);
        };
        let diffcty = intro.difficulty().uint();
        let item = (intro.timestamp().uint(), diffcty, u32_to_hash(diffcty));
        if !cacheable {
            return item;
        }
        let mut cache = self.block_caches.lock().unwrap();
        if let Some(blk_time) = cache.get(&hei) {
            return *blk_time;
        }
        cache.insert(hei, item);
        self.prune_cache_after_insert(&mut cache, hei);
        item
    }

    fn prune_cache_after_insert(&self, cache: &mut HashMap<u64, CachedBlockIntro>, newest: u64) {
        if cache.len() as u64 <= self.cache_blocks() {
            return;
        }
        let min_keep = newest.saturating_sub(self.cache_blocks() - 1);
        cache.retain(|hei, _| *hei >= min_keep);
    }

    fn target_bootstrap(&self) -> DifficultyTarget {
        DifficultyTarget::from_num(LOWEST_DIFFICULTY)
    }

    fn target_weighted_sliding(
        &self,
        prevdiff: u32,
        prevblkt: u64,
        hei: u64,
        src: &dyn BlockIntroSource,
    ) -> DifficultyTarget {
        let cacheable = hei - 1 <= src.cache_height_limit();
        if cacheable {
            if let Some((cached_hei, cached_diff, cached_time, cached_hash, cached_target)) = &*self.last_target_cache.lock().unwrap() {
                if *cached_hei == hei && *cached_diff == prevdiff && *cached_time == prevblkt {
                    return DifficultyTarget::new(hash_to_u32(cached_hash), *cached_hash, cached_target.clone());
                }
            }
        }
        let prevbign = u32_to_biguint(prevdiff);
        let mut observed: u128 = 0;
        let mut expected: u128 = 0;
        let group_target = (self.cnf.each_block_target_time * self.group_blocks()) as u128;
        let mut bound = hei - self.window_blocks() - 1;
        let mut prev_time = self.req_block_intro(bound, src).0;
        let last_group = self.window_groups() - 1;
        for i in 0..self.window_groups() {
            let next_time = if i == last_group {
                prevblkt
            } else {
                bound += self.group_blocks();
                self.req_block_intro(bound, src).0
            };
            let weight = (i + 1) as u128;
            observed += (next_time.saturating_sub(prev_time) as u128) * weight;
            expected += group_target * weight;
            prev_time = next_time;
        }
        let targetbign = clamp_target_half_double(&prevbign, scale_target_by_ratio(&prevbign, observed, expected));
        let target = DifficultyTarget::from_big(targetbign);
        if cacheable {
            let mut last = self.last_target_cache.lock().unwrap();
            *last = Some((hei, prevdiff, prevblkt, target.hash, target.big.clone()));
        }
        target
    }


    pub fn target(
        &self,
        mcnf: &MintConf,
        prevdiff: u32,
        prevblkt: u64,
        hei: u64,
        blkt: u64,
        src: &dyn BlockIntroSource,
    ) -> (u32, [u8; 32], BigUint) {
        if mcnf.is_mainnet() && !self.is_upgrade_height(hei) {
            return self.target_legacy(mcnf, prevdiff, prevblkt, hei, src).into_tuple()
        }
        if self.is_asert_height(hei) {
            return self.target_asert(prevdiff, prevblkt, hei, blkt, src).into_tuple()
        }
        if self.use_bootstrap_rule(hei) {
            return self.target_bootstrap().into_tuple()
        }
        self.target_weighted_sliding(prevdiff,prevblkt, hei, src).into_tuple()
    }
}

impl HacashMinter {
    fn next_difficulty(&self, prev: &dyn BlockRead, blkt: u64, src: &dyn BlockIntroSource) -> (u32, [u8; 32], BigUint) {
        let pdif = prev.difficulty().uint();
        let ptim = prev.timestamp().uint();
        let nhei = prev.height().uint() + 1;
        self.difficulty.target(&self.cnf, pdif, ptim, nhei, blkt, src)
    }
}

#[cfg(test)]
mod difficulty_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scoped_protocol_setup() -> protocol::setup::TestSetupScopeGuard {
        let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
        crate::setup::register_protocol_extensions(&mut setup);
        protocol::setup::install_test_scope(setup)
    }

    struct CountingStore {
        reads: AtomicUsize,
        root_height: u64,
        blocks: HashMap<u64, Vec<u8>>,
    }

    impl Store for CountingStore {
        fn status(&self) -> ChainStatus {
            ChainStatus {
                root_height: BlockHeight::from(self.root_height),
                last_height: BlockHeight::from(self.root_height),
            }
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
        fn block_data_by_height(&self, hei: &BlockHeight) -> Option<(Hash, Vec<u8>)> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.blocks.get(&hei.uint()).cloned().map(|v| (Hash::default(), v))
        }
    }

    struct TestIntroSource {
        cache_height_limit: u64,
        blocks: HashMap<u64, Vec<u8>>,
    }

    impl TestIntroSource {
        fn new(cache_height_limit: u64, blocks: HashMap<u64, Vec<u8>>) -> Self {
            Self { cache_height_limit, blocks }
        }
    }

    impl BlockIntroSource for TestIntroSource {
        fn cache_height_limit(&self) -> u64 {
            self.cache_height_limit
        }

        fn block_intro(&self, hei: u64) -> Option<Box<dyn BlockRead>> {
            if hei == 0 {
                return Some(Box::new(genesis::genesis_block().intro.clone()));
            }
            self.blocks
                .get(&hei)
                .and_then(|v| BlockIntro::build(v).ok())
                .map(|v| Box::new(v) as Box<dyn BlockRead>)
        }
    }

    fn build_intro(height: u64, timestamp: u64, difficulty: u32) -> Vec<u8> {
        BlockIntro {
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
        }.serialize()
    }

    #[test]
    fn req_cycle_block_reads_disk_once_after_cache_warmup() {
        let _setup = scoped_protocol_setup();
        let dgnr = DifficultyGnr::new(MintConf::new(&IniObj::new()));
        let mut blocks = HashMap::new();
        blocks.insert(288, build_intro(288, 1000, LOWEST_DIFFICULTY));
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            root_height: Uint5::MAX,
            blocks,
        };

        let src = StoreBlockIntroSource::new(&store);
        let first = dgnr.req_cycle_block(288, &src);
        let second = dgnr.req_cycle_block(288, &src);

        assert_eq!(first, second);
        assert_eq!(store.reads.load(Ordering::SeqCst), 1);
    }

    fn new_test_dgnr() -> DifficultyGnr {
        DifficultyGnr::new(MintConf::new(&IniObj::new()))
    }

    fn new_test_dgnr_with(adjust_blocks: u64, group_blocks: u64, target_time: u64, chain_id: u64) -> DifficultyGnr {
        let mut ini = IniObj::new();
        let mut mint = HashMap::new();
        mint.insert("chain_id".to_string(), Some(chain_id.to_string()));
        mint.insert("difficulty_adjust_blocks".to_string(), Some(adjust_blocks.to_string()));
        mint.insert("difficulty_group_blocks".to_string(), Some(group_blocks.to_string()));
        mint.insert("each_block_target_time".to_string(), Some(target_time.to_string()));
        ini.insert("mint".to_string(), mint);
        DifficultyGnr::new(MintConf::new(&ini))
    }

    fn test_upgrade_height(dgnr: &DifficultyGnr) -> u64 {
        dgnr.upgrade_height()
    }

    #[test]
    fn weighted_window_prefers_memory_cache_before_store() {
        let _setup = scoped_protocol_setup();
        let dgnr = new_test_dgnr();
        let mut blocks = HashMap::new();
        let upgrade_height = test_upgrade_height(&dgnr);
        let start = upgrade_height - dgnr.window_blocks() - 1;
        for i in 0..=dgnr.window_blocks() {
            let hei = start + i;
            blocks.insert(hei, build_intro(hei, 10_000 + i * 300, LOWEST_DIFFICULTY));
        }
        let store = CountingStore {
            reads: AtomicUsize::new(0),
            root_height: Uint5::MAX,
            blocks,
        };

        for i in 0..dgnr.cache_blocks() {
            let hei = upgrade_height - dgnr.cache_blocks() + i;
            let intro = BlockIntro::build(&build_intro(hei, 20_000 + i * 300, LOWEST_DIFFICULTY)).unwrap();
            dgnr.cache_block_intro(&intro);
        }
        let reads_before = store.reads.load(Ordering::SeqCst);
        let src = StoreBlockIntroSource::new(&store);
        let _ = dgnr.req_block_intro(upgrade_height - 1, &src);
        let reads_after = store.reads.load(Ordering::SeqCst);
        assert_eq!(reads_before, reads_after);
    }

    #[test]
    fn weighted_window_clamps_to_half_and_double() {
        let _setup = scoped_protocol_setup();
        let prevdiff = LOWEST_DIFFICULTY - 1024;
        let dgnr_fast = new_test_dgnr();
        let upgrade_height = test_upgrade_height(&dgnr_fast);
        let start = upgrade_height - dgnr_fast.window_blocks() - 1;

        let mut fast_blocks = HashMap::new();
        for i in 0..=dgnr_fast.window_blocks() {
            let hei = start + i;
            fast_blocks.insert(hei, build_intro(hei, 10_000 + i, prevdiff));
        }
        let fast_store = CountingStore {
            reads: AtomicUsize::new(0),
            root_height: Uint5::MAX,
            blocks: fast_blocks,
        };
        let prev_target = u32_to_biguint(prevdiff);
        let fast_target = dgnr_fast.target_weighted_sliding(prevdiff, 10_000 + dgnr_fast.window_blocks(), upgrade_height, &StoreBlockIntroSource::new(&fast_store)).big;
        assert!(fast_target <= prev_target.clone());
        assert!(fast_target >= prev_target.clone() / BigUint::from(2u8));

        let dgnr_slow = new_test_dgnr();
        let mut slow_blocks = HashMap::new();
        for i in 0..=dgnr_slow.window_blocks() {
            let hei = start + i;
            slow_blocks.insert(hei, build_intro(hei, 10_000 + i * 10_000, prevdiff));
        }
        let slow_store = CountingStore {
            reads: AtomicUsize::new(0),
            root_height: Uint5::MAX,
            blocks: slow_blocks,
        };
        let slow_target = dgnr_slow.target_weighted_sliding(prevdiff, 10_000 + dgnr_slow.window_blocks() * 10_000, upgrade_height, &StoreBlockIntroSource::new(&slow_store)).big;
        assert!(slow_target >= prev_target.clone());
        assert!(slow_target <= prev_target * BigUint::from(2u8));
    }

    #[test]
    fn weighted_window_scales_with_config_changes() {
        let _setup = scoped_protocol_setup();
        let prevdiff = LOWEST_DIFFICULTY - 1024;
        let cases = [
            (288u64, 4u64, 300u64),
            (144u64, 4u64, 150u64),
            (120u64, 5u64, 60u64),
        ];

        for (adjust_blocks, group_blocks, target_time) in cases {
            let dgnr = new_test_dgnr_with(adjust_blocks, group_blocks, target_time, 9);
            let next_height = dgnr.window_blocks() + 2;
            let start = 1;
            let mut blocks = HashMap::new();
            for i in 0..=dgnr.window_blocks() {
                let hei = start + i;
                blocks.insert(hei, build_intro(hei, 50_000 + i * target_time, prevdiff));
            }
            let store = CountingStore {
                reads: AtomicUsize::new(0),
                root_height: Uint5::MAX,
                blocks,
            };
            let prev_time = 50_000 + dgnr.window_blocks() * target_time;
            let target = dgnr.target_weighted_sliding(prevdiff, prev_time, next_height, &StoreBlockIntroSource::new(&store));
            let prev_target = u32_to_biguint(prevdiff);
            assert!(target.big <= prev_target.clone() * BigUint::from(2u8));
            assert!(target.big >= prev_target.clone() / BigUint::from(2u8));
            let ratio_num = if target.big > prev_target.clone() { target.big.clone() } else { prev_target.clone() };
            let ratio_den = if target.big > prev_target.clone() { prev_target.clone() } else { target.big.clone() };
            assert!(ratio_num <= ratio_den * BigUint::from(2u8));
        }
    }

    #[test]
    fn weighted_window_keeps_target_when_observed_equals_expected() {
        let _setup = scoped_protocol_setup();
        let prevdiff = LOWEST_DIFFICULTY - 1024;
        let dgnr = new_test_dgnr_with(120, 5, 60, 9);
        let next_height = dgnr.window_blocks() + 2;
        let mut blocks = HashMap::new();
        for i in 0..=dgnr.window_blocks() {
            let hei = 1 + i;
            blocks.insert(hei, build_intro(hei, 1000 + i * dgnr.cnf.each_block_target_time, prevdiff));
        }
        let store = CountingStore { reads: AtomicUsize::new(0), root_height: Uint5::MAX, blocks };
        let prev_time = 1000 + dgnr.window_blocks() * dgnr.cnf.each_block_target_time;
        let target = dgnr.target_weighted_sliding(prevdiff, prev_time, next_height, &StoreBlockIntroSource::new(&store));
        assert_eq!(target.num, prevdiff);
    }

    #[test]
    fn weighted_window_latest_groups_have_larger_impact_than_oldest_groups() {
        let _setup = scoped_protocol_setup();
        let prevdiff = LOWEST_DIFFICULTY - 1024;
        let dgnr = new_test_dgnr();
        let next_height = dgnr.window_blocks() + 2;
        let base_time = 10_000u64;

        let mut oldest_blocks = HashMap::new();
        let mut newest_blocks = HashMap::new();
        for i in 0..=dgnr.window_blocks() {
            let hei = 1 + i;
            let mut ts = base_time + i * dgnr.cnf.each_block_target_time;
            if i == dgnr.group_blocks() {
                ts += dgnr.cnf.each_block_target_time;
            }
            oldest_blocks.insert(hei, build_intro(hei, ts, prevdiff));
            let mut ts2 = base_time + i * dgnr.cnf.each_block_target_time;
            if i == dgnr.window_blocks() {
                ts2 += dgnr.cnf.each_block_target_time;
            }
            newest_blocks.insert(hei, build_intro(hei, ts2, prevdiff));
        }
        let oldest_store = CountingStore { reads: AtomicUsize::new(0), root_height: Uint5::MAX, blocks: oldest_blocks };
        let newest_store = CountingStore { reads: AtomicUsize::new(0), root_height: Uint5::MAX, blocks: newest_blocks };
        let prev_time_oldest = base_time + dgnr.window_blocks() * dgnr.cnf.each_block_target_time;
        let prev_time_newest = prev_time_oldest + dgnr.cnf.each_block_target_time;
        let oldest = dgnr.target_weighted_sliding(prevdiff, prev_time_oldest, next_height, &StoreBlockIntroSource::new(&oldest_store));
        let newest = dgnr.target_weighted_sliding(prevdiff, prev_time_newest, next_height, &StoreBlockIntroSource::new(&newest_store));
        let prev_target = u32_to_biguint(prevdiff);
        let oldest_delta = if oldest.big > prev_target { oldest.big.clone() - prev_target.clone() } else { prev_target.clone() - oldest.big.clone() };
        let newest_delta = if newest.big > prev_target { newest.big.clone() - prev_target.clone() } else { prev_target.clone() - newest.big.clone() };
        assert!(newest_delta > oldest_delta);
    }

    #[test]
    fn weighted_window_uses_real_parent_time_for_last_group() {
        let _setup = scoped_protocol_setup();
        let prevdiff = LOWEST_DIFFICULTY - 1024;
        let dgnr = new_test_dgnr();
        let next_height = dgnr.window_blocks() + 2;
        let mut blocks = HashMap::new();
        for i in 0..=dgnr.window_blocks() {
            let hei = 1 + i;
            blocks.insert(hei, build_intro(hei, 10_000 + i * dgnr.cnf.each_block_target_time, prevdiff));
        }
        let store = CountingStore { reads: AtomicUsize::new(0), root_height: Uint5::MAX, blocks };
        let canonical_prev = 10_000 + dgnr.window_blocks() * dgnr.cnf.each_block_target_time;
        let side_prev = canonical_prev + dgnr.cnf.each_block_target_time * 3;
        let canonical = dgnr.target_weighted_sliding(prevdiff, canonical_prev, next_height, &StoreBlockIntroSource::new(&store));
        let side = dgnr.target_weighted_sliding(prevdiff, side_prev, next_height, &StoreBlockIntroSource::new(&store));
        assert!(side.big > canonical.big);
    }

    #[test]
    fn req_block_intro_bypasses_cache_above_root_height() {
        let _setup = scoped_protocol_setup();
        let dgnr = new_test_dgnr();
        let mut first_blocks = HashMap::new();
        first_blocks.insert(100, build_intro(100, 10_000, LOWEST_DIFFICULTY - 100));
        let mut second_blocks = HashMap::new();
        second_blocks.insert(100, build_intro(100, 20_000, LOWEST_DIFFICULTY - 200));
        let first = TestIntroSource::new(99, first_blocks);
        let second = TestIntroSource::new(99, second_blocks);

        let first_intro = dgnr.req_block_intro(100, &first);
        let second_intro = dgnr.req_block_intro(100, &second);

        assert_ne!(first_intro, second_intro);
        assert_eq!(first_intro.0, 10_000);
        assert_eq!(second_intro.0, 20_000);
    }

    #[test]
    fn asert_start_block_uses_fixed_target_without_parent_cap() {
        let _setup = scoped_protocol_setup();
        let dgnr = new_test_dgnr();
        let upgrade_height = dgnr.asert_upgrade_height();
        let prevdiff = 0xf0ff_ffff;
        let src = TestIntroSource::new(0, HashMap::new());

        let target = dgnr.target_asert(prevdiff, 1_300, upgrade_height, 100_000_000, &src);

        assert_eq!(target.num, ASERT_START_TARGET_NUM);
        assert_eq!(target.hash, u32_to_hash(ASERT_START_TARGET_NUM));
        assert_eq!(target.big, u32_to_biguint(ASERT_START_TARGET_NUM));
    }

    #[test]
    fn legacy_difficulty_stays_active_through_738653() {
        let _setup = scoped_protocol_setup();
        let dgnr = new_test_dgnr();
        assert!(!dgnr.is_asert_height(738653));
        assert!(dgnr.is_asert_height(738654));
    }

    #[test]
    fn mainnet_lwma_window_is_marked_historical() {
        let _setup = scoped_protocol_setup();
        let dgnr = new_test_dgnr();
        assert!(!dgnr.is_historical_lwma_height(738143));
        assert!(dgnr.is_historical_lwma_height(738144));
        assert!(dgnr.is_historical_lwma_height(738653));
        assert!(!dgnr.is_historical_lwma_height(738654));
    }

    #[test]
    fn asert_followup_uses_candidate_block_time() {
        let _setup = scoped_protocol_setup();
        let dgnr = new_test_dgnr();
        let upgrade_height = dgnr.asert_upgrade_height();
        let prevdiff = ASERT_START_TARGET_NUM;

        let mut blocks = HashMap::new();
        blocks.insert(upgrade_height - 1, build_intro(upgrade_height - 1, 1_000, LOWEST_DIFFICULTY - 700_000));
        blocks.insert(upgrade_height, build_intro(upgrade_height, 1_600, ASERT_START_TARGET_NUM));
        let src = TestIntroSource::new(upgrade_height, blocks);

        let fast_target = dgnr.target_asert(prevdiff, 1_600, upgrade_height + 1, 1_900, &src);
        let slow_target = dgnr.target_asert(prevdiff, 1_600, upgrade_height + 1, 100_000_000, &src);

        assert!(slow_target.big > fast_target.big);
    }

    // ===================================================================
    // The claims scripts/hbit-local-chain-rig depends on.
    //
    // That rig builds a private chain the HBIT pool will serve, which needs a
    // network target with >= 34 leading zero bits (server.rs MIN_SHARE_FACTOR
    // 18 + MIN_SHARE_COST_BITS 16). It gets there with config alone, by raising
    // [mint].each_block_target_time. These tests pin the two facts that makes
    // it rest on: that the lever works, and that it cannot reach mainnet.
    // ===================================================================

    /// Leading zero BITS of a target, which is what the pool's admission check
    /// counts (pool_core::share_cost_bits). 34 is the threshold that matters.
    fn target_zero_bits(hash: &[u8; 32]) -> u32 {
        let mut n = 0u32;
        for b in hash {
            if *b == 0 {
                n += 8;
            } else {
                n += b.leading_zeros();
                break;
            }
        }
        n
    }

    /// Difficulty for a chain mined at one block per second since the anchor,
    /// which is the fastest a release node allows (block_build.rs floors the
    /// next timestamp at prev+1, chain/src/verify.rs rejects <= prev and any
    /// timestamp in the future).
    fn one_second_blocks_target(
        adjust_blocks: u64,
        target_time: u64,
        blocks_past_anchor: u64,
    ) -> DifficultyTarget {
        let dgnr = new_test_dgnr_with(adjust_blocks, 4, target_time, 7);
        let anchor = dgnr.asert_upgrade_height();
        let anchor_time = 1_000u64;
        let hei = anchor + blocks_past_anchor;
        let mut blocks = HashMap::new();
        blocks.insert(anchor, build_intro(anchor, anchor_time, ASERT_START_TARGET_NUM));
        let src = TestIntroSource::new(hei, blocks);
        // prevdiff only feeds the 2x easing cap, which bites when the chain is
        // BEHIND schedule; the rig is always ahead, so the anchor value is a
        // faithful stand-in and keeps the vector readable.
        dgnr.target_asert(
            ASERT_START_TARGET_NUM,
            anchor_time + blocks_past_anchor - 1,
            hei,
            anchor_time + blocks_past_anchor,
            &src,
        )
    }

    #[test]
    fn off_mainnet_a_fresh_chain_starts_at_exactly_22_bits() {
        let _setup = scoped_protocol_setup();
        // Why the rig has to exist at all. Off mainnet the ASERT anchor sits at
        // difficulty_adjust_blocks + 2 and its target is the fixed constant
        // ASERT_START_TARGET_NUM, whatever the window config is. 22 is twelve
        // bits short of what the pool will serve, so a chain cannot simply be
        // started and used.
        for (adjust, target_time) in [(288u64, 10u64), (8, 455), (8, 10), (16, 300)] {
            let dgnr = new_test_dgnr_with(adjust, 4, target_time, 7);
            assert_eq!(
                dgnr.asert_upgrade_height(),
                adjust + 2,
                "off-mainnet anchor is difficulty_adjust_blocks + 2"
            );
            let src = TestIntroSource::new(0, HashMap::new());
            let at_anchor = dgnr.target_asert(0, 0, adjust + 2, 0, &src);
            assert_eq!(at_anchor.num, ASERT_START_TARGET_NUM);
            assert_eq!(
                target_zero_bits(&at_anchor.hash),
                22,
                "fresh non-mainnet chain must start at 22 bits (adjust={adjust} tt={target_time})"
            );
        }
    }

    #[test]
    fn each_block_target_time_is_the_lever_that_reaches_a_pool_servable_chain() {
        let _setup = scoped_protocol_setup();
        // 292 blocks past the anchor at one second each. Both columns describe
        // the SAME mining run; only [mint].each_block_target_time differs.
        const BLOCKS: u64 = 292;
        const POOL_MIN_NETWORK_BITS: u32 = 34;

        // The documented testnet target time gets nowhere: each block can buy at
        // most 9 seconds of "ahead of schedule" and one bit costs 10800 of them.
        let documented = one_second_blocks_target(8, 10, BLOCKS);
        assert_eq!(
            target_zero_bits(&documented.hash),
            22,
            "at each_block_target_time=10 the chain has not moved off the anchor"
        );
        assert!(
            target_zero_bits(&documented.hash) < POOL_MIN_NETWORK_BITS,
            "a documented testnet must NOT be pool-servable, or this rig is pointless"
        );

        // The rig's target time, same 292 blocks, same one-second cadence.
        let rig = one_second_blocks_target(8, 455, BLOCKS);
        assert_eq!(
            target_zero_bits(&rig.hash),
            34,
            "at each_block_target_time=455, 292 one-second blocks reach exactly 34 bits"
        );
        assert!(rig.big < documented.big, "the lever must TIGHTEN the target");

        // Monotone in the lever, which is what makes the planner's search sound.
        let mut prev_bits = 0u32;
        for tt in [10u64, 57, 114, 228, 455, 910] {
            let bits = target_zero_bits(&one_second_blocks_target(8, tt, BLOCKS).hash);
            assert!(
                bits >= prev_bits,
                "raising each_block_target_time must never lower the difficulty \
                 (tt={tt} gave {bits}, previous gave {prev_bits})"
            );
            prev_bits = bits;
        }
        assert!(prev_bits > POOL_MIN_NETWORK_BITS);
    }

    #[test]
    fn the_rig_config_cannot_move_mainnet_asert() {
        let _setup = scoped_protocol_setup();
        // The rig sets difficulty_adjust_blocks = 8 and each_block_target_time
        // = 455 in the SAME [mint] section a mainnet node reads. Neither key may
        // move mainnet's ASERT anchor or its start target; if either did, a node
        // that picked up the rig's file would fork off mainnet.
        let stock = new_test_dgnr_with(288, 4, 300, 0);
        let with_rig_values = new_test_dgnr_with(8, 4, 455, 0);
        for dgnr in [&stock, &with_rig_values] {
            assert_eq!(
                dgnr.asert_upgrade_height(),
                ASERT_UPGRADE_HEIGHT,
                "mainnet ASERT must anchor at the consensus constant, not at a config value"
            );
            let src = TestIntroSource::new(0, HashMap::new());
            let at_anchor = dgnr.target_asert(0, 0, ASERT_UPGRADE_HEIGHT, 0, &src);
            assert_eq!(at_anchor.num, ASERT_START_TARGET_NUM);
            assert_eq!(target_zero_bits(&at_anchor.hash), 22);
        }
        // And the rig's own chain_id is not mainnet, which is what routes it to
        // the non-mainnet branch in the first place.
        assert!(!new_test_dgnr_with(8, 4, 455, 7).cnf.is_mainnet());
        assert!(new_test_dgnr_with(8, 4, 455, 0).cnf.is_mainnet());
    }
}
