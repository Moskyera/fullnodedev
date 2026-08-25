//! Shared hash / diamond comparison helpers for CPU and OpenCL mining paths.

pub fn hash_more_power(dst: &[u8], src: &[u8]) -> bool {
    let mut ln = dst.len();
    let l2 = src.len();
    if l2 < ln {
        ln = l2;
    }
    for i in 0..ln {
        let (l, r) = (dst[i], src[i]);
        if l < r {
            return true;
        } else if l > r {
            return false;
        }
    }
    false
}

pub fn hash_left_zero_pad3(dst: &[u8]) -> Vec<u8> {
    let mut idx = 0usize;
    for (i, &byte) in dst.iter().enumerate() {
        if byte > 0 {
            idx = i;
            break;
        }
    }
    // Clamp the end: a degenerate hash whose first non-zero byte sits in the last
    // two bytes (or an input shorter than 3 bytes) would otherwise slice past the
    // end and panic inside the sole result/submit thread.
    let end = (idx + 3).min(dst.len());
    dst[..end].to_vec()
}

pub fn diamond_more_power(dst: &[u8], src: &[u8]) -> bool {
    let o = b'0';
    for i in 0..dst.len().min(src.len()) {
        let (l, r) = (dst[i], src[i]);
        if l == o && r != o {
            return true;
        } else if l != o && r == o {
            return false;
        } else if l != o && r != o {
            return false;
        }
    }
    false
}

/// Mainnet consensus name shape: exactly DMD_L leading '0' chars followed by
/// DMD_M-DMD_L non-'0' chars. Mirrors `x16rs::check_diamond_hash_result` (and
/// `diamond_is_valid_name` in x16rs_diamond.cl) without allocating.
pub fn diamond_name_is_valid(dia: &[u8]) -> bool {
    const DMD_L: usize = 10;
    const DMD_M: usize = 16;
    if dia.len() != DMD_M {
        return false;
    }
    dia[..DMD_L].iter().all(|c| *c == b'0') && dia[DMD_L..].iter().all(|c| *c != b'0')
}

/// True when candidate `dst` should replace the current best `src`.
///
/// `diamond_more_power` on its own is pure more-leading-zeros-wins, so it ranks
/// an 11+ zero overshoot (which can never be minted) above a real diamond. The
/// GPU kernel already ranks valid-first; this is the same rule on the host so
/// kernel, host and the console "best so far" cannot disagree.
pub fn diamond_better(dst: &[u8], src: &[u8]) -> bool {
    match (diamond_name_is_valid(dst), diamond_name_is_valid(src)) {
        (true, false) => true,
        (false, true) => false,
        _ => diamond_more_power(dst, src),
    }
}

/// Step 1 of `x16rs::check_diamond_difficulty`, with the per-number terms hoisted
/// out of the nonce loop.
///
/// COPY NOTICE. The two fields and `passes` below are a VERBATIM copy of the
/// "check step 1" block of `x16rs::check_diamond_difficulty`
/// (x16rs/src/diamond.rs:58) -- same MODIFFBITS table, same `number / 42000`,
/// same `255 - ((number / 65536) as u8)`, same `>=` / `>` comparisons, same
/// iteration order. It is duplicated here, beside `diamond_name_is_valid`, so
/// that x16rs/ stays byte-for-byte untouched and therefore the full node stays
/// untouched. IF `check_diamond_difficulty` EVER CHANGES, THIS MUST CHANGE WITH
/// IT: `the_gate_is_a_verbatim_copy_of_step_one` and
/// `a_failing_gate_forbids_every_possible_x16rs_hash` below are the tripwires,
/// and they fail loudly rather than silently mining a wrong difficulty.
///
/// The whole point: step 1 reads ONLY `sha3hx`. It never looks at the x16rs
/// hash. So when it fails, `check_diamond_difficulty` returns false for EVERY
/// possible x16rs hash, and the expensive `x16rs_hash` rounds for that nonce
/// cannot produce a diamond no matter what they compute.
pub struct DiamondSha3Gate {
    shnumlp: usize,
    shmaxit: u8,
}

impl DiamondSha3Gate {
    /// Both terms depend only on the diamond number, which is constant for a
    /// whole mining round, so they are computed once per round instead of once
    /// per nonce.
    pub fn for_number(number: u32) -> Self {
        Self {
            shnumlp: number as usize / 42000, // 32step max to 64 years
            shmaxit: 255 - ((number / 65536) as u8),
        }
    }

    /// False means: no x16rs hash whatsoever can turn this sha3 into a diamond.
    pub fn passes(&self, sha3hx: &[u8; x16rs::H32S]) -> bool {
        const MODIFFBITS: [u8; x16rs::H32S] = [
            // difficulty requirements
            128, 132, 136, 140, 144, 148, 152, 156, // step +4
            160, 164, 168, 172, 176, 180, 184, 188, 192, 196, 200, 204, 208, 212, 216, 220, 224,
            228, 232, 236, 240, 244, 248, 252,
        ];
        for i in 0..x16rs::H32S {
            if i < self.shnumlp && sha3hx[i] >= MODIFFBITS[i] {
                return false; // fail
            }
            if sha3hx[i] > self.shmaxit {
                return false; // fail
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x16rs::{H32S, check_diamond_difficulty, check_diamond_hash_result, diamond_hash};

    /// Deterministic splitmix64. A fixed seed is deliberate: a failure here is a
    /// consensus failure, and it must reproduce exactly on the next run.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn byte(&mut self) -> u8 {
            (self.next_u64() >> 24) as u8
        }

        fn hash(&mut self) -> [u8; H32S] {
            let mut h = [0u8; H32S];
            for b in h.iter_mut() {
                *b = self.byte();
            }
            h
        }

        /// A hash biased towards small leading bytes, so that the gate actually
        /// PASSES sometimes at high diamond numbers. A uniform draw fails step 1
        /// on essentially every attempt there, and the implication would then
        /// hold vacuously. The prefix length sweeps 0..=32 so that both sides of
        /// the gate are exercised at every number in NUMBERS.
        fn easy_hash(&mut self) -> [u8; H32S] {
            let mut h = self.hash();
            let lead = (self.next_u64() % (H32S as u64 + 1)) as usize;
            for b in h.iter_mut().take(lead) {
                *b = self.byte() % 64; // below MODIFFBITS[0]=128 and below every shmaxit
            }
            h
        }
    }

    /// Diamond numbers spanning every regime of step 1: before the 42000 loop
    /// starts, the measured 133700 point, the sparse high numbers, and the
    /// 65536 boundaries where `shmaxit` steps down.
    const NUMBERS: [u32; 14] = [
        0, 1, 41_999, 42_000, 65_535, 65_536, 65_537, 84_000, 131_072, 133_700, 210_000, 300_000,
        1_000_000, 1_344_000,
    ];

    /// THE EQUIVALENCE PROOF, and the reason the prefilter is allowed to exist.
    ///
    /// It quantifies over the x16rs hash instead of drawing one: for each
    /// (number, sha3) whose gate fails, EVERY x16rs hash in a set that includes
    /// the extreme values (all zero -- the strongest hash the algorithm can
    /// produce, which satisfies step 2 at any difficulty) must still be
    /// rejected. That is strictly stronger than sampling, and it is why
    /// skipping the x16rs rounds cannot lose a diamond.
    #[test]
    fn a_failing_gate_forbids_every_possible_x16rs_hash() {
        let mut rng = Rng(0x0DDB_A11C_0FFE_E123);
        let mut failed = 0usize;
        let mut passed = 0usize;
        for number in NUMBERS {
            let gate = DiamondSha3Gate::for_number(number);
            for k in 0..400 {
                let sha3hx = if k % 2 == 0 {
                    rng.hash()
                } else {
                    rng.easy_hash()
                };
                if gate.passes(&sha3hx) {
                    passed += 1;
                    continue;
                }
                failed += 1;
                // Arbitrary x16rs hashes, including the two extremes and the
                // all-zero hash that clears step 2 at any difficulty.
                let mut arbitrary = vec![[0u8; H32S], [255u8; H32S], [1u8; H32S]];
                let mut alt = [0u8; H32S];
                alt[H32S - 1] = 1;
                arbitrary.push(alt);
                for _ in 0..8 {
                    arbitrary.push(rng.hash());
                }
                for x16rshx in &arbitrary {
                    assert!(
                        !check_diamond_difficulty(number, &sha3hx, x16rshx),
                        "gate rejected number={} sha3={:?} but check_diamond_difficulty \
                         accepted it with x16rs={:?}",
                        number,
                        sha3hx,
                        x16rshx
                    );
                }
            }
        }
        // Non-vacuity: the implication is only interesting if both sides of the
        // gate were actually exercised.
        assert!(failed > 100, "gate never rejected anything ({failed})");
        assert!(passed > 100, "gate never accepted anything ({passed})");
    }

    /// The other half: the gate must not reject anything the original accepts,
    /// or the miner would silently drop real diamonds. `check_diamond_difficulty
    /// == true` implies `gate == true`, which is the contrapositive of the test
    /// above but is asserted directly against the real function so a typo in one
    /// of the two MODIFFBITS tables cannot hide.
    #[test]
    fn the_gate_never_rejects_a_nonce_the_original_accepts() {
        let mut rng = Rng(0xFEED_FACE_CAFE_BEEF);
        let mut accepted = 0usize;
        for number in NUMBERS {
            let gate = DiamondSha3Gate::for_number(number);
            for _ in 0..4000 {
                let sha3hx = rng.easy_hash();
                // An x16rs hash that clears step 2 unconditionally, so step 1 is
                // the only thing that can decide the outcome.
                let x16rshx = [0u8; H32S];
                if check_diamond_difficulty(number, &sha3hx, &x16rshx) {
                    accepted += 1;
                    assert!(
                        gate.passes(&sha3hx),
                        "original accepted number={number} sha3={sha3hx:?} but the gate \
                         rejected it"
                    );
                }
            }
        }
        assert!(accepted > 100, "no accepting case was generated");
    }

    /// With an all-zero x16rs hash step 2 always succeeds, so
    /// `check_diamond_difficulty` collapses to exactly step 1. That makes the
    /// gate not merely sufficient but EQUAL to the block it copies, which is the
    /// tripwire if x16rs/src/diamond.rs ever changes.
    #[test]
    fn the_gate_is_a_verbatim_copy_of_step_one() {
        let mut rng = Rng(0x5EED_1234_ABCD_9876);
        for number in NUMBERS {
            let gate = DiamondSha3Gate::for_number(number);
            for _ in 0..2000 {
                let sha3hx = rng.easy_hash();
                assert_eq!(
                    gate.passes(&sha3hx),
                    check_diamond_difficulty(number, &sha3hx, &[0u8; H32S]),
                    "gate disagrees with step 1 at number={number} sha3={sha3hx:?}"
                );
            }
        }
        // Below 42000 the MODIFFBITS loop is inert and shmaxit is 255, so the
        // gate is free and passes everything, including the worst hash there is.
        assert!(DiamondSha3Gate::for_number(0).passes(&[255u8; H32S]));
        assert!(DiamondSha3Gate::for_number(41_999).passes(&[255u8; H32S]));
        // At 65536 shmaxit drops to 254, so a leading 255 byte is rejected.
        assert!(!DiamondSha3Gate::for_number(65_536).passes(&[255u8; H32S]));
    }

    /// The substitution that rides along in the mining loop: replacing
    /// `check_diamond_hash_result(..).is_some()` with `diamond_name_is_valid`
    /// drops a Vec allocation per attempt. app/src/opencl_dia.rs relies on this
    /// being equivalent; this checks the claim rather than inheriting it.
    ///
    /// The two are NOT equivalent on arbitrary bytes -- `check_diamond_hash_result`
    /// additionally requires the last 6 chars to be in DIAMOND_HASH_BASE_CHARS.
    /// They ARE equivalent on the output of `diamond_hash`, and the argument has
    /// two halves, both asserted below:
    ///   (a) `diamond_hash` only ever emits DIAMOND_HASH_BASE_CHARS, and
    ///   (b) on strings over that alphabet, "not '0'" and "in the alphabet and
    ///       not '0'" are the same predicate.
    #[test]
    fn diamond_name_is_valid_matches_check_diamond_hash_result_on_diamond_hash_output() {
        let mut rng = Rng(0xB16D_1A50_0FF0_1234);
        // (a) alphabet closure of diamond_hash.
        for _ in 0..5000 {
            let dia = diamond_hash(&rng.hash());
            for c in dia.iter() {
                assert!(
                    x16rs::DIAMOND_HASH_BASE_CHARS.contains(c),
                    "diamond_hash emitted {c} which is outside the base alphabet"
                );
            }
            // Agreement on real outputs too (all of these are invalid names at
            // random, but the two must still agree).
            assert_eq!(
                diamond_name_is_valid(&dia),
                check_diamond_hash_result(dia).is_some()
            );
        }
        // (b) agreement over the alphabet, with the leading-zero run forced
        // across every length so the mintable shape (exactly 10) is actually hit.
        let mut valid_hits = 0usize;
        for lead in 0..=16usize {
            for _ in 0..200 {
                let mut dia = [0u8; 16];
                for (i, slot) in dia.iter_mut().enumerate() {
                    *slot = if i < lead {
                        b'0'
                    } else {
                        // index 0 of the base chars is '0'; pick from all 17 so
                        // that a stray '0' after the run is also exercised.
                        x16rs::DIAMOND_HASH_BASE_CHARS[(rng.byte() % 17) as usize]
                    };
                }
                let a = diamond_name_is_valid(&dia);
                assert_eq!(
                    a,
                    check_diamond_hash_result(dia).is_some(),
                    "disagree on {:?}",
                    String::from_utf8_lossy(&dia)
                );
                if a {
                    valid_hits += 1;
                }
            }
        }
        assert!(valid_hits > 0, "no mintable name shape was generated");
        // And the documented difference on bytes that diamond_hash can never
        // produce, so the conditional nature of the equivalence is recorded.
        assert!(diamond_name_is_valid(b"0000000000ABCDEF"));
        assert!(check_diamond_hash_result(b"0000000000ABCDEF").is_none());
    }

    #[test]
    fn zero_pad3_never_slices_past_the_end() {
        // An upstream-supplied target with 30+ leading zero bytes used to panic
        // here and kill the result/submit thread.
        let mut degenerate = [0u8; 32];
        degenerate[31] = 1;
        assert_eq!(hash_left_zero_pad3(&degenerate).len(), 32);
        let mut near_end = [0u8; 32];
        near_end[30] = 1;
        assert_eq!(hash_left_zero_pad3(&near_end).len(), 32);
        assert_eq!(hash_left_zero_pad3(&[0u8; 32]).len(), 3);
        assert!(hash_left_zero_pad3(&[]).is_empty());
        assert_eq!(hash_left_zero_pad3(&[0u8, 1u8]).len(), 2);
    }

    #[test]
    fn zero_pad3_keeps_three_bytes_after_the_first_non_zero() {
        let mut normal = [0u8; 32];
        normal[2] = 9;
        assert_eq!(hash_left_zero_pad3(&normal), vec![0u8, 0u8, 9u8, 0u8, 0u8]);
    }

    const MINTABLE: &[u8; 16] = b"0000000000WTYUIA";
    const OVERSHOOT: &[u8; 16] = b"00000000000TYUIA";
    const WEAK: &[u8; 16] = b"000000000WTYUIAH";

    #[test]
    fn a_valid_diamond_name_is_exactly_ten_zeros_then_six_non_zeros() {
        assert!(diamond_name_is_valid(MINTABLE));
        // 11 leading zeros: "more power" but the node rejects it.
        assert!(!diamond_name_is_valid(OVERSHOOT));
        // 9 leading zeros: below the bar.
        assert!(!diamond_name_is_valid(WEAK));
        assert!(!diamond_name_is_valid(b"0000000000WTYUI"));
        assert!(!diamond_name_is_valid(&[0u8; 16]));
    }

    #[test]
    fn the_ranking_prefers_a_mintable_diamond_over_a_stronger_overshoot() {
        // This is the defect: the raw leading-zero rule ranks the unmintable
        // 11-zero hash above the real diamond, so the GPU work group and the
        // console "best" both report a hash the miner would never submit.
        assert!(diamond_more_power(OVERSHOOT, MINTABLE));
        assert!(!diamond_better(OVERSHOOT, MINTABLE));
        assert!(diamond_better(MINTABLE, OVERSHOOT));
        // Same rule as the kernel: valid beats invalid, otherwise fall back to
        // leading zeros.
        assert!(diamond_better(MINTABLE, WEAK));
        assert!(!diamond_better(WEAK, MINTABLE));
        assert!(diamond_better(OVERSHOOT, WEAK));
        assert!(!diamond_better(MINTABLE, MINTABLE));
    }
}
