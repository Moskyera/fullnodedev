
pub type IniObj = HashMap<String, HashMap<String, Option<String>>>;


pub fn join_path(a: &std::path::Path, b: &str) -> PathBuf {
    let mut a = a.to_path_buf();
    a.push(b);
    a
}

pub fn ini_section<'a>(ini: &'a IniObj, key: &str) -> &'a HashMap<String, Option<String>> {
    static EMPTY: std::sync::OnceLock<HashMap<String, Option<String>>> = std::sync::OnceLock::new();
    ini.get(key).unwrap_or_else(|| EMPTY.get_or_init(HashMap::new))
}

pub fn ini_must(sec: &HashMap<String, Option<String>>, key: &str, def: &str) -> String {
    ini_must_maxlen(sec, key, def, 0)
}

pub fn ini_must_maxlen(sec: &HashMap<String, Option<String>>, key: &str, def: &str, ml: usize) -> String {
    let mut val = sec
        .get(key)
        .and_then(|v| v.as_deref())
        .unwrap_or(def)
        .to_owned();
    if ml > 0 && val.len() > ml {
        let mut cut = ml;
        while cut > 0 && !val.is_char_boundary(cut) {
            cut -= 1;
        }
        val.truncate(cut);
    }
    val
}

pub fn ini_must_u64(sec: &HashMap<String, Option<String>>, key: &str, dv: u64) -> u64 {
    let val = ini_must(sec, key, &dv.to_string());
    match val.parse::<u64>() {
        Ok(n) => n,
        Err(_) => dv,
    }
}

pub fn ini_must_f64(sec: &HashMap<String, Option<String>>, key: &str, dv: f64) -> f64 {
    let val = ini_must(sec, key, &dv.to_string());
    match val.parse::<f64>() {
        Ok(n) => n,
        Err(_) => dv,
    }
}

pub fn ini_must_bool(sec: &HashMap<String, Option<String>>, key: &str, dv: bool) -> bool {
    let mut dfv = "false";
    if dv {
        dfv = "true";
    }
    let val = ini_must(sec, key, dfv);
    // two sided allowlist: an unrecognized word must never resolve to "on",
    // because these flags switch on fund spending features.
    match val.trim().to_ascii_lowercase().as_str() {
        "true"|"yes"|"y"|"on"|"enable"|"enabled"|"1" => true,
        "false"|"no"|"n"|"off"|"disable"|"disabled"|
        "none"|"null"|"0"|"_"|"" => false,
        _ => panic!("[Config Error] key '{}' has invalid boolean value '{}'; use true/yes/on/1 or false/no/off/0.", key, val),
    }
}


pub fn ini_must_account(sec: &HashMap<String, Option<String>>, key: &str) -> Account {
    // a spending account is never derived from a built in default password
    ini_must_account_required(sec, key)
}


pub fn ini_must_account_required(sec: &HashMap<String, Option<String>>, key: &str) -> Account {
    let pass = sec.get(key).and_then(|v| v.as_deref()).unwrap_or("").trim();
    if pass.is_empty() {
        panic!("[Config Error] '{}' is required: set a real wallet password or private key, it funds and signs the payments.", key)
    }
    if pass == "123456" {
        panic!("[Config Error] '{}' must not be the well known password '123456': its private key is public, any funds held there can be stolen by anyone.", key)
    }
    let Ok(acc) = Account::create_by(pass) else {
        panic!("[Config Error] account password for key '{}' is invalid.", key)
    };
    acc
}


#[cfg(test)]
mod ini_parse_tests {
    use super::*;

    fn sec_of(key: &str, val: &str) -> HashMap<String, Option<String>> {
        HashMap::from([(key.to_owned(), Some(val.to_owned()))])
    }

    #[test]
    fn negation_words_are_false() {
        for val in ["no", "No", "NO", "n", "off", "OFF", "disable", "disabled", "false", "FALSE", "0", "none", "null", "_", ""] {
            let sec = sec_of("enable", val);
            assert_eq!(ini_must_bool(&sec, "enable", false), false, "value '{}' must be false", val);
            assert_eq!(ini_must_bool(&sec, "enable", true), false, "value '{}' must be false", val);
        }
    }

    #[test]
    fn affirmation_words_are_true() {
        for val in ["true", "True", "TRUE", "yes", "Y", "on", "On", "enable", "enabled", "1"] {
            let sec = sec_of("enable", val);
            assert_eq!(ini_must_bool(&sec, "enable", false), true, "value '{}' must be true", val);
        }
    }

    #[test]
    fn missing_bool_key_uses_the_default() {
        let sec = HashMap::new();
        assert_eq!(ini_must_bool(&sec, "enable", false), false);
        assert_eq!(ini_must_bool(&sec, "enable", true), true);
    }

    #[test]
    fn unknown_bool_value_is_rejected() {
        let sec = sec_of("enable", "maybe");
        let res = std::panic::catch_unwind(|| ini_must_bool(&sec, "enable", false));
        assert!(res.is_err(), "an unrecognized boolean must not silently enable the feature");
    }

    #[test]
    fn missing_account_key_is_rejected() {
        let sec = HashMap::new();
        let res = std::panic::catch_unwind(|| ini_must_account(&sec, "bid_password"));
        assert!(res.is_err(), "a spending account must never come from a default password");
    }

    #[test]
    fn blank_account_key_is_rejected() {
        let sec = sec_of("bid_password", "   ");
        let res = std::panic::catch_unwind(|| ini_must_account(&sec, "bid_password"));
        assert!(res.is_err(), "a blank password must never build a spending account");
    }

    #[test]
    fn well_known_account_password_is_rejected() {
        let sec = sec_of("bid_password", "123456");
        let res = std::panic::catch_unwind(|| ini_must_account_required(&sec, "bid_password"));
        assert!(res.is_err(), "the publicly known password must never build a spending account");
    }

    #[test]
    fn valid_account_password_builds_the_expected_key() {
        let sec = sec_of("bid_password", "a-real-wallet-password");
        let acc = ini_must_account_required(&sec, "bid_password");
        let want = Account::create_by("a-real-wallet-password").unwrap();
        assert_eq!(acc.readable(), want.readable());
    }
}
