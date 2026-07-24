
#[derive(Clone, Copy)]
pub struct MintConf {
    pub chain_id: u64, // sub chain id
    pub sync_maxh: u64, // sync block of max height
    pub show_miner_name: bool,
    pub difficulty_group_blocks: u64, // reuses unstable_block value for PoW grouped sampling span
    pub difficulty_adjust_blocks: u64, // height : 288
    pub each_block_target_time: u64, // secs : 300
    pub test_coin: bool
    // pub _test_mul: u64,
}

/// Strict variant of `ini_must_u64` for the consensus critical `[mint]` keys.
/// `ini_must_u64` cannot tell "key absent" from "key present but unparseable" and returns the
/// default for both, so a typo like `chain_id = 5abc` would silently resolve to 0 = mainnet and
/// run the wrong chain's rules. An absent or empty value still takes the default; anything
/// present but not a number is a hard startup failure, matching the other guards below.
fn mint_ini_u64_strict(sec: &HashMap<String, Option<String>>, key: &str, dv: u64) -> u64 {
    let Some(raw) = sec.get(key).and_then(|v| v.as_deref()) else {
        return dv;
    };
    // the ini parser keeps inline comments in the value, so `chain_id = 0 ; mainnet` must not
    // be treated as a typo
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
            "[Config Error] [mint].{} is present but is not a valid number: {:?}",
            key, raw
        ),
    }
}

#[derive(Clone, Copy)]
struct DifficultyWindowConf {
    adjust_blocks: u64,
    group_blocks: u64,
    target_time: u64,
}

impl DifficultyWindowConf {
    fn parse(sec: &HashMap<String, Option<String>>) -> DifficultyWindowConf {
        let adjust_blocks = mint_ini_u64_strict(sec, "difficulty_adjust_blocks", 288);
        let group_blocks = mint_ini_u64_strict(sec, "difficulty_group_blocks", mint_ini_u64_strict(sec, "unstable_block", 4));
        let target_time = mint_ini_u64_strict(sec, "each_block_target_time", 300);
        if group_blocks == 0 {
            panic!("config [mint].difficulty_group_blocks must be greater than 0")
        }
        if adjust_blocks == 0 {
            panic!("config [mint].difficulty_adjust_blocks must be greater than 0")
        }
        if adjust_blocks % group_blocks != 0 {
            panic!(
                "config [mint] difficulty window invalid: difficulty_adjust_blocks={} must be divisible by difficulty_group_blocks={}",
                adjust_blocks,
                group_blocks,
            )
        }
        DifficultyWindowConf { adjust_blocks, group_blocks, target_time }
    }
}

impl MintConf {

    pub fn is_mainnet(&self) -> bool {
        self.chain_id == 0
    }

    pub fn new(ini: &IniObj) -> MintConf {

        let sec = ini_section(ini, "mint");
        let diff = DifficultyWindowConf::parse(&sec);

        let cnf = MintConf {
            chain_id: mint_ini_u64_strict(&sec, "chain_id", 0),
            sync_maxh: mint_ini_u64_strict(&sec, "height_max", 0),
            show_miner_name: ini_must_bool(&sec, "show_miner_name", false),
            difficulty_adjust_blocks: diff.adjust_blocks, // 1 day
            difficulty_group_blocks: diff.group_blocks, // protocol reuses unstable_block numeric value for difficulty grouping
            each_block_target_time: diff.target_time, // 5 mins
            test_coin: ini_must_bool(&sec, "test_coin", false),
            // _test_mul: ini_must_u64(&sec, "_test_mul", 1), // test
        };

        cnf
    }


}

#[cfg(test)]
mod mint_config_tests {
    use super::*;

    #[test]
    #[should_panic(expected = "difficulty window invalid")]
    fn difficulty_window_requires_integral_grouping() {
        let mut ini = IniObj::new();
        let mut mint = HashMap::new();
        mint.insert("difficulty_adjust_blocks".to_string(), Some("288".to_string()));
        mint.insert("difficulty_group_blocks".to_string(), Some("7".to_string()));
        ini.insert("mint".to_string(), mint);
        let _ = MintConf::new(&ini);
    }

    fn mint_ini(pairs: &[(&str, &str)]) -> IniObj {
        let mut ini = IniObj::new();
        let mut mint = HashMap::new();
        for (k, v) in pairs {
            mint.insert(k.to_string(), Some(v.to_string()));
        }
        ini.insert("mint".to_string(), mint);
        ini
    }

    #[test]
    #[should_panic(expected = "chain_id is present but is not a valid number")]
    fn unparseable_chain_id_must_not_silently_become_mainnet() {
        let _ = MintConf::new(&mint_ini(&[("chain_id", "5abc")]));
    }

    #[test]
    fn absent_or_empty_chain_id_keeps_the_mainnet_default() {
        assert!(MintConf::new(&mint_ini(&[])).is_mainnet());
        assert!(MintConf::new(&mint_ini(&[("chain_id", "  ")])).is_mainnet());
    }

    #[test]
    fn chain_id_tolerates_surrounding_space_and_inline_comment() {
        let cnf = MintConf::new(&mint_ini(&[("chain_id", " 7 ; sidechain")]));
        assert_eq!(cnf.chain_id, 7);
        assert!(!cnf.is_mainnet());
    }

    #[test]
    #[should_panic(expected = "each_block_target_time is present but is not a valid number")]
    fn unparseable_block_target_time_must_not_silently_revert_to_default() {
        let _ = MintConf::new(&mint_ini(&[("each_block_target_time", "5min")]));
    }
}
