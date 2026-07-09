use ml_dsa::{
    ExpandedSigningKey, ExpandedSigningKeyBytes, Generate, KeyExport, KeyInit, Keypair,
    MlDsa65, Signature as MlDsaSignature, SignatureEncoding, SigningKey, VerifyingKey,
};

const SECP_PK_SIZE: usize = 33;
const SECP_SIG_SIZE: usize = 64;

/// ML-DSA-65 domain-separation context for Hacash Type4 transaction signatures.
pub const MLDSA_TX_DOMAIN_CTX: &[u8] = b"HACASH_TX4";

pub type MldsaPublicKey = VerifyingKey<MlDsa65>;
pub type MldsaSecretKey = ExpandedSigningKey<MlDsa65>;

const MLDSA65_PK_BYTES: usize = 1952;
const MLDSA65_SIG_BYTES: usize = 3309;
const MLDSA65_SK_BYTES: usize = 4032;

#[derive(Clone, PartialEq)]
pub enum HybridAccountKind {
    PqcOnly,
    Hybrid,
}

#[derive(Clone, PartialEq)]
pub struct HybridAccount {
    kind: HybridAccountKind,
    secp: Option<Account>,
    mldsa_sk: MldsaSecretKey,
    mldsa_pk: MldsaPublicKey,
    mldsa_pk_bytes: Vec<u8>,
    address: [u8; ADDRESS_SIZE],
    address_readable: String,
}

impl HybridAccount {
    pub fn kind(&self) -> &HybridAccountKind {
        &self.kind
    }

    pub fn is_pqc_only(&self) -> bool {
        matches!(self.kind, HybridAccountKind::PqcOnly)
    }

    pub fn is_hybrid(&self) -> bool {
        matches!(self.kind, HybridAccountKind::Hybrid)
    }

    pub fn secp_account(&self) -> Option<&Account> {
        self.secp.as_ref()
    }

    pub fn mldsa_public_key(&self) -> &MldsaPublicKey {
        &self.mldsa_pk
    }

    pub fn mldsa_public_key_bytes(&self) -> &[u8] {
        &self.mldsa_pk_bytes
    }

    pub fn address(&self) -> &[u8; ADDRESS_SIZE] {
        &self.address
    }

    pub fn readable(&self) -> &str {
        &self.address_readable
    }

    pub fn check_addr(&self, addr: &[u8]) -> Rerr {
        if self.address == *addr {
            return Ok(());
        }
        errf!(
            "HybridAccount check failed: expected {} but got {}",
            self.address_readable,
            Account::to_base58check(addr)
        )
    }

    pub fn create_pqc_randomly(randomfill: &dyn Fn(&mut [u8]) -> Rerr) -> Ret<HybridAccount> {
        let _ = randomfill;
        let signing = SigningKey::<MlDsa65>::generate();
        let mldsa_sk = ExpandedSigningKey::from_seed(signing.as_seed());
        let mldsa_pk = signing.verifying_key().clone();
        let mldsa_pk_bytes = pk_to_vec(&mldsa_pk);
        let address = get_pqckey_address(&mldsa_pk_bytes);
        let addrshow = Account::to_readable(&address);
        Ok(HybridAccount {
            kind: HybridAccountKind::PqcOnly,
            secp: None,
            mldsa_sk,
            mldsa_pk,
            mldsa_pk_bytes,
            address,
            address_readable: addrshow,
        })
    }

    pub fn create_hybrid_randomly(randomfill: &dyn Fn(&mut [u8]) -> Rerr) -> Ret<HybridAccount> {
        let secp = Account::create_randomly(randomfill)?;
        let signing = SigningKey::<MlDsa65>::generate();
        let mldsa_sk = ExpandedSigningKey::from_seed(signing.as_seed());
        let mldsa_pk = signing.verifying_key().clone();
        Self::from_secp_and_mldsa(secp, mldsa_sk, mldsa_pk)
    }

    pub fn create_hybrid_from_secp(secp: Account) -> Ret<HybridAccount> {
        let signing = SigningKey::<MlDsa65>::generate();
        let mldsa_sk = ExpandedSigningKey::from_seed(signing.as_seed());
        let mldsa_pk = signing.verifying_key().clone();
        Self::from_secp_and_mldsa(secp, mldsa_sk, mldsa_pk)
    }

    fn from_secp_and_mldsa(
        secp: Account,
        mldsa_sk: MldsaSecretKey,
        mldsa_pk: MldsaPublicKey,
    ) -> Ret<HybridAccount> {
        let secp_pk = secp.public_key().serialize_compressed();
        let mldsa_pk_bytes = pk_to_vec(&mldsa_pk);
        let address = get_hybrid_address(&secp_pk, &mldsa_pk_bytes);
        let addrshow = Account::to_readable(&address);
        Ok(HybridAccount {
            kind: HybridAccountKind::Hybrid,
            secp: Some(secp),
            mldsa_sk,
            mldsa_pk,
            mldsa_pk_bytes,
            address,
            address_readable: addrshow,
        })
    }

    pub fn get_pqckey_address(mldsa_pk: &[u8]) -> [u8; ADDRESS_SIZE] {
        get_pqckey_address(mldsa_pk)
    }

    pub fn get_hybrid_address(secp_pk: &[u8; SECP_PK_SIZE], mldsa_pk: &[u8]) -> [u8; ADDRESS_SIZE] {
        get_hybrid_address(secp_pk, mldsa_pk)
    }

    pub fn sign_hash(&self, hash: &[u8; 32]) -> Ret<Vec<u8>> {
        let sig = self
            .mldsa_sk
            .sign_deterministic(hash, MLDSA_TX_DOMAIN_CTX)
            .map_err(|e| e.to_string())?;
        let sig_bytes = sig.to_bytes();
        match self.kind {
            HybridAccountKind::PqcOnly => {
                let mut body = Vec::with_capacity(public_key_bytes() + signature_bytes());
                body.extend_from_slice(&self.mldsa_pk_bytes);
                body.extend_from_slice(sig_bytes.as_ref());
                Ok(body)
            }
            HybridAccountKind::Hybrid => {
                let secp = self
                    .secp
                    .as_ref()
                    .ok_or_else(|| "hybrid account missing secp key".to_string())?;
                let secp_pk = secp.public_key().serialize_compressed();
                let secp_sig = secp.do_sign(hash);
                let mut body = Vec::with_capacity(
                    SECP_PK_SIZE + SECP_SIG_SIZE + public_key_bytes() + signature_bytes(),
                );
                body.extend_from_slice(&secp_pk);
                body.extend_from_slice(&secp_sig);
                body.extend_from_slice(&self.mldsa_pk_bytes);
                body.extend_from_slice(sig_bytes.as_ref());
                Ok(body)
            }
        }
    }

    pub fn sign_alg_id(&self) -> u8 {
        match self.kind {
            HybridAccountKind::PqcOnly => 1,
            HybridAccountKind::Hybrid => 3,
        }
    }

    pub fn export_key_blob(&self) -> Ret<HybridKeyBlob> {
        let kind = self.sign_alg_id();
        let mldsa_sk = expanded_sk_to_vec(&self.mldsa_sk);
        let secp_sk = match &self.secp {
            Some(acc) => Some(acc.secret_key().serialize()),
            None => None,
        };
        Ok(HybridKeyBlob {
            kind,
            mldsa_sk,
            secp_sk,
            mldsa_pk: self.mldsa_pk_bytes.clone(),
        })
    }

    pub fn from_key_blob(blob: &HybridKeyBlob) -> Ret<HybridAccount> {
        if blob.mldsa_sk.len() != secret_key_bytes() {
            return errf!(
                "mldsa secret key size {} expected {}",
                blob.mldsa_sk.len(),
                secret_key_bytes()
            );
        }
        if blob.mldsa_pk.len() != public_key_bytes() {
            return errf!(
                "mldsa public key size {} expected {}",
                blob.mldsa_pk.len(),
                public_key_bytes()
            );
        }
        let mldsa_sk = expanded_sk_from_bytes(&blob.mldsa_sk)?;
        let mldsa_pk = VerifyingKey::<MlDsa65>::new_from_slice(&blob.mldsa_pk)
            .map_err(|e| e.to_string())?;
        let mldsa_pk_bytes = pk_to_vec(&mldsa_pk);
        if pk_to_vec(&mldsa_sk.verifying_key()) != mldsa_pk_bytes {
            return errf!("hybrid key blob mldsa pk/sk mismatch");
        }
        match blob.kind {
            1 => {
                let address = get_pqckey_address(&mldsa_pk_bytes);
                Ok(HybridAccount {
                    kind: HybridAccountKind::PqcOnly,
                    secp: None,
                    mldsa_sk,
                    mldsa_pk,
                    mldsa_pk_bytes,
                    address,
                    address_readable: Account::to_readable(&address),
                })
            }
            3 => {
                let secp_sk = blob
                    .secp_sk
                    .ok_or_else(|| "hybrid key blob missing secp secret".to_string())?;
                let secp = Account::create_by_secret_key_value(secp_sk)?;
                Self::from_secp_and_mldsa(secp, mldsa_sk, mldsa_pk)
            }
            _ => errf!("hybrid key blob kind {} not supported", blob.kind),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HybridKeyBlob {
    pub kind: u8,
    pub mldsa_sk: Vec<u8>,
    pub secp_sk: Option<[u8; 32]>,
    pub mldsa_pk: Vec<u8>,
}

pub fn mldsa65_secret_key_size() -> usize {
    secret_key_bytes()
}

fn pk_to_vec(pk: &MldsaPublicKey) -> Vec<u8> {
    pk.to_bytes().into_iter().collect()
}

fn secret_key_bytes() -> usize {
    MLDSA65_SK_BYTES
}

fn public_key_bytes() -> usize {
    MLDSA65_PK_BYTES
}

fn signature_bytes() -> usize {
    MLDSA65_SIG_BYTES
}

fn expanded_sk_to_vec(sk: &MldsaSecretKey) -> Vec<u8> {
    #[allow(deprecated)]
    {
        sk.to_expanded().into_iter().collect()
    }
}

fn expanded_sk_from_bytes(bytes: &[u8]) -> Ret<MldsaSecretKey> {
    if bytes.len() != secret_key_bytes() {
        return errf!(
            "mldsa expanded secret key size {} expected {}",
            bytes.len(),
            secret_key_bytes()
        );
    }
    let mut enc: ExpandedSigningKeyBytes<MlDsa65> = Default::default();
    for (dst, src) in enc.iter_mut().zip(bytes.iter()) {
        *dst = *src;
    }
    #[allow(deprecated)]
    {
        Ok(ExpandedSigningKey::from_expanded(&enc))
    }
}

pub fn get_pqckey_address(mldsa_pk: &[u8]) -> [u8; ADDRESS_SIZE] {
    let dt = sha2(mldsa_pk);
    let dt = ripemd160(dt);
    let version = 6u8;
    let mut addr = [version; ADDRESS_SIZE];
    addr[1..].copy_from_slice(&dt[..]);
    addr
}

pub fn get_hybrid_address(secp_pk: &[u8; SECP_PK_SIZE], mldsa_pk: &[u8]) -> [u8; ADDRESS_SIZE] {
    let mut stuff = Vec::with_capacity(1 + SECP_PK_SIZE + 1 + mldsa_pk.len());
    stuff.push(0x01);
    stuff.extend_from_slice(secp_pk);
    stuff.push(0x02);
    stuff.extend_from_slice(mldsa_pk);
    let dt = sha2(stuff);
    let dt = ripemd160(dt);
    let version = 7u8;
    let mut addr = [version; ADDRESS_SIZE];
    addr[1..].copy_from_slice(&dt[..]);
    addr
}

pub fn legacy_secp_sign_body(acc: &Account, hash: &[u8; 32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(SECP_PK_SIZE + SECP_SIG_SIZE);
    body.extend_from_slice(&acc.public_key().serialize_compressed());
    body.extend_from_slice(&acc.do_sign(hash));
    body
}

pub type MldsaVerifyObserver = fn(u64);

static MLDSA_VERIFY_OBSERVER: std::sync::OnceLock<MldsaVerifyObserver> = std::sync::OnceLock::new();

pub fn set_mldsa_verify_observer(observer: MldsaVerifyObserver) {
    let _ = MLDSA_VERIFY_OBSERVER.set(observer);
}

#[inline]
fn observe_mldsa_verify_us(us: u64) {
    if let Some(obs) = MLDSA_VERIFY_OBSERVER.get() {
        obs(us);
    }
}

pub fn verify_mldsa65_detached(msg: &[u8; 32], pk_bytes: &[u8], sig_bytes: &[u8]) -> bool {
    let start = std::time::Instant::now();
    let Ok(pk) = VerifyingKey::<MlDsa65>::new_from_slice(pk_bytes) else {
        return false;
    };
    let Ok(sig) = MlDsaSignature::<MlDsa65>::try_from(sig_bytes) else {
        return false;
    };
    let ok = pk.verify_with_context(msg, MLDSA_TX_DOMAIN_CTX, &sig);
    let us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
    observe_mldsa_verify_us(us);
    ok
}

pub fn address_from_hybrid_sign_body(alg: u8, body: &[u8]) -> Ret<[u8; ADDRESS_SIZE]> {
    match alg {
        0 => {
            if body.len() != SECP_PK_SIZE + SECP_SIG_SIZE {
                return errf!("legacy secp hybrid sign body length invalid");
            }
            let mut pk = [0u8; SECP_PK_SIZE];
            pk.copy_from_slice(&body[..SECP_PK_SIZE]);
            Ok(Account::get_address_by_public_key(pk))
        }
        1 => {
            if body.len() != public_key_bytes() + signature_bytes() {
                return errf!("mldsa65 hybrid sign body length invalid");
            }
            let pk = &body[..public_key_bytes()];
            Ok(get_pqckey_address(pk))
        }
        3 => {
            let expect = SECP_PK_SIZE + SECP_SIG_SIZE + public_key_bytes() + signature_bytes();
            if body.len() != expect {
                return errf!("hybrid sign body length invalid");
            }
            let mut secp_pk = [0u8; SECP_PK_SIZE];
            secp_pk.copy_from_slice(&body[..SECP_PK_SIZE]);
            let mldsa_off = SECP_PK_SIZE + SECP_SIG_SIZE;
            let mldsa_pk = &body[mldsa_off..mldsa_off + public_key_bytes()];
            Ok(get_hybrid_address(&secp_pk, mldsa_pk))
        }
        _ => errf!("hybrid sign alg {} not supported", alg),
    }
}

#[cfg(test)]
mod pqc_tests {
    use super::*;

    #[test]
    fn mldsa65_sizes_match_wire_constants() {
        assert_eq!(public_key_bytes(), 1952);
        assert_eq!(signature_bytes(), 3309);
        assert_eq!(secret_key_bytes(), 4032);
    }

    #[test]
    fn pqc_roundtrip_sign_verify() {
        let acc = HybridAccount::create_pqc_randomly(&|buf| {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (i as u8).wrapping_add(3);
            }
            Ok(())
        })
        .unwrap();
        let msg = sha2(b"test-message");
        let body = acc.sign_hash(&msg).unwrap();
        let pk = &body[..public_key_bytes()];
        let sig = &body[public_key_bytes()..];
        assert!(verify_mldsa65_detached(&msg, pk, sig));
        assert_eq!(acc.address(), &get_pqckey_address(pk));
    }

    #[test]
    fn hybrid_roundtrip_sign_verify() {
        let acc = HybridAccount::create_hybrid_randomly(&|buf| {
            for b in buf.iter_mut() {
                *b = 7;
            }
            Ok(())
        })
        .unwrap();
        let msg = sha2(b"hybrid-test");
        let body = acc.sign_hash(&msg).unwrap();
        let secp_pk: [u8; SECP_PK_SIZE] = body[..SECP_PK_SIZE].try_into().unwrap();
        let secp_sig = &body[SECP_PK_SIZE..SECP_PK_SIZE + SECP_SIG_SIZE];
        let mldsa_off = SECP_PK_SIZE + SECP_SIG_SIZE;
        let mldsa_pk = &body[mldsa_off..mldsa_off + public_key_bytes()];
        let mldsa_sig = &body[mldsa_off + public_key_bytes()..];
        assert!(Account::verify_signature(&msg, &secp_pk, secp_sig.try_into().unwrap()));
        assert!(verify_mldsa65_detached(&msg, mldsa_pk, mldsa_sig));
        assert_eq!(
            acc.address(),
            &get_hybrid_address(&secp_pk, mldsa_pk)
        );
    }

    #[test]
    fn key_blob_roundtrip() {
        let acc = HybridAccount::create_pqc_randomly(&|_| Ok(())).unwrap();
        let blob = acc.export_key_blob().unwrap();
        let acc2 = HybridAccount::from_key_blob(&blob).unwrap();
        assert_eq!(acc.address(), acc2.address());
        let msg = sha2(b"blob-test");
        let body1 = acc.sign_hash(&msg).unwrap();
        let body2 = acc2.sign_hash(&msg).unwrap();
        assert_eq!(body1, body2);
    }
}