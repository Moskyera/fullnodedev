

// The ini parser already drops inline comments, but config sections are also built by hand
// (tests, embedded defaults, the panel). Money keys therefore clean their own value: an
// operator writing `reward = 1Abc... ; my wallet` must never end up with a mangled address.
fn ini_money_value(raw: &str) -> &str {
    raw.split(|c| c == ';' || c == '#').next().unwrap_or("").trim()
}

fn ini_money_required<'a>(
    sec: &'a HashMap<String, Option<String>>,
    sec_name: &str,
    key: &str,
    what: &str,
) -> &'a str {
    let val = sec
        .get(key)
        .and_then(|v| v.as_deref())
        .map(ini_money_value)
        .unwrap_or("");
    if val.is_empty() {
        panic!(
            "[Config Error] [{}] {} is required: set '{} = <{}>' in the config file. There is no default, a wrong or missing value sends the money somewhere you do not control.",
            sec_name, key, key, what
        )
    }
    val
}

// A reward address receives every coinbase this node mines, so it has NO default.
// It used to fall back to a hardcoded example address, which silently paid a node's whole
// mining income to a stranger for as long as the operator failed to notice.
pub fn ini_must_address_required(
    sec: &HashMap<String, Option<String>>,
    sec_name: &str,
    key: &str,
) -> Address {
    let adr = ini_money_required(sec, sec_name, key, "your own wallet address");
    let Ok(addr) = Address::from_readable(adr) else {
        panic!("[Config Error] [{}] {} address {} format invalid.", sec_name, key, adr)
    };
    addr
}


// Bid and fee amounts have no default either: a placeholder amount is either far too small
// (the node bids and never wins, burning power for nothing) or far too large (it overpays
// from the operator's own wallet). Both are silent, so an unset key must stop startup.
pub fn ini_must_amount_required(
    sec: &HashMap<String, Option<String>>,
    sec_name: &str,
    key: &str,
) -> Amount {
    let amt = ini_money_required(sec, sec_name, key, "amount, e.g. 1 for 1 HAC");
    let Ok(amount) = Amount::from(amt) else {
        panic!("[Config Error] [{}] {} amount {} format invalid.", sec_name, key, amt)
    };
    amount
}


#[cfg(test)]
mod ini_money_tests {
    use super::*;

    fn sec_of(pairs: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
            .collect()
    }

    #[test]
    fn missing_reward_address_is_a_hard_failure() {
        let sec = sec_of(&[("enable", Some("true"))]);
        let res = std::panic::catch_unwind(|| ini_must_address_required(&sec, "miner", "reward"));
        assert!(res.is_err(), "a missing reward key must never resolve to a built in address");
    }

    #[test]
    fn valueless_reward_address_is_a_hard_failure() {
        // `reward` written on its own line, with no '=' at all
        let sec = sec_of(&[("reward", None)]);
        let res = std::panic::catch_unwind(|| ini_must_address_required(&sec, "miner", "reward"));
        assert!(res.is_err(), "a valueless reward key must never resolve to a built in address");
    }

    #[test]
    fn blank_reward_address_is_a_hard_failure() {
        for blank in ["", "   ", "\t", " ; not set yet", "# todo"] {
            let sec = sec_of(&[("reward", Some(blank))]);
            let res = std::panic::catch_unwind(|| ini_must_address_required(&sec, "miner", "reward"));
            assert!(res.is_err(), "a blank reward value {:?} must be rejected", blank);
        }
    }

    #[test]
    fn reward_address_panic_names_the_section_and_the_key() {
        let sec = sec_of(&[]);
        let res = std::panic::catch_unwind(|| ini_must_address_required(&sec, "diamondminer", "reward"));
        let err = res.unwrap_err();
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()).unwrap_or_default());
        assert!(msg.contains("[diamondminer]"), "message must name the section: {}", msg);
        assert!(msg.contains("reward"), "message must name the key: {}", msg);
    }

    #[test]
    fn no_hardcoded_example_address_can_be_returned() {
        // the old default, a well known example address nobody running this node owns
        let old_default = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS";
        for sec in [sec_of(&[]), sec_of(&[("reward", None)]), sec_of(&[("reward", Some(" "))])] {
            let got = std::panic::catch_unwind(|| {
                ini_must_address_required(&sec, "miner", "reward").to_readable()
            });
            assert!(
                got.is_err(),
                "an unset reward must fail, it must never fall back to {}",
                old_default
            );
        }
    }

    #[test]
    fn valid_reward_address_is_read_with_or_without_an_inline_comment() {
        let want = "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9";
        for raw in [
            "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9",
            "  1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9  ",
            "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9 ; payout wallet",
            "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9 # payout wallet",
        ] {
            let sec = sec_of(&[("reward", Some(raw))]);
            let addr = ini_must_address_required(&sec, "miner", "reward");
            assert_eq!(addr.to_readable(), want, "value {:?} must parse to the address", raw);
        }
    }

    #[test]
    fn invalid_reward_address_is_a_hard_failure() {
        let sec = sec_of(&[("reward", Some("not-an-address"))]);
        let res = std::panic::catch_unwind(|| ini_must_address_required(&sec, "miner", "reward"));
        assert!(res.is_err(), "a malformed address must be rejected");
    }

    #[test]
    fn missing_or_blank_amount_is_a_hard_failure() {
        let cases: [HashMap<String, Option<String>>; 3] =
            [sec_of(&[]), sec_of(&[("bid_min", None)]), sec_of(&[("bid_min", Some("  "))])];
        for sec in cases {
            let res =
                std::panic::catch_unwind(|| ini_must_amount_required(&sec, "diamondminer", "bid_min"));
            assert!(res.is_err(), "an unset bid amount must never fall back to a placeholder");
        }
    }

    #[test]
    fn valid_amount_is_read_with_or_without_an_inline_comment() {
        let want = Amount::from("1:248").unwrap();
        for raw in ["1:248", " 1:248 ", "1:248 ; smallest unit", "1:248 # smallest unit"] {
            let sec = sec_of(&[("bid_min", Some(raw))]);
            let amt = ini_must_amount_required(&sec, "diamondminer", "bid_min");
            assert_eq!(amt, want, "value {:?} must parse to the amount", raw);
        }
    }

    #[test]
    fn invalid_amount_is_a_hard_failure() {
        let sec = sec_of(&[("bid_min", Some("one hac"))]);
        let res = std::panic::catch_unwind(|| ini_must_amount_required(&sec, "diamondminer", "bid_min"));
        assert!(res.is_err(), "a malformed amount must be rejected");
    }
}

