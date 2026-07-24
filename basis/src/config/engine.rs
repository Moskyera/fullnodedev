




/// Strict variant of `ini_must_u64` for the keys where silently falling back to the default
/// picks the wrong chain or the wrong limit. `ini_must_u64` cannot tell "key absent" from
/// "key present but unparseable" and returns the default for both, so a typo like
/// `chain_id = 5abc` would resolve to 0, which is mainnet, and the node would run mainnet
/// rules while the operator believes it is on a side chain. An absent or empty value still
/// takes the default; anything present but not a number is a hard startup failure.
fn engine_ini_u64_strict(sec: &HashMap<String, Option<String>>, sec_name: &str, key: &str, dv: u64) -> u64 {
    let Some(raw) = sec.get(key).and_then(|v| v.as_deref()) else {
        return dv;
    };
    // the section map can also be built by hand, so drop an inline comment here too:
    // `chain_id = 0 ; mainnet` must not be treated as a typo
    let val = raw
        .split(|c| c == ';' || c == '#')
        .next()
        .unwrap_or("")
        .trim();
    if val.is_empty() {
        return dv;
    }
    match val.parse::<u64>() {
        Ok(n) => n,
        Err(_) => panic!(
            "[Config Error] [{}] {} is present but is not a valid number: {:?}",
            sec_name, key, raw
        ),
    }
}


#[derive(Clone)]
pub struct EngineConf {
    pub max_block_txs: usize,
    pub max_block_size: usize,
    pub max_tx_size: usize,
    pub max_tx_actions: usize,
    pub chain_id: u32, // sub chain id
    pub unstable_block: u64, // The number of blocks that are likely to fall back from the fork
    pub fast_sync: bool,
    pub sync_maxh: u64, // sync max height, limit
    pub data_dir: String,
    pub block_data_dir: PathBuf, // block data
    pub state_data_dir: PathBuf, // chain state
    pub vmlog_data_dir: PathBuf, // vmlog state
    pub show_miner_name: bool,
    // block logs
    pub vm_log_enable: bool,
    pub vm_log_open_height: u64,
    pub vm_log_can_delete: bool,
    pub vm_log_delete_auth_hash: String,
    // dev count
    pub dev_count_switch: usize,
    // data service
    pub diamond_form: bool,
    pub recent_blocks: bool,
    pub average_fee_purity: bool,
    pub lowest_fee_purity: u64, 
    // hac miner
    pub miner_enable: bool,
    pub miner_reward_address: Address,
    pub miner_message: Fixed16,
    // diamond miner
    pub dmer_enable: bool,
    pub dmer_reward_address: Address,
    pub dmer_bid_account: Account,
    pub dmer_bid_min:  Amount,
    pub dmer_bid_max:  Amount,
    pub dmer_bid_step: Amount,
    // tx pool
    pub txpool_maxs: Vec<usize>,
    // VM contract cache (performance-only, consensus-neutral)
    // Unit: MB. `0` disables cache.
    pub contract_cache_size: f64,
}


impl EngineConf {

    pub fn is_open_miner(&self) -> bool {
        self.miner_enable || self.dmer_enable
    }

    pub fn is_mainnet(&self) -> bool {
        self.chain_id == 0
    }

    // Coinbase used by non-block external execution contexts (mempool check/sandbox).
    // If this node has miner enabled and a configured reward address, use that address;
    // otherwise keep zero-address semantics.
    pub fn external_exec_author(&self) -> Address {
        if self.miner_enable && self.miner_reward_address != Address::default() {
            return self.miner_reward_address
        }
        Address::default()
    }
    
    pub fn new(ini: &IniObj) -> EngineConf {
        

        // datadir
        let data_dir = get_mainnet_data_dir(ini);

        // server sec
        let sec_server = &ini_section(ini, "server");

        // a simple hac trs size is 166 bytes
        // fee_purity is now per-byte: 1:244 = 1000000:238, purity = 1000000 / 166 ≈ 6024
        const LOWEST_FEE_PURITY: u64 = 10000_00 / 166; // 6024

        let mut cnf = EngineConf{
            max_block_txs: 1000,
            max_block_size: 1024*1024*1, // 1MB
            max_tx_size: 1024 * 16, // 16kb
            max_tx_actions: 200, // 200
            chain_id: 0,
            unstable_block: 4, // 4 block
            fast_sync: false,
            sync_maxh: 0,
            block_data_dir: join_path(&data_dir, "block"),
            state_data_dir: join_path(&data_dir, "state"),
            vmlog_data_dir: join_path(&data_dir, "vmlog"),
            data_dir: data_dir.to_str().unwrap().to_owned(),
            dev_count_switch: 0,
            show_miner_name: false,
            // logs
            vm_log_enable: false,
            vm_log_open_height: 0,
            vm_log_can_delete: false,
            vm_log_delete_auth_hash: String::new(),
            //
            diamond_form: ini_must_bool(sec_server, "diamond_form", true),
            recent_blocks: ini_must_bool(sec_server, "recent_blocks", false),
            average_fee_purity: ini_must_bool(sec_server, "average_fee_purity", false),
            lowest_fee_purity: LOWEST_FEE_PURITY,
            // HAC miner
            miner_enable: false,
            miner_reward_address: Address::default(),
            miner_message: Fixed16::default(),
            // Diamond miner
            dmer_enable: false,
            dmer_reward_address: Address::default(),
            dmer_bid_account: Account::create_by_password("123456").unwrap(),
            dmer_bid_min:  Amount::small_mei(1),
            dmer_bid_max:  Amount::small_mei(31),
            dmer_bid_step: Amount::small(5, 247),
            // tx pool
            txpool_maxs: Vec::default(),
            // vm cache
            contract_cache_size: 0.0,
        };
        // setup lowest_fee
        if ini_must(sec_server, "lowest_fee", "").trim().len() > 0 {
            let lfepr = ini_must_amount_required(sec_server, "server", "lowest_fee").compress(2, AmtCpr::Grow)
                .unwrap().to_238_u64().unwrap() / 166; //  =6024, simple hac trs size
            cnf.lowest_fee_purity = lfepr;
            println!("[Config] node accepted lowest fee purity {}.", lfepr);
        }

        let sec = &ini_section(ini, "node");
        cnf.fast_sync = ini_must_bool(sec, "fast_sync", false);

        let sec_mint = &ini_section(ini, "mint");
        // chain_id selects the consensus rule set, so it is parsed strictly and its range is
        // checked: `as u32` alone would fold 4294967296 back to 0, which is mainnet.
        let chain_id = engine_ini_u64_strict(sec_mint, "mint", "chain_id", 0);
        if chain_id > u32::MAX as u64 {
            panic!("[Config Error] [mint] chain_id {} is out of range, it must be between 0 and {}.",
                chain_id, u32::MAX)
        }
        cnf.chain_id = chain_id as u32;
        cnf.sync_maxh = engine_ini_u64_strict(sec_mint, "mint", "height_max", 0);
        cnf.dev_count_switch = engine_ini_u64_strict(sec_mint, "mint", "dev_count_switch", 0) as usize;
        cnf.show_miner_name = ini_must_bool(sec_mint, "show_miner_name", false);

        let sec_vm = &ini_section(ini, "vm");
        cnf.vm_log_enable = ini_must_bool(sec_vm, "log_enable", false);
        cnf.vm_log_can_delete = ini_must_bool(sec_vm, "log_can_delete", false);
        cnf.vm_log_open_height = engine_ini_u64_strict(sec_vm, "vm", "log_open_height", 0);
        cnf.vm_log_delete_auth_hash = ini_must(sec_vm, "log_delete_auth_hash", "");

        // HAC miner
        let sec_miner = &ini_section(ini, "miner");
        cnf.miner_enable = ini_must_bool(sec_miner, "enable", false);
        if cnf.miner_enable {
            cnf.miner_reward_address = ini_must_address_required(sec_miner, "miner", "reward");
            if !cnf.miner_reward_address.is_privakey() {
                panic!("miner reward address {} must be PRIVAKEY type but got version {}",
                    cnf.miner_reward_address.to_readable(), cnf.miner_reward_address.version())
            }
            let msg = ini_must_maxlen(sec_miner, "message", "", 16);
            let msgapp = vec![' ' as u8].repeat(16-msg.len());
            let msg: [u8; 16] = vec![msg.as_bytes().to_vec(), msgapp].concat().try_into().unwrap();
            cnf.miner_message = Fixed16::from_readable(&msg).unwrap();
        }

        // Diamond miner
        let sec_dmer = &ini_section(ini, "diamondminer");
        cnf.dmer_enable = ini_must_bool(sec_dmer, "enable", false);
        if cnf.dmer_enable {
            cnf.dmer_reward_address = ini_must_address_required(sec_dmer, "diamondminer", "reward");
            if !cnf.dmer_reward_address.is_privakey() {
                panic!("diamond miner reward address {} must be PRIVAKEY type but got version {}",
                    cnf.dmer_reward_address.to_readable(), cnf.dmer_reward_address.version())
            }
            cnf.dmer_bid_account = ini_must_account_required(sec_dmer, "bid_password");
            cnf.dmer_bid_min =  ini_must_amount_required(sec_dmer, "diamondminer", "bid_min").compress(2, AmtCpr::Grow).unwrap();
            cnf.dmer_bid_max =  ini_must_amount_required(sec_dmer, "diamondminer", "bid_max").compress(2, AmtCpr::Grow).unwrap();
            cnf.dmer_bid_step = ini_must_amount_required(sec_dmer, "diamondminer", "bid_step").compress(2, AmtCpr::Grow).unwrap();
        }

        // tx pool
        // An unset `maxs` must stay empty: the caller overlays this list onto its own
        // defaults, so the old fallback of 100 per unparsed entry silently shrank a mining
        // node's pool from 2000 to 100 slots whenever the key was simply absent.
        let sec_txpool = &ini_section(ini, "txpool");
        let txpool_maxs = ini_must(sec_txpool, "maxs", "").replace(" ", "");
        cnf.txpool_maxs = txpool_maxs.split(",").filter(|a| !a.is_empty()).map(|a|{
            match a.parse::<usize>() {
                Ok(n) => n,
                _ => panic!("[Config Error] [txpool] maxs entry {:?} is not a valid number.", a),
            }
        }).collect();

        // vm contract cache (performance-only), unit: MB
        let sec_vm = &ini_section(ini, "vm");
        cnf.contract_cache_size = ini_must_f64(sec_vm, "contract_cache_size", 0.0);

        // ok
        cnf
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_exec_author_returns_zero_when_miner_disabled() {
        let ini = IniObj::new();
        let cnf = EngineConf::new(&ini);
        assert_eq!(cnf.external_exec_author(), Address::default());
    }

    #[test]
    fn external_exec_author_returns_miner_reward_when_enabled() {
        let reward = "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9".to_owned();
        let mut ini = IniObj::new();
        ini.insert(
            "miner".to_owned(),
            HashMap::from([
                ("enable".to_owned(), Some("true".to_owned())),
                ("reward".to_owned(), Some(reward.clone())),
            ]),
        );
        let cnf = EngineConf::new(&ini);
        assert_eq!(
            cnf.external_exec_author(),
            Address::from_readable(&reward).unwrap()
        );
    }

    #[test]
    fn negation_word_keeps_the_miners_off() {
        for word in ["no", "off", "disabled"] {
            let mut ini = IniObj::new();
            ini.insert(
                "miner".to_owned(),
                HashMap::from([("enable".to_owned(), Some(word.to_owned()))]),
            );
            ini.insert(
                "diamondminer".to_owned(),
                HashMap::from([("enable".to_owned(), Some(word.to_owned()))]),
            );
            let cnf = EngineConf::new(&ini);
            assert_eq!(cnf.miner_enable, false, "enable = {} must stay off", word);
            assert_eq!(cnf.dmer_enable, false, "enable = {} must stay off", word);
        }
    }

    #[test]
    fn diamond_miner_refuses_to_start_without_a_bid_password() {
        let mut ini = IniObj::new();
        ini.insert(
            "diamondminer".to_owned(),
            HashMap::from([
                ("enable".to_owned(), Some("true".to_owned())),
                ("reward".to_owned(), Some("1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9".to_owned())),
            ]),
        );
        let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
        assert!(res.is_err(), "an unset bid_password must not build a spending account");
    }

    #[test]
    fn miner_reward_address_must_be_privakey() {
        let reward = Address::from([Address::SCRIPTMH; Address::SIZE]).to_readable();
        let mut ini = IniObj::new();
        ini.insert(
            "miner".to_owned(),
            HashMap::from([
                ("enable".to_owned(), Some("true".to_owned())),
                ("reward".to_owned(), Some(reward)),
            ]),
        );
        let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
        assert!(res.is_err(), "a non PRIVAKEY miner reward must be rejected at config load");
    }

    fn ini_of(section: &str, pairs: &[(&str, Option<&str>)]) -> IniObj {
        let mut ini = IniObj::new();
        ini.insert(
            section.to_owned(),
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
                .collect(),
        );
        ini
    }

    // the example address the missing reward key used to fall back to
    const OLD_DEFAULT_REWARD: &str = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS";

    #[test]
    fn hac_miner_refuses_to_start_without_a_reward_address() {
        for reward in [None, Some(""), Some("   ")] {
            let mut pairs: Vec<(&str, Option<&str>)> = vec![("enable", Some("true"))];
            if let Some(val) = reward {
                pairs.push(("reward", Some(val)));
            }
            let ini = ini_of("miner", &pairs);
            let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
            assert!(
                res.is_err(),
                "reward {:?} must stop startup, never mine the coinbase to {}",
                reward, OLD_DEFAULT_REWARD
            );
        }
    }

    #[test]
    fn diamond_miner_refuses_to_start_without_a_reward_address() {
        let ini = ini_of("diamondminer", &[
            ("enable", Some("true")),
            ("bid_password", Some("a-real-wallet-password")),
            ("bid_min", Some("1")),
            ("bid_max", Some("2")),
            ("bid_step", Some("1:244")),
        ]);
        let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
        assert!(
            res.is_err(),
            "a missing diamond reward must stop startup, never pay {}",
            OLD_DEFAULT_REWARD
        );
    }

    #[test]
    fn diamond_miner_refuses_to_start_without_the_bid_amounts() {
        let full: [(&str, Option<&str>); 6] = [
            ("enable", Some("true")),
            ("reward", Some("1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9")),
            ("bid_password", Some("a-real-wallet-password")),
            ("bid_min", Some("1")),
            ("bid_max", Some("2")),
            ("bid_step", Some("1:244")),
        ];
        // the complete config builds
        let cnf = EngineConf::new(&ini_of("diamondminer", &full));
        assert_eq!(cnf.dmer_enable, true);
        // dropping any single bid amount must stop startup instead of bidding a placeholder
        for missing in ["bid_min", "bid_max", "bid_step"] {
            let pairs: Vec<(&str, Option<&str>)> =
                full.iter().filter(|(k, _)| *k != missing).cloned().collect();
            let ini = ini_of("diamondminer", &pairs);
            let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
            assert!(res.is_err(), "a missing {} must stop startup", missing);
        }
    }

    #[test]
    fn unparseable_chain_id_must_not_silently_become_mainnet() {
        let ini = ini_of("mint", &[("chain_id", Some("5abc"))]);
        let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
        assert!(res.is_err(), "a typo in chain_id must not run mainnet rules by accident");
    }

    #[test]
    fn chain_id_outside_the_u32_range_is_rejected() {
        // 4294967296 folds back to 0 = mainnet under a bare `as u32`
        let ini = ini_of("mint", &[("chain_id", Some("4294967296"))]);
        let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
        assert!(res.is_err(), "an out of range chain_id must not wrap around to mainnet");
    }

    #[test]
    fn absent_or_empty_chain_id_keeps_the_mainnet_default() {
        assert!(EngineConf::new(&IniObj::new()).is_mainnet());
        assert!(EngineConf::new(&ini_of("mint", &[("chain_id", Some("  "))])).is_mainnet());
        assert!(EngineConf::new(&ini_of("mint", &[("chain_id", None)])).is_mainnet());
    }

    #[test]
    fn chain_id_tolerates_surrounding_space_and_inline_comment() {
        let cnf = EngineConf::new(&ini_of("mint", &[("chain_id", Some(" 7 ; sidechain"))]));
        assert_eq!(cnf.chain_id, 7);
        assert_eq!(cnf.is_mainnet(), false);
    }

    #[test]
    fn unparseable_mint_and_vm_numbers_are_rejected() {
        let cases: [(&str, &str); 3] = [
            ("mint", "height_max"),
            ("mint", "dev_count_switch"),
            ("vm", "log_open_height"),
        ];
        for (section, key) in cases {
            let ini = ini_of(section, &[(key, Some("12x"))]);
            let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
            assert!(res.is_err(), "[{}] {} = 12x must not fall back to the default", section, key);
        }
    }

    #[test]
    fn txpool_maxs_stays_empty_when_unset_so_callers_keep_their_own_limits() {
        assert_eq!(EngineConf::new(&IniObj::new()).txpool_maxs, Vec::<usize>::new());
        assert_eq!(
            EngineConf::new(&ini_of("txpool", &[("maxs", Some("  "))])).txpool_maxs,
            Vec::<usize>::new()
        );
    }

    #[test]
    fn txpool_maxs_reads_a_list_and_rejects_garbage() {
        let cnf = EngineConf::new(&ini_of("txpool", &[("maxs", Some("2000, 100"))]));
        assert_eq!(cnf.txpool_maxs, vec![2000usize, 100usize]);
        // a trailing separator is tolerated
        let cnf = EngineConf::new(&ini_of("txpool", &[("maxs", Some("2000,100,"))]));
        assert_eq!(cnf.txpool_maxs, vec![2000usize, 100usize]);
        let ini = ini_of("txpool", &[("maxs", Some("2000,lots"))]);
        let res = std::panic::catch_unwind(|| EngineConf::new(&ini));
        assert!(res.is_err(), "a malformed txpool limit must be reported, not defaulted");
    }
}
