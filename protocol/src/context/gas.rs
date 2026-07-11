// Returned-gas charging in this channel intentionally accounts only for the extra9 delta.
// For current Type3/AST/runtime charge sites, plain actions add no returned-gas charge here by design.
#[inline(always)]
pub(crate) fn extra9_surcharge(extra9: bool, gas: u32) -> u32 {
    maybe!(extra9, gas.saturating_mul(9), 0)
}

/// - `table[i] = floor(138 * 1.07^i)`, `i in 0..=255`.
/// - `table[255] = 4,292,817,207` (fits in `u32`).
/// start from 138 but set it 0
/// Runtime decoding keeps `gas_max=0` as a reserved "no VM gas" value.
/// Therefore `decode_gas_budget(0)=0`, while non-zero bytes use the lookup table.
pub const GAS_BUDGET_LOOKUP_1P07_FROM_138: [u32; 256] = [
    /*0*/ 0, 147, 157, 169, 180, 193, 207, 221, 237, 253, 271, 290, 310, 332, 355, 380,
    /*16*/ 407, 435, 466, 499, 534, 571, 611, 654, 699, 748, 801, 857, 917, 981, 1050, 1124,
    /*32*/ 1202, 1286, 1376, 1473, 1576, 1686, 1804, 1931, 2066, 2211, 2365, 2531, 2708, 2898,
    3101, 3318, /*48*/ 3550, 3799, 4065, 4349, 4654, 4979, 5328, 5701, 6100, 6527, 6984, 7473,
    7996, 8556, 9155, 9796, /*64*/ 10481, 11215, 12000, 12840, 13739, 14701, 15730, 16831,
    18009, 19270, 20619, 22062, 23607, 25259, 27027, 28919, /*80*/ 30944, 33110, 35428, 37908,
    40561, 43401, 46439, 49689, 53168, 56889, 60872, 65133, 69692, 74571, 79791, 85376,
    /*96*/ 91352, 97747, 104589, 111911, 119744, 128126, 137095, 146692, 156961, 167948,
    179704, 192284, 205743, 220146, 235556, 252045, /*112*/ 269688, 288566, 308766, 330379,
    353506, 378251, 404729, 433060, 463374, 495811, 530517, 567654, 607389, 649907, 695400, 744078,
    /*128*/ 796164, 851895, 911528, 975335, 1043608, 1116661, 1194827, 1278465, 1367958,
    1463715, 1566175, 1675807, 1793114, 1918632, 2052936, 2196642, /*144*/ 2350407, 2514935,
    2690980, 2879349, 3080904, 3296567, 3527327, 3774240, 4038436, 4321127, 4623606, 4947258,
    5293566, 5664116, 6060604, 6484847, /*160*/ 6938786, 7424501, 7944216, 8500311, 9095333,
    9732006, 10413247, 11142174, 11922126, 12756675, 13649643, 14605118, 15627476, 16721399,
    17891897, 19144330, /*176*/ 20484433, 21918343, 23452627, 25094311, 26850913, 28730477,
    30741611, 32893523, 35196070, 37659795, 40295981, 43116699, 46134868, 49364309, 52819811,
    56517198, /*192*/ 60473402, 64706540, 69235998, 74082517, 79268294, 84817074, 90754270,
    97107068, 103904563, 111177883, 118960335, 127287558, 136197687, 145731525, 155932732,
    166848023, /*208*/ 178527385, 191024302, 204396003, 218703723, 234012984, 250393893,
    267921466, 286675968, 306743286, 328215316, 351190388, 375773715, 402077876, 430223327,
    460338960, 492562687, /*224*/ 527042075, 563935020, 603410472, 645649205, 690844649,
    739203775, 790948039, 846314402, 905556410, 968945359, 1036771534, 1109345541, 1186999729,
    1270089710, 1358995990, 1454125710, /*240*/ 1555914509, 1664828525, 1781366522,
    1906062178, 2039486531, 2182250588, 2335008129, 2498458698, 2673350807, 2860485364, 3060719339,
    3274969693, 3504217571, 3749512802, 4011978698, 4292817207,
];

#[inline(always)]
pub const fn decode_gas_budget(b: u8) -> i64 {
    GAS_BUDGET_LOOKUP_1P07_FROM_138[b as usize] as i64
}

pub const TX_GAS_BUDGET_CAP_BYTE: u8 = 99; // decode_gas_budget(99) == 111911

#[derive(Clone, Copy)]
struct GasPrice {
    purity_fee: i128,
    purity_size: i128,
}

impl GasPrice {
    fn from_tx(tx: &dyn TransactionRead, height: u64) -> Ret<Self> {
        // Preserve the raw fee/size fraction so gas settle does not lose price precision.
        let raw_fee = tx
            .fee_got()
            .to_238_u128()
            .map_err(|e| format!("tx gas price invalid: {}", e))?;
        let purity_size = tx.billing_size()? as u128;
        let floor = crate::params::vm_lowest_fee_purity(height) as u128;
        let floor_fee = floor
            .checked_mul(purity_size)
            .ok_or_else(|| "tx gas price invalid".to_owned())?;
        let purity_fee = raw_fee.max(floor_fee);
        if purity_fee > i128::MAX as u128 || purity_size > i128::MAX as u128 {
            return errf!("tx gas price invalid");
        }
        let purity_fee = purity_fee as i128;
        let purity_size = purity_size as i128;
        if purity_fee <= 0 || purity_size <= 0 {
            return errf!("tx gas price invalid");
        }
        Ok(Self {
            purity_fee,
            purity_size,
        })
    }
}

#[derive(Clone)]
/// Source of truth for protocol-side gas billing.
///
/// This ledger tracks the tx-scoped gas budget, cumulative charges, and rebates
/// that determine the final HAC burn/refund in `gas_refund()`.
/// VM runtime bucket reporting (`VmGasBuckets`) is not used as the billing source of truth.
struct GasCounter {
    running: bool,
    remaining: i64,
    used: i64,
    rebated: i64,
    max_charge: Amount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GasDiag {
    pub running: bool,
    pub remaining: i64,
    pub used: i64,
    pub rebated: i64,
    pub used_net: i64,
    pub max_charge: Amount,
}

impl Default for GasCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl GasCounter {
    fn new() -> Self {
        Self {
            running: false,
            remaining: 0,
            used: 0,
            rebated: 0,
            max_charge: Amount::zero(),
        }
    }

    fn calc_burn_amount(cost: i64, price: &GasPrice) -> Ret<Amount> {
        if cost <= 0 {
            return errf!("gas cost invalid");
        }
        let num = (cost as i128)
            .checked_mul(price.purity_fee)
            .ok_or_else(|| "gas burn overflow".to_owned())?;
        let den = price.purity_size;
        if den <= 0 {
            return errf!("gas settle denominator invalid");
        }
        let burn = (num + den - 1) / den;
        if burn <= 0 {
            return errf!("gas burn underflow");
        }
        if burn > u64::MAX as i128 {
            return errf!("gas burn overflow");
        }
        Ok(Amount::unit238(burn as u64))
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn remaining(&self) -> i64 {
        self.remaining
    }

    fn diag(&self) -> GasDiag {
        GasDiag {
            running: self.running,
            remaining: self.remaining,
            used: self.used,
            rebated: self.rebated,
            used_net: self.used_net(),
            max_charge: self.max_charge.clone(),
        }
    }

    fn rebated_checkpoint(&self) -> i64 {
        self.rebated
    }

    fn restore_rebated(&mut self, rebated: i64) {
        self.rebated = rebated;
    }

    fn max_charge(&self) -> Ret<Amount> {
        if !self.max_charge.is_positive() {
            return errf!("gas not initialized");
        }
        Ok(self.max_charge.clone())
    }

    #[inline(always)]
    fn used_net(&self) -> i64 {
        let cut = self.rebated.min(self.used);
        self.used - cut
    }

    fn used_charge(&self, price: &GasPrice) -> Ret<Amount> {
        if !self.max_charge.is_positive() {
            return errf!("gas not initialized");
        }
        let used = self.used_net();
        if used <= 0 {
            return Ok(Amount::zero());
        }
        Self::calc_burn_amount(used, price)
    }

    fn begin(&mut self, budget: i64, max_charge: Amount) -> Rerr {
        if budget <= 0 {
            return errf!("gas budget invalid");
        }
        if self.running {
            return errf!("gas already initialized");
        }
        if self.max_charge.is_positive() {
            return errf!("gas already settled");
        }
        self.running = true;
        self.remaining = budget;
        self.used = 0;
        self.rebated = 0;
        self.max_charge = max_charge;
        Ok(())
    }

    fn finalize(&mut self, price: &GasPrice) -> Ret<(Amount, Amount)> {
        if !self.running {
            if self.max_charge.is_positive() {
                return errf!("gas already settled");
            }
            return errf!("gas not initialized");
        }
        let used_charge = self.used_charge(price)?;
        let refund = self.max_charge.sub_mode_u128(&used_charge)?;
        self.running = false;
        Ok((refund, used_charge))
    }

    fn charge(&mut self, gas: i64) -> Rerr {
        if gas < 0 {
            return errf!("gas cost invalid");
        }
        if gas == 0 {
            return Ok(()); // do nothing
        }
        if !self.running {
            return maybe!(
                self.max_charge.is_positive(),
                errf!("gas already settled"),
                errf!("gas not initialized")
            );
        }
        let Some(next) = self.remaining.checked_sub(gas) else {
            return errf!("gas has run out");
        };
        if next < 0 {
            return errf!("gas has run out");
        }
        self.remaining = next;
        self.used = self
            .used
            .checked_add(gas)
            .ok_or_else(|| "gas has run out".to_owned())?;
        Ok(())
    }

    fn rebate(&mut self, gas: i64) -> Rerr {
        if gas < 0 {
            return errf!("gas refund invalid");
        }
        if !self.running {
            return maybe!(
                self.max_charge.is_positive(),
                errf!("gas already settled"),
                errf!("gas not initialized")
            );
        }
        if gas == 0 {
            return Ok(());
        }
        self.rebated = self
            .rebated
            .checked_add(gas)
            .ok_or_else(|| "gas refund overflow".to_owned())?;
        Ok(())
    }
}

impl ContextInst<'_> {
    fn gas_initialize(&mut self, budget: i64) -> Rerr {
        if self.gas.running {
            return errf!("gas already initialized");
        }
        if self.gas.max_charge.is_positive() {
            return errf!("gas already settled");
        }
        if budget <= 0 {
            return errf!("gas budget invalid");
        }
        let price = GasPrice::from_tx(self.tx(), self.env().block.height)?;
        let cap = decode_gas_budget(TX_GAS_BUDGET_CAP_BYTE);
        let budget = budget.min(cap);
        let max_burn_amt = GasCounter::calc_burn_amount(budget, &price)?;
        let main = self.env().tx.main;
        crate::operate::hac_sub(self, &main, &max_burn_amt)?;
        self.gas.begin(budget, max_burn_amt)
    }

    /// Finalize protocol-side gas settlement for a successfully committing transaction.
    ///
    /// Source of truth:
    /// - final HAC burn/refund is derived from `self.gas.used_net()` in `GasCounter`
    /// - not from VM runtime bucket reporting (`VmGasBuckets`) returned by VM entry calls
    /// - `total_count.ast_vm_gas_burn_238` below is committed statistics only
    ///
    /// Commit semantics:
    /// - this runs only on the tx success path
    /// - if upper layers roll back the transaction, neither the refund nor the statistics update commit
    pub fn gas_refund(&mut self) -> Rerr {
        let price = GasPrice::from_tx(self.tx(), self.env().block.height)?;
        let (refund, used_charge) = self.gas.finalize(&price)?;
        if refund.is_positive() {
            // do refund
            let main = self.env().tx.main;
            crate::operate::hac_add(self, &main, &refund)?;
        }
        if !used_charge.is_positive() {
            return Ok(());
        }
        let used_238 = used_charge.to_238_u64()?;
        if used_238 == 0 {
            return Ok(());
        }
        // add count
        let mut state = crate::state::CoreState::wrap(self.state());
        crate::operate::with_total_count(&mut state, |ttcount| {
            crate::operate::total_add_u12(
                &mut ttcount.ast_vm_gas_burn_238,
                used_238 as u128,
                "ast_vm_gas_burn_238",
            )
        })?;
        Ok(())
    }
}
