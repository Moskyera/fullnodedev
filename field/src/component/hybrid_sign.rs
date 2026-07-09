/// Hybrid / PQC signature algorithm identifiers (wire `alg` field).
pub mod sign_alg {
    /// Legacy secp256k1 embedded in Type4 hybrid sign list (33-byte pk + 64-byte sig).
    pub const LEGACY_SECP: u8 = 0;
    /// ML-DSA-65 only (PQCKEY v6 addresses).
    pub const MLDSA65: u8 = 1;
    /// secp256k1 + ML-DSA-65 hybrid (HYBRID v7 addresses).
    pub const HYBRID_SECP_MLDSA65: u8 = 3;

    pub const SECP_PK_SIZE: usize = 33;
    pub const SECP_SIG_SIZE: usize = 64;
    pub const MLDSA65_PK_SIZE: usize = 1952;
    pub const MLDSA65_SIG_SIZE: usize = 3309;

    pub const BODY_LEGACY_SECP: usize = SECP_PK_SIZE + SECP_SIG_SIZE;
    pub const BODY_MLDSA65: usize = MLDSA65_PK_SIZE + MLDSA65_SIG_SIZE;
    pub const BODY_HYBRID: usize =
        SECP_PK_SIZE + SECP_SIG_SIZE + MLDSA65_PK_SIZE + MLDSA65_SIG_SIZE;
}

combi_struct! { HybridSign,
    alg: Uint1
    body: BytesW2
}

impl HybridSign {
    pub fn alg_id(&self) -> u8 {
        *self.alg
    }

    pub fn expected_body_len(alg: u8) -> Ret<usize> {
        use sign_alg::*;
        match alg {
            LEGACY_SECP => Ok(BODY_LEGACY_SECP),
            MLDSA65 => Ok(BODY_MLDSA65),
            HYBRID_SECP_MLDSA65 => Ok(BODY_HYBRID),
            _ => errf!("hybrid sign alg {} not supported", alg),
        }
    }

    pub fn check_wire(&self) -> Rerr {
        let alg = self.alg_id();
        let expect = Self::expected_body_len(alg)?;
        let got = self.body.length();
        maybe!(
            got == expect,
            Ok(()),
            errf!(
                "hybrid sign alg {} body length {} expected {}",
                alg,
                got,
                expect
            )
        )
    }

    pub fn body_bytes(&self) -> &[u8] {
        self.body.as_ref()
    }
}

combi_list!(HybridSignW1, Uint1, HybridSign);
combi_list!(HybridSignW2, Uint2, HybridSign);

#[cfg(test)]
mod hybrid_sign_tests {
    use super::*;

    #[test]
    fn expected_body_lengths_match_mldsa65_wire() {
        use sign_alg::*;
        assert_eq!(BODY_LEGACY_SECP, 97);
        assert_eq!(BODY_MLDSA65, 5261);
        assert_eq!(BODY_HYBRID, 5358);
    }

    #[test]
    fn check_wire_rejects_unknown_alg() {
        let mut sign = HybridSign::default();
        sign.alg = Uint1::from(2u8);
        sign.body = BytesW2::from(vec![0u8; 10]).unwrap();
        assert!(sign.check_wire().is_err());
    }

    #[test]
    fn check_wire_rejects_wrong_body_len() {
        let mut sign = HybridSign::default();
        sign.alg = Uint1::from(sign_alg::MLDSA65);
        sign.body = BytesW2::from(vec![0u8; 100]).unwrap();
        assert!(sign.check_wire().is_err());
    }
}