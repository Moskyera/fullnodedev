use base58check::*;
pub const ADDR_OR_PTR_DIV_NUM: u8 = 20;

#[repr(transparent)]
#[derive(Debug, Hash, Copy, Clone, PartialEq, Eq)]
pub struct Address(Fixed21);

pub type Addrptr = Uint1;

impl Address {
    pub const SIZE: usize = 21;
}

impl Default for Address {
    fn default() -> Self {
        Address(Fixed21::default())
    }
}

impl Deref for Address {
    type Target = Fixed21;
    fn deref(&self) -> &Fixed21 {
        &self.0
    }
}

impl From<[u8; 21]> for Address {
    fn from(v: [u8; 21]) -> Self {
        Address(Fixed21::from(v))
    }
}

impl From<Fixed21> for Address {
    fn from(v: Fixed21) -> Self {
        Address(v)
    }
}

impl Index<usize> for Address {
    type Output = u8;
    fn index(&self, idx: usize) -> &u8 {
        &self.0[idx]
    }
}

impl Parse for Address {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        self.0.parse(buf)
    }
}

impl Serialize for Address {
    fn serialize_to(&self, out: &mut Vec<u8>) {
        self.0.serialize_to(out);
    }
    fn size(&self) -> usize {
        self.0.size()
    }
}

impl_field_only_new!{Address}

impl ToJSON for Address {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        let version = self.0[0];
        let data = &self.0.as_ref()[1..];
        format!("\"{}\"", data.to_base58check(version))
    }
}

impl FromJSON for Address {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let raw = json_expect_quoted_decoded(json)?;
        let trimmed = raw.trim();
        // Try bare base58check first (Address-specific, no prefix)
        if let Ok(addr) = Self::from_readable(trimmed) {
            *self = addr;
            return Ok(());
        }
        if trimmed.len() >= 4 && (trimmed.starts_with("b58:") || trimmed.starts_with("B58:")) {
            return errf!("Address JSON no longer accepts b58: prefix");
        }
        // Fall back to generic binary (0x, b64:, plain)
        let data = json_decode_binary(json)?;
        if data.len() != Self::SIZE {
            return errf!("Address size mismatch: expected {} but got {}", Self::SIZE, data.len());
        }
        let addr = Address(Fixed21::from(data.try_into().unwrap()));
        addr.check_version()?;
        *self = addr;
        Ok(())
    }
}

impl AsRef<[u8]> for Address {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl Address {
    pub fn must_vec(v: Vec<u8>) -> Self {
        Self::must(&v)
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{}", self.to_readable())
    }
}

// System addresses: 21-byte big-endian value < u32::MAX.
// These addresses are computationally unknowable (nobody holds the private key).
pub static ADDRESS_ZERO: Address = Address(Fixed21::from([0u8; 21])); // value = 0, blackhole
pub static ADDRESS_ONEX: Address = Address(Fixed21::from([
    0u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 1u8,
])); // value = 1, TEX settlement
pub static ADDRESS_TWOX: Address = Address(Fixed21::from([
    0u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0, 2u8,
])); // value = 2, reserved


macro_rules! address_version_define {
    ( $($key:ident : $name:ident , $num:expr)+ ) => {

impl Address {
    
    $(
    pub const $key: u8 = $num; // leading symbol: 1
    )+

    pub fn version(&self) -> u8 {
        self[0]
    }

    pub fn check_version(&self) -> Rerr {
        let v = self.version();
        match v {
            $( $num )|+ => Ok(()),
            _ => errf!("address version {} not supported", v)
        }
    }

    $(
    concat_idents::concat_idents!{ is_version = is_, $name {
    pub fn is_version(&self) -> bool {
        self.version() == Self::$key
    }
    }}
    concat_idents::concat_idents!{ must_version = must_, $name {
    pub fn must_version(&self) -> Rerr {
        maybe!(self.version() == Self::$key,
            Ok(()),
            errf!("address {} is not {} type", self.to_readable(), stringify!($key))
        )
    }
    }}
    concat_idents::concat_idents!{ creat_by_version = create_, $name {
    pub fn creat_by_version(hx: [u8; 20]) -> Self {
        let mut data = [0u8; 21];
        data[0] = Self::$key;
        data[1..].copy_from_slice(&hx);
        Self::from(data)
    }
    }}
    )+

    pub fn from_bytes(stuff: &[u8]) -> Ret<Self> {
        if stuff.len() != Self::SIZE {
            return errf!("address size mismatch")
        }
        let addr = Self::from(<Vec<u8> as TryInto<[u8; 21]>>::try_into(stuff.to_vec()).unwrap());
        addr.check_version()?;
        Ok(addr)
    }

}   

    }
}

/*
    version
    https://en.bitcoin.it/wiki/List_of_address_prefixes
    scriptmh: Pay-to-Script-Merkl-Hash
*/
address_version_define!{
    PRIVAKEY : privakey, 0 // leading symbol: 1
    CONTRACT : contract, 1 // leading symbol: Q-Z, a-k, m-o
    SCRIPTMH : scriptmh, 5 // leading symbol: 3
    PQCKEY   : pqckey, 6 // post-quantum ML-DSA-65 key hash
    HYBRID   : hybrid, 7 // secp256k1 + ML-DSA-65 hybrid key hash
}


/*
    readable
*/
impl Address {
    
    pub const UNKNOWN: Self = Address(Fixed21::DEFAULT);

    pub const fn zero() -> Self {
        Self::UNKNOWN
    }

    /// Returns true if this address is a system address whose 21-byte
    /// big-endian value is less than `u32::MAX`.  These addresses are
    /// computationally unknowable — nobody holds the corresponding
    /// private key because RIPEMD160 preimage collision probability
    /// for any value below 2³² is ≈ 2⁻¹²⁸.
    ///
    /// System addresses include:
    ///   - ADDRESS_ZERO  (value 0)  — blackhole
    ///   - ADDRESS_ONEX  (value 1)  — TEX settlement escrow
    ///   - ADDRESS_TWOX  (value 2)  — reserved
    /// User-controlled signing address: legacy privakey, PQC-only, or hybrid.
    pub fn is_user_signing_address(&self) -> bool {
        self.is_privakey() || self.is_pqckey() || self.is_hybrid()
    }

    pub fn is_privakey_unknown(&self) -> bool {
        let b = self.as_ref(); // 21 bytes
        // Value < u32::MAX  →  top 17 bytes must be 0.
        b[..17].iter().all(|&x| x == 0)
            && u32::from_be_bytes([b[17], b[18], b[19], b[20]]) < u32::MAX
    }

    pub fn to_readable(&self) -> String {
        Account::to_readable(&*self)
    }

    pub fn prefix(&self, n: usize) -> String {
        let mut s = self.to_readable();
        let _ = s.split_off(n);
        s
    }
    
    pub fn from_readable(addr: &str) -> Ret<Self> {
        let Ok((version, body)) = addr.from_base58check() else {
            return errf!("base58check failed")
        };
        if body.len() != Self::SIZE - 1 {
            return Err("address length invalid".to_string())
        }
        let mut data = [0u8; Self::SIZE];
        data[0] = version;
        data[1..].copy_from_slice(&body);
        let address = Self::from(data);
        address.check_version()?;
        Ok(address)
    }
    
}





/*
*
*/
combi_list!{ AddressW1, Uint1, Address }

impl ParsePrefix for AddressW1 {
    fn create_with_prefix(prefix: &[u8], rest: &[u8]) -> Ret<(Self, usize)> {
        if prefix.is_empty() {
            return errf!("AddressW1 prefix is empty");
        }
        let count_byte = prefix[0];
        if count_byte == 0 {
            return errf!("AddressW1 count cannot be zero");
        }
        let count = count_byte as usize;
        let mut v = AddressW1::new();
        v.count = Uint1::from(count_byte);
        let mut seek = 0;
        for _ in 0..count {
            let mut addr = Address::new();
            seek += addr.parse(&rest[seek..])?;
            v.lists.push(addr);
        }
        Ok((v, 1 + seek))
    }
}

/*
*
*/
combi_revenum_old_no_json!{ AddrOrList, Address, AddressW1, ADDR_OR_PTR_DIV_NUM }

impl AddrOrList {

    #[allow(dead_code)]
    pub fn to_list(&self) -> Vec<Address> {
        match self {
            Self::Val1(v) => vec![*v],
            Self::Val2(v) => v.as_list().clone(),
        }
    }

    pub fn from_list(list: Vec<Address>) -> Ret<Self> {
        if list.is_empty() {
            return errf!("AddrOrList list cannot be empty");
        }
        let max_count = u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize;
        if list.len() > max_count {
            return errf!("AddrOrList list length {} exceeds max {}", list.len(), max_count);
        }
        let mut v = AddressW1::new();
        v.append(list)?;
        Ok(Self::Val2(v))
    }

    pub fn from_addr(v: Address) -> Self {
        Self::Val1(v)
    } 

}

impl ToJSON for AddrOrList {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        match self {
            Self::Val1(v) => v.to_json_fmt(fmt),
            Self::Val2(v) => v.to_json_fmt(fmt),
        }
    }
}

impl FromJSON for AddrOrList {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let s = json.trim();
        if s.starts_with('"') && s.ends_with('"') {
            let mut v = Address::new();
            v.from_json(s)?;
            *self = Self::Val1(v);
            return Ok(());
        }
        if s.starts_with('[') && s.ends_with(']') {
            let mut v = AddressW1::new();
            v.from_json(s)?;
            if v.length() == 0 {
                return errf!("invalid AddrOrList JSON: list length cannot be zero");
            }
            let max_count = u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize;
            if v.length() > max_count {
                return errf!(
                    "invalid AddrOrList JSON: list length {} exceeds max {}",
                    v.length(),
                    max_count
                );
            }
            *self = Self::Val2(v);
            return Ok(());
        }
        errf!("invalid AddrOrList JSON: expected quoted address or address array")
    }
}


/*
*
*/
combi_revenum_no_json!{ AddrOrPtr, Address, Addrptr, ADDR_OR_PTR_DIV_NUM }

impl Copy for AddrOrPtr {} 

impl From<Address> for AddrOrPtr {
    fn from(addr: Address) -> Self {
        Self::Val1(addr)
    }
}


impl AddrOrPtr {

    /**
    * real address by ptr in list 
    */
    #[allow(dead_code)]
    pub fn real(&self, addrs: &Vec<Address>) -> Ret<Address> {
        match self {
            Self::Val1(v) => Ok(*v),
            Self::Val2(v) => {
                let ix = v.uint();
                if ix < ADDR_OR_PTR_DIV_NUM {
                    return errf!("addr ptr index invalid")
                }
                let i = (ix - ADDR_OR_PTR_DIV_NUM) as usize;
                maybe!(i < addrs.len(), Ok(addrs[i].clone()), errf!("addr ptr index overflow"))
            },
        }
    }

    pub fn from_addr(v: Address) -> Self {
        Self::Val1(v)
    } 

    pub fn from_ptr(i: u8) -> Self {
        let raw = i
            .checked_add(ADDR_OR_PTR_DIV_NUM)
            .expect("AddrOrPtr::from_ptr index overflow");
        Self::Val2(Addrptr::from(raw))
    } 

    pub fn to_readable(&self) -> String {
        self.readable()
    }

    pub fn readable(&self) -> String {
        match self {
            Self::Val1(v) => v.to_readable(),
            Self::Val2(v) => match v.uint().checked_sub(ADDR_OR_PTR_DIV_NUM) {
                Some(ix) => format!("<address pointer {}>", ix),
                None => format!("<invalid address pointer raw {}>", v.uint()),
            },
        }
    }

}

impl ToJSON for AddrOrPtr {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        match self {
            Self::Val1(v) => v.to_json_fmt(fmt),
            Self::Val2(v) => match v.uint().checked_sub(ADDR_OR_PTR_DIV_NUM) {
                Some(ix) => ix.to_string(),
                None => v.uint().to_string(),
            },
        }
    }
}

impl FromJSON for AddrOrPtr {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let s = json.trim();
        if s.starts_with('"') && s.ends_with('"') {
            let mut v = Address::new();
            v.from_json(s)?;
            *self = Self::Val1(v);
            return Ok(());
        }
        let ix = json_expect_unquoted(s)?
            .parse::<u16>()
            .map_err(|e: std::num::ParseIntError| e.to_string())?;
        let max_ix = u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize;
        if ix as usize > max_ix {
            return errf!("addr ptr index {} exceeds max {}", ix, max_ix);
        }
        *self = Self::from_ptr(ix as u8);
        Ok(())
    }
}





/************************ test ************************/





#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn test1() {

        let adr0 = "1111111111111111111114oLvT2";
        let adr1 = Address::UNKNOWN;
        let adr2 = Address::from_readable(adr0).unwrap();
        
        assert_eq!(adr1.to_readable(), adr2.to_readable());

        let adra = "14Xrfwd7XWmvzjpinTxxc9PwdHf37Myryy";
        let privkey = "594ac10e33501c06e3fae0f9133f4701c204a1f9de62a97cc33754a051019db7";

        let adrb = Account::create_by(privkey).unwrap();
        assert_eq!(adra, adrb.readable());

        let adrc = "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9";
        assert_eq!(adrc, Account::create_by("123456").unwrap().readable());

    }

    #[test]
    fn test_addr_or_ptr_json_reject_overflow_index() {
        let mut ptr = AddrOrPtr::default();
        let too_large = u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize + 1;
        let err = ptr.from_json(&too_large.to_string()).unwrap_err();
        assert!(err.contains("exceeds max"), "{}", err);
    }

    #[test]
    fn test_addr_or_ptr_readable_invalid_raw_no_underflow() {
        let ptr = AddrOrPtr::Val2(Addrptr::from(1));
        let s = ptr.to_readable();
        assert!(s.contains("invalid address pointer raw 1"));
    }

    #[test]
    #[should_panic(expected = "AddrOrPtr::from_ptr index overflow")]
    fn test_addr_or_ptr_from_ptr_overflow_panics() {
        let _ = AddrOrPtr::from_ptr(u8::MAX - ADDR_OR_PTR_DIV_NUM + 1);
    }

    #[test]
    fn test_addr_or_ptr_json_roundtrip_as_index() {
        let ptr = AddrOrPtr::from_ptr(7);
        let json = ptr.to_json();
        assert_eq!(json, "7");
        let mut got = AddrOrPtr::default();
        got.from_json(&json).unwrap();
        assert_eq!(got.serialize(), ptr.serialize());
    }

    #[test]
    fn test_addr_or_list_json_reject_unencodable_count() {
        let too_many = (u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize) + 1;
        let list = AddressW1::from_list(vec![ADDRESS_ONEX; too_many]).unwrap();
        let json = list.to_json();
        let mut obj = AddrOrList::default();
        let err = obj.from_json(&json).unwrap_err();
        assert!(err.contains("exceeds max"), "{}", err);
    }

    #[test]
    fn test_addr_or_list_json_accept_max_encodable_count() {
        let max_ok = u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize;
        let list = AddressW1::from_list(vec![ADDRESS_ONEX; max_ok]).unwrap();
        let json = list.to_json();
        let mut obj = AddrOrList::default();
        obj.from_json(&json).unwrap();
        assert_eq!(obj.serialize(), AddrOrList::Val2(list).serialize());
    }

    #[test]
    fn test_addr_or_list_json_roundtrip_single_address_string() {
        let obj = AddrOrList::from_addr(ADDRESS_ONEX);
        let json = obj.to_json();
        let mut got = AddrOrList::default();
        got.from_json(&json).unwrap();
        assert_eq!(got.serialize(), obj.serialize());
    }

    #[test]
    fn test_addr_or_list_json_reject_empty_list() {
        let mut obj = AddrOrList::default();
        assert!(obj.from_json("[]").is_err());
    }

    #[test]
    fn test_addr_or_list_binary_reject_zero_count_list() {
        let mut obj = AddrOrList::default();
        assert!(obj.parse(&[ADDR_OR_PTR_DIV_NUM]).is_err());
    }

    #[test]
    fn test_addr_or_list_from_list_reject_unencodable_count() {
        let too_many = u8::MAX as usize - ADDR_OR_PTR_DIV_NUM as usize + 1;
        assert!(AddrOrList::from_list(vec![ADDRESS_ONEX; too_many]).is_err());
    }

}
