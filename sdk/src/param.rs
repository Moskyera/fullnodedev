
macro_rules! q_acc {
    ( $pass: expr) => ({
        match SysAccount::create_by(&$pass) {
            Err(e) => return errf!("private key invalid: {}", &e),
            Ok(a) => a,
        }
    })
}



macro_rules! q_amt {
    ( $stuff: expr) => ({
        match Amount::from(&$stuff) {
            Err(e) => return errf!("amount invalid: {}", &e),
            Ok(a) => a,
        }
    })
}



macro_rules! q_adr {
    ( $addr: expr) => ({
        match Address::from_readable(&$addr) {
            Err(e) => return errf!("address invalid: {}", &e),
            Ok(a) => a,
        }
    })
}

macro_rules! q_hybrid_acc {
    ($keystore: expr, $pass: expr) => {{
        match hybrid_account_from_keystore(&$keystore, &$pass) {
            Err(e) => return errf!("hybrid keystore unlock failed: {}", e),
            Ok(a) => a,
        }
    }};
}




