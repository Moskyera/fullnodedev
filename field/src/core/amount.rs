
use num_bigint::*;
use num_bigint::Sign::*;
use num_traits::*;

const U128S: usize = u128::BITS as usize / 8;
const U64S:  usize =  u64::BITS as usize / 8;

pub const UNIT_MEI:  u8 = 248;
pub const UNIT_244:  u8 = 244;
pub const UNIT_ZHU:  u8 = 240;
pub const UNIT_238:  u8 = 238;
pub const UNIT_SHUO: u8 = 232;
pub const UNIT_AI:   u8 = 224;
pub const UNIT_MIAO: u8 = 216;

const FROM_CHARS: &[u8; 13] = b"0123456789-.:"; 

pub const AMOUNT_STORE_MAX_SIZE: usize = 12;


pub enum AmtMode {
    U64,
    U128,
    BIGINT,
}

pub enum AmtCpr {
    Discard,
    Grow,
}



#[derive(Default, Hash, Clone, PartialEq, Eq)]
pub struct Amount {
	unit: u8,
	dist: i8,
	byte: Vec<u8>,
}

static ZERO_AMT: OnceLock<Amount> = OnceLock::new();

impl std::fmt::Display for Amount {
    fn fmt(&self,f: &mut Formatter) -> Result {
        write!(f,"{}", self.to_fin_string())
    }
}

impl Debug for Amount {
    fn fmt(&self,f: &mut Formatter) -> Result {
        write!(f,"[{},{},{:?}]", self.unit, self.dist, self.byte)
    }
}

impl Ord for Amount {
    fn cmp(&self, other: &Self) -> Ordering {
        Amount::cmp(self, other)
    }
}

impl PartialOrd for Amount {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}


impl Amount {
    fn parse_check_canonical(unit: u8, dist: i8, byte: &[u8]) -> Ret<()> {
        let rbtl = byte.len();
        if rbtl > 1 && bytes_is_zero(byte)  {
            return errf!("multi-byte amount cannot be all zero")
        }
        if rbtl > 1 && byte[0] == 0 {
            return errf!("amount leading zero byte is not canonical")
        }
        if bytes_is_zero(byte) && (unit != 0 || dist != 0 || !byte.is_empty()) {
            return errf!("amount semantic zero is not canonical");
        }
        Ok(())
    }
}

impl Parse for Amount {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        let unit = bufeatone(&buf)?;
        let dist = bufeatone(&buf[1..])?;
        if dist == i8::MIN as u8 {
            return errf!("dist cannot be {}", i8::MIN)
        }
        let dist = dist as i8;
        let btlen = dist.abs() as usize;
        let bts = bufeat_ref(&buf[2..], btlen)?;
        let byte = bts.to_vec();
        let rbtl = byte.len();
        if btlen != rbtl {
            return errf!("dist and byte len mismatch")
        }
        if dist != 0 && rbtl == 0 {
            return errf!("dist and byte zero mismatch")
        }
        Self::parse_check_canonical(unit, dist, &byte)?;
        self.unit = unit;
        self.dist = dist;
        self.byte = byte;
        Ok(2 + btlen)
    }
}

impl Serialize for Amount {
    fn serialize_to(&self, out: &mut Vec<u8>) {
        /*if self.is_zero() {
            if self.unit != 0 || self.dist != 0 || !self.byte.is_empty() {
                panic!("amount: semantic zero in non-canonical form (unit={}, dist={}, byte_len={}) - check historical data", self.unit, self.dist, self.byte.len());
            }
            out.push(0);
            out.push(0);
            return;
        }*/
        out.push(self.unit);
        out.push(self.dist as u8);
        out.extend_from_slice(&self.byte);
    }
    fn size(&self) -> usize {
        1 + 1 + self.byte.len()
    }
}


impl_field_only_new!{Amount}

impl ToJSON for Amount {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        if fmt.unit.is_empty() {
            format!("\"{}\"", self.to_fin_string())
        } else {
            format!("\"{}\"", self.to_unit_string(&fmt.unit))
        }
    }
}

impl FromJSON for Amount {
    fn from_json(&mut self, json_str: &str) -> Ret<()> {
        let s = json_expect_quoted_decoded(json_str)?;
        let amt = Amount::from(&s)?;
        self.unit = amt.unit;
        self.dist = amt.dist;
        self.byte = amt.byte;
        Ok(())
    }
}


impl Amount {

    pub fn unit(&self) -> u8 {
        self.unit
    }

    pub fn dist(&self) -> i8 {
        self.dist
    }

    pub fn byte(&self) -> &Vec<u8> {
        &self.byte
    }

    pub fn tail_len(&self) -> usize {
        self.dist.abs() as usize
    }

    pub fn tail_u128(&self) -> Ret<u128> {
        if self.byte.len() > U128S {
            return errf!("amount tail bytes length exceeds max {}", U128S)
        }
        Ok(u128::from_be_bytes(add_left_padding(&self.byte, U128S).try_into().unwrap()))
    }

    pub fn tail_u64(&self) -> Ret<u64> {
        if self.byte.len() > U64S {
            return errf!("amount tail bytes length exceeds max {}", U64S)
        }
        Ok(u64::from_be_bytes(add_left_padding(&self.byte, U64S).try_into().unwrap()))
    }

    pub fn is_zero(&self) -> bool {
        bytes_is_zero(&self.byte)
    }

    pub fn not_zero(&self) -> bool {
        !self.is_zero()
    }

    // check must be positive and cannot be zero
    pub fn is_positive(&self) -> bool {
        self.dist > 0 && bytes_not_zero(&self.byte)
    }

    // check must be negative and cannot be zero
    pub fn is_negative(&self) -> bool {
        self.dist < 0 && bytes_not_zero(&self.byte)
    }

    fn check_long(&self, max: usize) -> Ret<()> {
        let amtsz = self.size();
        if amtsz > max {
            return errf!("amount {} size {} exceeds max {} for storage", self, amtsz, max)
        }
        Ok(())
    }

    pub fn check_6_long(&self) -> Ret<()> {
        self.check_long(6)
    }

    pub fn check_4_long(&self) -> Ret<()> {
        self.check_long(4)
    }

    pub fn check_store_long(&self) -> Ret<()> {
        self.check_long(AMOUNT_STORE_MAX_SIZE)
    }
    
}


macro_rules! ret_amtfmte {
    ($tip: expr, $v: expr) => {
        return Err(format!("amount {} from '{}' format invalid or overflow", $tip, $v))
    };
}

macro_rules! coin_with {
    ($fn:ident, $ty:ty) => {
        pub fn $fn(mut v: $ty, mut u: u8) -> Self {
            if v == 0 {
                return Self::zero()
            }
            while v % 10 == 0 {
                if u == 255 {
                    break // unit max
                }
                v /= 10;
                u += 1;
            }
            let bts = drop_left_zero(&v.to_be_bytes());
            Self {
                unit: u,
                dist: bts.len() as i8,
                byte: bts
            }
        }
    }
}

// from
impl Amount {

    pub fn zero_ref() -> &'static Self {
        ZERO_AMT.get_or_init(||Self::zero())
    }
    pub fn zero() -> Self {
        Self::default()
    }
    pub fn small(v: u8, u: u8) -> Self {
        if v == 0 {
            return Self::zero()
        }
        Self {
            unit: u,
            dist: 1i8,
            byte: vec![v],
        }
    }
    pub fn small_mei(v: u8) -> Self {
        if v == 0 {
            return Self::zero()
        }
        Self {
            unit: UNIT_MEI,
            dist: 1i8,
            byte: vec![v],
        }
    }
    pub fn mei(v: u64) -> Self {
        Self::coin(v, UNIT_MEI)
    }
    pub fn zhu(v: u64) -> Self {
        Self::coin(v, UNIT_ZHU)
    }
    pub fn shuo(v: u64) -> Self {
        Self::coin(v, UNIT_SHUO)
    }
    pub fn ai(v: u64) -> Self {
        Self::coin(v, UNIT_AI)
    }
    pub fn miao(v: u64) -> Self {
        Self::coin(v, UNIT_MIAO)
    }
    pub fn unit238(v: u64) -> Self {
        Self::coin(v, UNIT_238)
    }

    coin_with!{coin_u128, u128}
    coin_with!{coin_u64,  u64}

    pub fn coin(v: u64, u: u8) -> Self {
        Self::coin_u64(v, u)
    }

    pub fn from(v: &str) -> Ret<Self> {
        let v = v.trim().replace(",", "");
        for a in v.chars() {
            if !a.is_ascii() || !FROM_CHARS.contains(&(a as u8)) {
                ret_amtfmte!{"unsupported characters", String::from(a)}
            }
        }
        maybe!(v.contains(":"), Self::from_fin(v), Self::from_mei(v))
    }

    fn from_fin(v: String) -> Ret<Self> {
        let amt: Vec<&str> = v.split(":").collect();
        if amt.len() != 2 {
            ret_amtfmte!{"amt", v}
        }
        let Ok(u) = amt[1].parse::<u8>() else {
            ret_amtfmte!{"unit", amt[1]}
        };
        let Ok(v) = amt[0].parse::<i128>() else {
            // from bigint
            let Ok(bign) = BigInt::from_str_radix(&amt[0], 10) else {
                return errf!("amount '{}' overflow BigInt::from_str_radix", amt[0])
            };
            let powv = BigInt::from(10u64).pow(u as u32);
            let bign = bign * powv;
            return Self::from_bigint(&bign)
        };
        if v == i128::MIN {
            return errf!("amount '{}' overflow", v)
        }
        // from u128
        let mut amt = Self::coin_u128(v.abs() as u128, u);
        if v < 0 {
            amt.dist *= -1; // if neg
        }
        Ok(amt)
    }
    
    fn from_mei(v: String) -> Ret<Self> {
        let v = v.trim();
        if v.is_empty() {
            ret_amtfmte!{"value", v}
        }
        let (negative, digits) = match v.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, v),
        };
        let parts: Vec<&str> = digits.split('.').collect();
        if parts.len() > 2 {
            ret_amtfmte!{"value", v}
        }
        let int_part = parts[0];
        let frac_part = maybe!(parts.len() == 2, parts[1], "");
        if int_part.is_empty() && frac_part.is_empty() {
            ret_amtfmte!{"value", v}
        }
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
        {
            ret_amtfmte!{"value", v}
        }
        let frac_trimmed = frac_part.trim_end_matches('0');
        let frac_len = frac_trimmed.len();
        if frac_len > UNIT_MEI as usize {
            ret_amtfmte!{"value", v}
        }
        let unit = UNIT_MEI - frac_len as u8;
        let mut digits = String::with_capacity(int_part.len() + frac_trimmed.len());
        digits.push_str(int_part);
        digits.push_str(frac_trimmed);
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Ok(Self::zero());
        }
        let Ok(mut bigv) = BigInt::from_str_radix(digits, 10) else {
            ret_amtfmte!{"value", v}
        };
        if negative {
            bigv = -bigv;
        }
        let powv = BigInt::from(10u64).pow(unit as u32);
        let bigv = bigv * powv;
        Self::from_bigint(&bigv)
    }

    pub fn from_bigint( bignum: &BigInt ) -> Ret<Self> {
        let numstr = bignum.to_string();
        if numstr == "0" {
            return Ok(Amount::zero())
        }
        let mut numuse = numstr.as_str().trim_end_matches('0').to_owned();
        let mut unit = numstr.len() - numuse.len();
        if unit > 255 { // unit max is 255 
            numuse += &"0".repeat(unit - 255);
            unit = 255;
        }
        let biguse = BigInt::from_str_radix(&numuse, 10);
        if let Err(e) = biguse {
            return errf!("BigInt::from_str_radix failed: {} {} {} {}", numstr, numuse, numuse, e.to_string())
        }
        let biguse = biguse.unwrap();
        let (sign, byte) = biguse.to_bytes_be();
        let dist = byte.len();
        if dist > 127 {
            return Err("Amount is too wide.".to_string())
        }
        let mut dist = dist as i8;
        if sign == Minus {
            dist *= -1; // is negative
        }
        // ok
        Ok( Self {
            byte,
            dist,
            unit: unit as u8
        })
    }


    pub fn from_unit_byte(unit: u8, byte: Vec<u8>) -> Ret<Self> {
        let bl = byte.len();
        if bl > 127 {
            return Err("amount bytes len overflow 127.".to_string())
        }
        if bl == 0 || bytes_is_zero(&byte) {
            return Ok(Self::zero())
        }
        let byte = drop_left_zero(&byte);
        if byte.is_empty() {
            return Ok(Self::zero())
        }
        let dist = byte.len();
        if dist > 127 {
            return Err("amount bytes len overflow 127.".to_string())
        }
        Ok(Amount{
            unit: unit,
            dist: dist as i8,
            byte: byte,
        })
    }


}

// to string
impl Amount {


    pub fn sign(&self) -> String {
        maybe!(self.dist < 0, "-", "").to_string()
    }

    pub fn to_string(&self) -> String {
        let a = self.to_fin_string();
        "ㄜ".to_owned() + a.as_str()
    }

    pub fn to_fin_string(&self) -> String {
        let (a, b, c) = self.to_string_part();
        format!("{}{}:{}", a, b, c)
    }

    pub fn to_string_part(&self) -> (String, String, String) {
        let blen = self.tail_len();
        let s2 = maybe!(blen > U128S, 
            BigInt::from_bytes_be(Plus, &self.byte).to_string(),
            maybe!(blen > U64S,
                u128::from_be_bytes(add_left_padding(&self.byte, U128S).try_into().unwrap()).to_string(),
                u64::from_be_bytes(add_left_padding(&self.byte,  U64S).try_into().unwrap()).to_string()
            )
        );
        (self.sign(), s2, self.unit.to_string())
    }

    pub fn to_bigint(&self) -> BigInt {
        if self.is_zero() {
            return 0u64.into();
        }
        let sig = maybe!(self.dist > 0, Plus, Minus); // sign
        let bignum = BigInt::from_bytes_be(sig, &self.byte[..]);
        let base: BigInt = 10u64.into();
        let powv = base.pow(self.unit as u32);
        bignum * powv
    }

    pub fn to_biguint(&self) -> BigUint {
        assert!(!self.is_negative());
        if self.is_zero() {
            return 0u64.into();
        }
        let numv = BigUint::from_bytes_be(&self.byte[..]);
        let powv = BigUint::from(10u64).pow(self.unit as u64);
        numv * powv
    }


    pub fn to_unit_string(&self, unit_str: &str) -> String {
        let unit;
        if let Ok(u) = unit_str.parse::<u8>() {
            unit = u;
        }else{
            unit = match unit_str {
                "mei"  => UNIT_MEI,
                "zhu"  => UNIT_ZHU,
                "shuo" => UNIT_SHUO,
                "ai"   => UNIT_AI,
                "miao" => UNIT_MIAO,
                _ => 0,
            }
        }
        maybe!(unit > 0, unsafe { self.to_unit_float(unit).to_string() }, self.to_fin_string())
    }

}


macro_rules! to_unit_define {
    ($fu64:ident, $fu128:ident, $unit:expr) => {
        
    pub fn $fu128(&self) -> Ret<u128> {
        let Some(v) = self.to_unit_biguint($unit).and_then(|v| v.to_u128()) else {
            return errf!("amount {} overflow {} u128", self.to_fin_string(), stringify!($fu128))
        };
        Ok(v)
    }
    
    pub fn $fu64(&self) -> Ret<u64> {
        let u = self.$fu128()?;
        if u > u64::MAX as u128 {
            return errf!("amount {} overflow {} u64", self.to_fin_string(), stringify!($fu64))
        }
        Ok(u as u64)
    }
    };
}



impl Amount {

    to_unit_define!{ to_mei_u64, to_mei_u128, UNIT_MEI }
    to_unit_define!{ to_244_u64, to_244_u128, UNIT_244 }
    to_unit_define!{ to_zhu_u64, to_zhu_u128, UNIT_ZHU }
    to_unit_define!{ to_238_u64, to_238_u128, UNIT_238 } // for fee_purity

    pub fn to_unit_biguint(&self, base_unit: u8) -> Option<BigUint> {
        if self.is_negative() {
            return None
        }
        if self.is_zero() {
            return Some(0u64.into())
        }
        let bigu = self.to_biguint();
        let powv: BigUint = BigUint::from(10u64).pow(base_unit as u64);
        Some(bigu / powv)
    }

    pub unsafe fn to_unit_float(&self, base_unit: u8) -> f64 {
        if self.is_zero() {
            return 0f64
        }
        let chax = (base_unit as i64 - (self.unit as i64)).abs();
        let tv = maybe!(self.tail_len() <= U128S,
            self.tail_u128().unwrap() as f64, // u128
            match BigInt::from_bytes_be(Plus, &self.byte[..]).to_f64() {
                Some(v) => v,
                None => return f64::NAN,
            }
        );
        // by f64
        let base = 10f64.powf(chax as f64);
        let resv = maybe!(self.unit > base_unit, tv * base, tv / base);
        // sign
        return maybe!(self.dist > 0, resv, resv * -1f64);

    }

}


macro_rules! cmp_with {
    ($fn:ident, $ty:ty) => {
        fn $fn(&self, src: &Amount) -> Ordering {
            use Ordering::*;
            concat_idents!{ tail_u = tail_, $ty {
                let mut tns1 = self.tail_u().unwrap().to_string();
                let mut tns2 =  src.tail_u().unwrap().to_string();
            }}
            let ts1 = tns1.len();
            let ts2 = tns2.len();
            let us1 = self.unit as usize;
            let us2 =  src.unit as usize;
            let rlunit1 = us1 + ts1;
            let rlunit2 = us2 + ts2;
            if rlunit1 > rlunit2 {
                return Greater
            } else if rlunit1 < rlunit2 {
                return Less
            }
            // byte width is same
            if us1 > us2 {
                tns1 += &"0".repeat(us1-us2);
            } else if us1 < us2 {
                tns2 += &"0".repeat(us2-us1);
            }
            let ns = tns1.len(); 
            assert!(ns==tns2.len());
            // !("greater_with: {}   {}", tns1, tns2);
            for i in 0 .. ns {
                let a1 = tns1.as_bytes()[i];
                let a2 = tns2.as_bytes()[i];
                if a1 > a2 {
                    return Greater // more
                }else if a1 < a2 {
                    return Less // less
                }
            }
            return Equal // all same
        }
        
    };
}


// compare 
impl Amount {

    pub fn equal(&self, src: &Amount) -> bool {
        self.unit == src.unit &&
        self.dist == src.dist &&
        self.byte == src.byte
        ||
        self.is_zero() && src.is_zero()
    }

    cmp_with!{cmp_mode_u128, u128}
    cmp_with!{cmp_mode_u64,  u64}

    pub fn cmp_mode_bigint(&self, src: &Amount) -> Ordering {
        let db = self.to_bigint();
        let sb =  src.to_bigint();
        db.cmp(&sb)
    }

    pub fn cmp(&self, src: &Amount) -> Ordering {
        use Ordering::*;
        if self.dist < 0 || src.dist < 0 {
            // panic!("cannot compare between with negative")
            return self.cmp_mode_bigint(src)
        }
        if self.equal(src) {
            return Equal // a == b
        }
        let dzro = self.is_zero();
        let szro =  src.is_zero();
        if dzro && szro {
            return Equal // both(0
        } else if dzro {
            return Less // left(0) < right(+)
        } else if szro {
            return Greater // left(+) > right(0)
        }
        // U128 or U64
        let dtl = self.tail_len();
        let stl =  src.tail_len();
        if dtl <= U64S && stl <= U64S {
            return self.cmp_mode_u64(src)
        } else if dtl <= U128S && stl <= U128S {
            return self.cmp_mode_u128(src)
        }else {
            return self.cmp_mode_bigint(src)
        }
        // UBIG
    }


}

// compute 
impl Amount {

    pub fn dist_mul(&self, n: u128) -> Ret<Self> {
        if self.is_zero() {
            return Ok(Self::zero())
        }
        if self.dist < 0 {
            return errf!("dist_mul not supported for negative amount")
        }
        if self.byte.len() > U128S {
            return errf!("dist_mul failed: dist overflow")
        }
        let du = u128::from_be_bytes(add_left_padding(&self.byte, U128S).try_into().unwrap());
        let Some(du) = du.checked_mul(n) else {
            return errf!("dist_mul failed: u128 overflow")
        };
        Ok(Self::coin_u128(du, self.unit))
    }

    pub fn ratio_floor(&self, numerator: u64, denominator: u64) -> Ret<Self> {
        if denominator == 0 {
            return errf!("ratio_floor denominator cannot be zero")
        }
        if self.is_negative() {
            return errf!("cannot ratio_floor for negative")
        }
        if self.is_zero() || numerator == 0 {
            return Ok(Self::zero())
        }
        let base = self.to_bigint();
        let scaled = base * BigInt::from(numerator);
        let part = scaled / BigInt::from(denominator);
        Self::from_bigint(&part)
    }

    pub fn ten_percent_floor(&self) -> Ret<Self> {
        self.ratio_floor(1, 10)
    }

    pub fn unit_sub(&self, sub: u8) -> Ret<Self> {
        if sub == 0 {
            return Ok(self.clone())
        }
        if sub > self.unit {
            return errf!("unit_sub failed: unit must be greater than {}", sub)
        }
        let mut res = self.clone();
        res.unit -= sub;
        Ok(res)
    }

    pub fn add_mode_bigint(&self, src: &Amount) -> Ret<Self> {
        let mut db = self.to_bigint();
        let ds =  src.to_bigint();
        db = db + ds;
        Self::from_bigint(&db)
    }

    pub fn sub_mode_bigint(&self, src: &Amount) -> Ret<Self> {
        let mut db = self.to_bigint();
        let ds =  src.to_bigint();
        db = db - ds;
        Self::from_bigint(&db)
    }

    pub fn add(&self, amt: &Amount, mode: AmtMode) -> Ret<Self> {
        if self.is_negative() || amt.is_negative() {
            return self.add_mode_bigint(amt)
        }
        match mode {
            AmtMode::U64 => self.add_mode_u64(amt),
            AmtMode::U128 => self.add_mode_u128(amt),
            AmtMode::BIGINT => self.add_mode_bigint(amt),
        }
    }

    pub fn sub(&self, amt: &Amount, mode: AmtMode) -> Ret<Self> {
        if self.is_negative() || amt.is_negative() {
            return self.sub_mode_bigint(amt)
        }
        match mode {
            AmtMode::U64 => self.sub_mode_u64(amt),
            AmtMode::U128 => self.sub_mode_u128(amt),
            AmtMode::BIGINT => self.sub_mode_bigint(amt),
        }
    }

    pub fn compress(&self, btn: usize, cpr: AmtCpr) -> Ret<Self> {
        if self.dist < 0 {
            return errf!("cannot compress negative amount")
        }
        let mut amt = self.clone();
        if amt.tail_len() > U128S {
            return errf!("amount bytes too long to compress")
        }
        while amt.tail_len() > btn {
            if amt.unit == 255 {
                return errf!("amount uint too large to compress")
            }
            let mut numpls = u128::from_be_bytes(add_left_padding(&amt.byte, U128S).try_into().unwrap()) / 10;
            if let AmtCpr::Grow = cpr {
                numpls += 1;
            }
            let nbts = drop_left_zero(&numpls.to_be_bytes());
            // update
            amt.unit += 1;
            amt.dist = nbts.len() as i8;
            amt.byte = nbts;
        }
        // Normalize semantic zero to canonical so (unit,0,[]) never produced
        if amt.is_zero() {
            return Ok(Self::zero());
        }
        Ok(amt)
    }


}


/************* compute *************/


macro_rules! rte_ovfl {
    () => {
        return Err("amount computing size overflow".to_string());
    };
}
macro_rules! rte_cneg {
    ($tip: expr) => {
        return Err(format!("amount {}: operands cannot be negative", $tip));
    };
}

fn bytes_not_zero(v: &[u8]) -> bool {
    v.iter().any(|a|*a>0)
}

fn bytes_is_zero(v: &[u8]) -> bool {
    !bytes_not_zero(v)
}

fn add_left_padding(v: &Vec<u8>, n: usize) -> Vec<u8> {
    assert!(v.len() <= n, "add_left_padding: byte len {} > {}", v.len(), n);
    vec![
        vec![0u8; n - v.len()],
        v.clone(),
    ].concat()
}

fn drop_left_zero(v: &[u8]) -> Vec<u8> {
    let mut res = &v[..];
    while res.len() > 0 && res[0] == 0 {
        res = &res[1..];
    }
    res.to_vec()
}


macro_rules! compute_mode_define {
    ($fun:ident, $op:ident, $ty:ty, $ts:expr, $add_or_sub:expr) => {

        pub fn $fun(&self, src: &Amount) -> Ret<Self> {
            let dst: &Amount = self;
            if dst.dist < 0 || src.dist < 0 {
                rte_cneg!{stringify!($op)}
            }
            let dzro = dst.is_zero();
            let szro = src.is_zero();
            if dzro && szro {
                return Ok(Self::zero())
            }
            if $add_or_sub {
                // add
                if dzro {
                    return Ok(src.clone())
                }else if szro {
                    return Ok(dst.clone())
                }
            }else{
                // sub
                if dzro {
                    rte_ovfl!{}
                }else if szro {
                    return Ok(dst.clone())
                }
            }
            // both not zero
            let dtl = dst.tail_len();
            let stl = src.tail_len();
            if dtl > $ts || stl > $ts {
                rte_ovfl!{}
            }
            let mut du = <$ty>::from_be_bytes(add_left_padding(&dst.byte, $ts).try_into().unwrap());
            let mut su = <$ty>::from_be_bytes(add_left_padding(&src.byte, $ts).try_into().unwrap());
            let utsk = (dst.unit as i32 - src.unit as i32).abs() as u32;
            let baseut;
            if dst.unit > src.unit {
                let Some(pwn) = (10 as $ty).checked_pow(utsk) else {
                    rte_ovfl!{}
                };
                let Some(ndu) = du.checked_mul( pwn as $ty ) else {
                    rte_ovfl!{}
                };
                du = ndu;
                baseut = src.unit;
            }else if dst.unit < src.unit {
                let Some(pwn) = (10 as $ty).checked_pow(utsk) else {
                    rte_ovfl!{}
                };
                let Some(nsu) = su.checked_mul( pwn as $ty ) else {
                    rte_ovfl!{}
                };
                su = nsu;
                baseut = dst.unit;
            }else{
                baseut = dst.unit;
                if !$add_or_sub && du == su {
                    // sub with same
                    return Ok(Self::zero())
                }
            }
            // do add
            let Some(resv) = du.$op( su ) else {
                // return self.add_mode_bigint(src) // to mode bigint
                rte_ovfl!{}
            };

            concat_idents!{ coin_u = coin_, $ty {
                Ok(Self::coin_u(resv, baseut))
            }} 
        }
    }
}

impl Amount {

compute_mode_define!{add_mode_u64,  checked_add, u64,   U64S, true}
compute_mode_define!{add_mode_u128, checked_add, u128, U128S, true}
compute_mode_define!{sub_mode_u64,  checked_sub, u64,   U64S, false}
compute_mode_define!{sub_mode_u128, checked_sub, u128, U128S, false}

}



/************************ test ************************/







#[cfg(test)]
mod amount_tests {
    use super::*;

    #[test]
    fn test1() {

        let a1 = Amount::mei(9527);
        let a2 = Amount::coin(9527, 248);
        let a3 = Amount::from("133188:246").unwrap();
        let a4 = Amount::from("1000.88   ").unwrap();
        let a3 = a3.sub(&Amount::mei(331), AmtMode::U64).unwrap();
        assert_eq!(a1.to_fin_string(), a2.to_fin_string());
        assert_eq!(a3.to_fin_string(), a4.to_fin_string());

    }


    #[test]
    fn test2() {

        let a1 = Amount::mei(1000);
        let a2 = Amount::mei(2000);
        let a3 = Amount::mei(3000);
        let a4 = a1.add_mode_u128(&a2).unwrap();
        let a5 = a1.add_mode_u64(&a2).unwrap();
        let a6 = a3.sub_mode_u128(&a1).unwrap();
        let a7 = a3.sub_mode_u128(&a4).unwrap();
        assert_eq!(a3.to_fin_string(), a4.to_fin_string());
        assert_eq!(a3.to_fin_string(), a5.to_fin_string());
        assert_eq!(a3.to_fin_string(), "3:251");
        assert_eq!(a6.to_fin_string(), a2.to_fin_string());
        assert_eq!(a7.to_fin_string(), "0:0");
        println!("{}  {}  {}  {}  {}  {}  {}", a1, a2, a3, a4, a5, a6, a7);
        println!("{:?}  {:?}  {:?}  {:?}  {:?}  {:?}  {:?}", a1, a2, a3, a4, a5, a6, a7);

    }

    #[test]
    fn test3() {
        let stf = "340282366920938463463374607431768211455";
        let a0 = Amount::from("7:200").unwrap();
        for i in 1 .. stf.len() {
            let f = format!("{}:200", &stf[..i]);
            let a1 = Amount::from(&f).unwrap();
            let a3 = a0.add_mode_u128(&a1).unwrap();
            println!("{}", a3);
        }

    }

    #[test]
    fn test4() {
        let stf = "340282366920938463463374607431768211455";
        let a0 = Amount::from(&format!("{}:200", stf)).unwrap();
        let stl = stf.len();
        for i in 1 .. stl {
            let a1 = Amount::from(&format!("{}:200", &stf[stl-i..])).unwrap();
            let a2 = a0.sub_mode_u128(&a1).unwrap();
            println!("{}", a2);
        }

    }


    #[test]
    fn test5() {
        let stf = "340282366920938463463374607431768211455";
        let a0 = Amount::from(&format!("{}:100", stf)).unwrap();
        for i in 1 .. 16 {
            let a1 = a0.compress(16-i, AmtCpr::Grow).unwrap();
            let a2 = a0.compress(16-i, AmtCpr::Discard).unwrap();
            println!("{}", a1);
            println!("{}", a2);
        }
    }


    #[test]
    fn test6() {
        let stf = "34028236692093846346337460743176821145579745987243958534961784351938476103756328479843750927435562347109475";
        let _a0 = Amount::from(&format!("{}:100", stf)).unwrap();
        let stl = stf.len();
        for i in 1 .. stl {
            let a1 = Amount::from(&format!("{}:100", &stf[stl-i..])).unwrap();
            println!("{}", a1)
            // let a2 = a0.sub_mode_bigint(&a1).unwrap();
            // println!("{}", a2);
        }

    }

    #[test]
    fn test7() {

        let a1 = Amount::from("11111111111111111:201").unwrap();
        let a2 = Amount::from("111111111111112:202").unwrap();
        // assert!(a2 > a1);
        println!("{} {} {} {} {}", a1<a2, a1==a2, a1>a2, a1>=a2, a1<=a2)

    }


    #[test]
    fn test8() {

        // RUST_BACKTRACE=all cargo test amount_tests::test8 -- --nocapture

        use std::time::Instant;

        let a1 = Amount::mei(1234567890);
        let a2 = Amount::mei(3344520);
        let mx = 100000usize;
        let mut aa = Amount::zero();

        let now = Instant::now();
        for _ in 0..mx {
            aa = a1.add_mode_u64(&a2).unwrap();
        }
        println!("{}  {}", aa, now.elapsed().as_secs_f32());


        let now = Instant::now();
        for _ in 0..mx {
            aa = a1.add_mode_u128(&a2).unwrap();
        }
        println!("{}  {}", aa, now.elapsed().as_secs_f32());


        let now = Instant::now();
        for _ in 0..mx {
            aa = a1.add_mode_bigint(&a2).unwrap();
        }
        println!("{}  {}", aa, now.elapsed().as_secs_f32());


    }


    #[test]
    fn test9() {

        let a1 = Amount::zero();
        let a2 = Amount{
            unit: 0,
            dist: 1,
            byte: vec![0],
        };

        println!("bytes_not_zero  {} {} {}", bytes_not_zero(&[]), bytes_not_zero(&[0]), bytes_not_zero(&[0,0,0,0]));

        println!("a1 = {:?}, a2 = {:?}, a1 < a2 = {}", a1, a2, a1 < a2);

    }

    #[test]
    fn test_bigint_round_trip_and_units() {
        let full_value = "1234567890123456789012345678901234567890";
        let zero_unit_amt = Amount::from(&format!("{}:0", full_value)).unwrap();
        let expect_int = BigInt::from_str_radix(full_value, 10).unwrap();
        assert_eq!(zero_unit_amt.to_bigint(), expect_int);

        let trailing_zero_value = "9876500000";
        let base_int = BigInt::from_str_radix(trailing_zero_value, 10).unwrap();
        let amt = Amount::from_bigint(&base_int).unwrap();
        assert_eq!(amt.to_bigint(), base_int);
        assert!(amt.unit() > 0);
        let roundtrip = Amount::from_bigint(&amt.to_bigint()).unwrap();
        assert_eq!(roundtrip.to_bigint(), base_int);
    }

    #[test]
    fn test_bigint_add_sub_consistency() {
        let a = Amount::from_bigint(
            &BigInt::from_str_radix("111111111111111111111111111111111111111", 10).unwrap()
        ).unwrap();
        let b = Amount::from_bigint(
            &BigInt::from_str_radix("222222222222222222222222222222222222222", 10).unwrap()
        ).unwrap();
        let sum = a.add_mode_bigint(&b).unwrap();
        let expected_sum = a.to_bigint() + b.to_bigint();
        assert_eq!(sum.to_bigint(), expected_sum);
        let diff = sum.sub_mode_bigint(&b).unwrap();
        assert_eq!(diff.to_bigint(), a.to_bigint());
    }

    #[test]
    fn test_serialize_parse_roundtrip() {
        let amt = Amount::from("123456789012345:248").unwrap();
        let serialized = amt.serialize();
        let mut parsed = Amount::default();
        parsed.parse(&serialized).unwrap();
        assert!(parsed.equal(&amt));
    }

    #[test]
    fn test_from_unit_byte_zero_normalizes() {
        let amt = Amount::from_unit_byte(UNIT_MEI, vec![0]).unwrap();
        assert!(amt.is_zero());
        assert_eq!(amt.to_bigint(), BigInt::from(0));
    }

    #[test]
    fn test_from_unit_byte_strips_leading_zeros() {
        let with_leading = Amount::from_unit_byte(UNIT_MEI, vec![0, 0, 1]).unwrap();
        let canonical = Amount::mei(1);
        assert!(with_leading.equal(&canonical), "from_unit_byte with leading zeros must match coin/mei");
        assert_eq!(with_leading.to_bigint(), canonical.to_bigint());
    }

    #[test]
    fn test_zero_serialize_parse_semantic_zero_accepted() {
        let canonical = Amount::zero();
        let bytes = canonical.serialize();
        assert_eq!(bytes, &[0u8, 0], "zero must serialize to exactly [0, 0]");
        let mut parsed = Amount::default();
        parsed.parse(&bytes).unwrap();
        assert!(parsed.unit() == 0 && parsed.dist() == 0 && parsed.byte().is_empty(),
            "parse of [0,0] must yield canonical (0,0,[])");
        assert!(parsed.equal(&canonical));

        let alt_zero = Amount { unit: 0, dist: 1, byte: vec![0] };
        let alt_ser = alt_zero.serialize();
        assert_eq!(alt_ser, vec![0u8, 1, 0], "non-canonical semantic zero keeps raw encoding");

        let non_canonical_zero_bytes = vec![0u8, 1, 0];
        let mut parsed = Amount::default();
        assert!(
            parsed.parse(&non_canonical_zero_bytes).is_err(),
            "strict Amount must reject non-canonical semantic zero [0,1,0]"
        );
    }

    #[test]
    fn test_compress_zero_yields_canonical() {
        let small = Amount::mei(5);
        let compressed = small.compress(0, AmtCpr::Discard).unwrap();
        assert!(compressed.is_zero(), "5/10 rounds to 0");
        assert!(compressed.unit() == 0 && compressed.dist() == 0 && compressed.byte().is_empty(),
            "compress that yields zero must return canonical (0,0,[])");
    }

    #[test]
    fn test_unit_sub_zero_is_noop() {
        let a = Amount::mei(100);
        let b = a.unit_sub(0).unwrap();
        assert!(a.equal(&b));
        let z = Amount::zero();
        let z2 = z.unit_sub(0).unwrap();
        assert!(z2.is_zero());
    }

    #[test]
    fn test_unit_sub_eq_unit_yields_zero_unit() {
        // unit_sub(self.unit) should succeed and yield unit 0 (valid)
        let a = Amount::mei(1); // unit=248
        let b = a.unit_sub(UNIT_MEI).unwrap();
        assert_eq!(b.unit(), 0, "unit_sub(248) on unit 248 should yield unit 0");
        assert!(b.equal(&Amount::from("1:0").unwrap()));
    }

    #[test]
    fn test_ten_percent_floor_behaviour() {
        let a = Amount::from("100:0").unwrap();
        assert!(a.ten_percent_floor().unwrap().equal(&Amount::from("10:0").unwrap()));

        let b = Amount::from("9:0").unwrap();
        assert!(b.ten_percent_floor().unwrap().is_zero(), "9 * 10% floor should be zero");

        let c = Amount::from("123:0").unwrap();
        assert!(c.ten_percent_floor().unwrap().equal(&Amount::from("12:0").unwrap()));
    }

    #[test]
    fn test_from_rejects_non_ascii() {
        assert!(Amount::from("1\u{012e}2:248").is_err(), "non-ASCII char must be rejected");
        assert!(Amount::from("100：248").is_err(), "fullwidth colon must be rejected");
    }

    #[test]
    fn test_serialize_size_matches_len() {
        let zero = Amount::zero();
        assert_eq!(zero.serialize().len(), zero.size());
        let a = Amount::from("123:248").unwrap();
        assert_eq!(a.serialize().len(), a.size());
        let b = Amount::mei(1);
        assert_eq!(b.serialize().len(), b.size());
    }

    #[test]
    fn test_bigint_negative_roundtrip() {
        let neg = BigInt::from(-12300);
        let amt = Amount::from_bigint(&neg).unwrap();
        assert_eq!(amt.to_bigint(), neg);
        let roundtrip = Amount::from_bigint(&amt.to_bigint()).unwrap();
        assert_eq!(roundtrip.to_bigint(), neg);
        let neg2 = Amount::from("-123:248").unwrap();
        assert_eq!(neg2.to_bigint(), BigInt::from(-123) * BigInt::from(10u64).pow(248u32));
    }

    #[test]
    fn test_bigint_unit_cap_roundtrip() {
        let huge = BigInt::from(10u64).pow(300u32);
        let amt = Amount::from_bigint(&huge).unwrap();
        assert_eq!(amt.to_bigint(), huge, "unit>255 cap must preserve value");
    }

    #[test]
    fn test_check_store_long_guard() {
        let ok = Amount::from("1234567890:248").unwrap();
        assert!(ok.size() <= AMOUNT_STORE_MAX_SIZE);
        assert!(ok.check_store_long().is_ok());

        let long = Amount::from("340282366920938463463374607431768211455:0").unwrap();
        assert!(long.size() > AMOUNT_STORE_MAX_SIZE);
        assert!(long.check_store_long().is_err());
    }

    #[test]
    fn test_invariant_add_then_sub_u128() {
        let pairs = [
            (Amount::from("1:248").unwrap(), Amount::from("2:248").unwrap()),
            (Amount::from("999999:220").unwrap(), Amount::from("12345:220").unwrap()),
            (Amount::from("100000000000:200").unwrap(), Amount::from("3:200").unwrap()),
        ];

        for (a, b) in pairs {
            let sum = a.add_mode_u128(&b).unwrap();
            let back = sum.sub_mode_u128(&b).unwrap();
            assert!(back.equal(&a));
        }
    }

    #[test]
    fn test_invariant_sub_then_add_u128() {
        let pairs = [
            (Amount::from("1000:248").unwrap(), Amount::from("1:248").unwrap()),
            (Amount::from("123456789:210").unwrap(), Amount::from("23456789:210").unwrap()),
            (Amount::from("340282366920938463463374607431768211455:0").unwrap(), Amount::from("1:0").unwrap()),
        ];

        for (a, b) in pairs {
            let diff = a.sub_mode_u128(&b).unwrap();
            let back = diff.add_mode_u128(&b).unwrap();
            assert!(back.equal(&a));
        }
    }

    #[test]
    fn mined_balance_send_and_fee_sub() {
        let mut bal = Amount::zero();
        for _ in 0..105 {
            bal = bal.add_mode_u128(&Amount::small_mei(1)).expect("add reward");
        }
        // Wallet wire "45:0" is parsed as fin(value:unit) -> unit 0, causing 10^248 overflow vs mei balance.
        assert!(bal.sub_mode_u128(&Amount::from("45:0").unwrap()).is_err());
        // Dot mei strings align units correctly.
        let send = Amount::from("45.0").unwrap();
        let fee = Amount::from("1.244").unwrap();
        bal.sub_mode_u128(&send).expect("sub send");
        bal.sub_mode_u128(&send)
            .unwrap()
            .sub_mode_u128(&fee)
            .expect("sub fee");
    }
}
