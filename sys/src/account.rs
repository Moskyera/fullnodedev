use base58check::*;
use libsecp256k1::{ SecretKey, PublicKey, Signature, Message };
use zeroize::Zeroizing;
// use rand::{self, RngCore};


const ADDRESS_SIZE: usize = 21;
const PRIVATE_SIZE: usize = 32;
const PUBLIC_SIZE: usize = 33;


#[derive(Clone, PartialEq)]
pub struct Account {
    secret_key: SecretKey,
    public_key: PublicKey,
    address: [u8; ADDRESS_SIZE],
    address_readable: String,
}


impl Drop for Account {
    fn drop(&mut self) {
        self.secret_key.clear();
    }
}



impl Account {
    pub fn check_addr(&self, addr: &[u8]) -> Rerr {
        if self.address == *addr {
            return Ok(())
        }
        // not match
        errf!("Account check failed: expected {} but got {}", 
            self.address_readable, Self::to_base58check(addr))
    }
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
    pub fn address(&self) -> &[u8; ADDRESS_SIZE] {
        &self.address
    }
    pub fn readable(&self) -> &str {
        &self.address_readable
    }
}




impl Account {

    // create
    
    pub fn create_randomly(randomfill: &dyn Fn(&mut [u8]) -> Rerr) -> Ret<Account> {
        loop {
            let mut data = Zeroizing::new([0u8; PRIVATE_SIZE]);
            /*if let Err(e) = getrandom::fill(&mut data) {
                return Err(e.to_string())
            }*/
            randomfill(&mut data[..])?;
            // println!("{:?}", data)
            if data[0] < 255 {
                return Account::create_by_secret_key_value(*data)
            }
        }
    }
    

    pub fn create_by(pass: &str) -> Ret<Account> {
        // is private key
        if pass.len() == PRIVATE_SIZE * 2
            && let Ok(bts) = hex::decode(pass)
        {
            let bts = Zeroizing::new(bts);
            if bts.len() == PRIVATE_SIZE {
                let mut key = Zeroizing::new([0u8; PRIVATE_SIZE]);
                key.copy_from_slice(&bts);
                return Account::create_by_secret_key_value(*key);
            }
        }
        // is passward
        Account::create_by_password(pass)
    }

    pub fn create_by_password(pass: &str) -> Ret<Account> {
        let dt = Zeroizing::new(sha2(pass));
        Account::create_by_secret_key_value(*dt)
    }

    pub fn create_by_secret_key_value(key32: [u8; PRIVATE_SIZE]) -> Ret<Account> {
        let key32 = Zeroizing::new(key32);
        if key32[0] == 255 && key32[1] == 255 && key32[2] == 255 && key32[3] == 255 {
            return Err("secret_key not supported; try a different one".to_string());
        }
        match SecretKey::parse(&key32) {
            Err(e) => Err(e.to_string()),
            Ok(mut sk) => {
                let account = Account::create_by_secret_key(&sk);
                sk.clear();
                Ok(account)
            }
        }
    }

    fn create_by_secret_key(seckey: &SecretKey) -> Account {
        let pubkey = PublicKey::from_secret_key(seckey);
        let address = Account::get_address_by_public_key( pubkey.serialize_compressed() );
        let addrshow = Account::to_readable(&address);
        Account {
            secret_key: *seckey,
            public_key: pubkey,
            address,
            address_readable: addrshow,
        }
    }


    pub fn get_address_by_public_key(pubkey: [u8; PUBLIC_SIZE]) -> [u8; ADDRESS_SIZE] {
        // serialize_compressed
        let dt = sha2(pubkey);
        let dt = ripemd160(dt);
        let version = 0;
        let mut addr = [version; ADDRESS_SIZE];
        addr[1..].copy_from_slice(&dt[..]);
        addr
    }


    pub fn to_readable(addr: &[u8; ADDRESS_SIZE]) -> String {
        let version = addr[0];
        addr[1..].to_base58check(version)
    }

    pub fn to_base58check(s: &[u8]) -> String {
        let v = maybe!(!s.is_empty(), s[0], 0);
        let b = maybe!(!s.is_empty(), &s[1..], &[] as &[u8]);
        b.to_base58check(v)
    }

}



// signature
impl Account {

    // return Signature
    pub fn do_sign(&self, msg: &[u8; 32]) -> [u8; 64] {
        let msg = Message::parse(msg);
        let (s, _r) = libsecp256k1::sign(&msg, &self.secret_key);
        s.serialize()
    }

    pub fn verify_signature(msg: &[u8; 32], publickey: &[u8; 33], signature: &[u8; 64]) -> bool {
        if let Ok(pubkey) = PublicKey::parse_compressed(publickey)
            && let Ok(sigobj) = Signature::parse_standard(signature)
        {
            return libsecp256k1::verify(
                &Message::parse(msg),
                &sigobj,
                &pubkey,
            )
        }
        false
    }

}


#[cfg(test)]
mod account_secret_lifecycle_tests {
    use super::*;

    #[test]
    fn account_has_drop_glue_for_secret_erasure() {
        assert!(std::mem::needs_drop::<Account>());
    }

    #[test]
    fn dropping_one_clone_does_not_invalidate_another() {
        let account = Account::create_by_secret_key_value([1u8; PRIVATE_SIZE]).unwrap();
        let clone = account.clone();
        let message = [7u8; 32];

        drop(account);

        let signature = clone.do_sign(&message);
        let public_key = clone.public_key().serialize_compressed();
        assert!(Account::verify_signature(&message, &public_key, &signature));
    }
}
