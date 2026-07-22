// Long-lived protocol economic parameters.
//
// Keep temporary upgrade gates in `upgrade.rs`; this module is for consensus
// parameters that remain part of protocol execution after upgrade gates are gone.

/*
    Permanent storage pricing reference:
    - 0.0002 HAC / 200 bytes = 0.000001 HAC per byte
    - 1600 bytes * 10000 periods ~= 8 HAC total permanent protocol cost
    - 10000 periods ~= 9.51 years when one period = 100 blocks
*/
pub const CONTRACT_STORE_PERM_PERIODS: u64 = 10_000;

/*
    Minimum VM fee purity floor, in unit-238 per tx byte.
    50000:238 == 100:244 == 0.000005 HAC per byte.
*/
pub const VM_LOWEST_FEE_PURITY: u64 = 50_000;

/*
    Maximum number of deterministic Type3 signers (|D| = |R0 ∪ E|).
    Fixed consensus parameter.
*/
pub const MAX_TYPE3_SIGNERS: usize = 200;

// Future consensus changes can lower the VM fee purity floor by appending
// `(activation_height, new_floor)` here. The first value intentionally has no
// activation height: tx type3 is already gated by the upgrade gate, so this
// default starts applying when type3 itself becomes valid.
pub const VM_LOWEST_FEE_PURITY_REDUCTIONS: &[(u64, u64)] = &[];

#[inline]
pub fn vm_lowest_fee_purity(height: u64) -> u64 {
    let mut purity = VM_LOWEST_FEE_PURITY;
    for (activation_height, next_purity) in VM_LOWEST_FEE_PURITY_REDUCTIONS {
        if height >= *activation_height && *next_purity < purity {
            purity = *next_purity;
        }
    }
    purity
}

#[inline]
pub fn vm_effective_fee_purity(height: u64, raw_fee_purity: u64) -> u64 {
    let floor = vm_lowest_fee_purity(height);
    if raw_fee_purity < floor {
        floor
    } else {
        raw_fee_purity
    }
}
