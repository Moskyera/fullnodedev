

/* * */
inst_state_define! { VMState,

    201, contract,         ContractAddress  : ContractSto
    202, contract_edition, ContractAddress  : ContractEdition
    205, ctrtkvdb,         ValueKey         : ValueSto
    206, ctrtstatus,       ContractAddress  : StatusSto

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageDebug {
    pub value: Value,
    pub live_blocks: u64,
    pub recover_blocks: u64,
    pub active: bool,
    pub recoverable: bool,
}

/* state storage */
#[allow(dead_code)]
impl VMState<'_> {
    pub fn contract_set_sync_edition(&mut self, addr: &ContractAddress, sto: &ContractSto) {
        self.contract_set(addr, sto);
        self.contract_edition_set(addr, &sto.calc_edition());
    }

    fn status_key_max(cap: &SpaceCap) -> usize {
        VolatileKvLimits::from_space_cap(cap).key_max_bytes
    }

    fn status_contract_addr(cadr: &Address) -> VmrtRes<ContractAddress> {
        ContractAddress::from_addr(*cadr).map_ires(
            StorageError,
            format!(
                "status storage must be in contract address but got {}",
                cadr
            ),
        )
    }

    fn status_key_bytes(cap: &SpaceCap, key: &Value) -> VmrtRes<Vec<u8>> {
        let key = key.extract_key_bytes_with_error_code(StorageKeyInvalid)?;
        let key_max = Self::status_key_max(cap);
        if key.len() > key_max {
            return itr_err_fmt!(
                StorageKeyInvalid,
                "status key too long, max {} bytes but got {}",
                key_max,
                key.len()
            );
        }
        Ok(key)
    }

    fn status_load_by_contract(
        &self,
        cap: &SpaceCap,
        caddr: &ContractAddress,
    ) -> VmrtRes<StatusMap> {
        match self.ctrtstatus(caddr) {
            Some(sto) => {
                let status = sto.to_status_map()?;
                status.validate_key_lengths(Self::status_key_max(cap), StorageError)?;
                Ok(status)
            }
            None => Ok(StatusMap::default()),
        }
    }

    fn status_load(&self, cap: &SpaceCap, cadr: &Address) -> VmrtRes<StatusMap> {
        let caddr = Self::status_contract_addr(cadr)?;
        self.status_load_by_contract(cap, &caddr)
    }

    fn status_save_by_contract(
        &mut self,
        cap: &SpaceCap,
        caddr: &ContractAddress,
        status: &StatusMap,
    ) -> VmrtRes<()> {
        if status.is_empty() {
            self.ctrtstatus_del(caddr);
            return Ok(());
        }
        status.ensure_save_bounds(cap)?;
        let sto = StatusSto::from_status_map(status)
            .map_ires(StorageError, "serialize status object failed".to_owned())?;
        self.ctrtstatus_set(caddr, &sto);
        Ok(())
    }

    fn status_save(&mut self, cap: &SpaceCap, cadr: &Address, status: &StatusMap) -> VmrtRes<()> {
        let caddr = Self::status_contract_addr(cadr)?;
        self.status_save_by_contract(cap, &caddr, status)
    }

    pub(crate) fn status_get_gas(gst: &GasExtra, value: &Value) -> i64 {
        maybe!(matches!(value, Value::Nil), 0i64, gst.status_read(value.val_size()))
    }

    /// Same key/value constraints as [`VMState::sput`] (excluding contract-address check): key ≤ `kv_key_size`,
    /// delete allowed only as `Nil`, otherwise scalar and encoded length ≤ `value_size`.
    /// Returns stored key bytes and value byte length for dynamic gas (`status_write`).
    pub(crate) fn status_put_prepare(
        cap: &SpaceCap,
        key: &Value,
        value: &Value,
    ) -> VmrtRes<(Vec<u8>, usize)> {
        let kbytes = Self::status_key_bytes(cap, key)?;
        let vlen = if matches!(value, Value::Nil) {
            0usize
        } else {
            value.check_scalar()?;
            let vlen = value.extract_bytes_len_with_error_code(StorageValSizeErr)?;
            if !SpaceCap::scalar_field_len_fits(vlen, cap.value_size) {
                let eff_max = cap.value_size.min(SpaceCap::FIELD_BYTES_SERIALIZE_MAX);
                return itr_err_fmt!(
                    StorageValSizeErr,
                    "value too long, max {} bytes but got {}",
                    eff_max,
                    vlen
                );
            }
            vlen
        };
        Ok((kbytes, vlen))
    }

    pub(crate) fn status_put_gas(
        gst: &GasExtra,
        cap: &SpaceCap,
        key: &Value,
        value: &Value,
    ) -> VmrtRes<i64> {
        let (kbytes, vlen) = Self::status_put_prepare(cap, key, value)?;
        Ok(gst.status_write(kbytes.len(), vlen))
    }

    fn sget(&self, cap: &SpaceCap, cadr: &Address, k: &Value) -> VmrtRes<Value> {
        let key = Self::status_key_bytes(cap, k)?;
        Ok(self.status_load(cap, cadr)?.get(&key))
    }

    fn sput(&mut self, cap: &SpaceCap, cadr: &Address, k: Value, v: Value) -> VmrtErr {
        let caddr = Self::status_contract_addr(cadr)?;
        let (key, _vlen) = Self::status_put_prepare(cap, &k, &v)?;
        let mut status = self.status_load_by_contract(cap, &caddr)?;
        status.set_or_remove(key, v)?;
        self.status_save_by_contract(cap, &caddr, &status)
    }

    fn skey(cadr: &Address, key: &Value, key_max: usize) -> VmrtRes<ValueKey> {
        cadr.check_version().map_ires(
            StorageError,
            format!("storage must be in effective address but got {}", cadr),
        )?;
        let k = key.extract_key_bytes_with_error_code(StorageKeyInvalid)?;
        if k.len() > key_max {
            return itr_err_fmt!(
                StorageKeyInvalid,
                "storage key too long, max {} bytes but got {}",
                key_max,
                k.len()
            );
        }
        let mut k = vec![cadr.to_vec(), k].concat();
        if k.len() > Hash::SIZE {
            k = sys::sha3(k).to_vec();
        }
        Ok(ValueKey::from(k))
    }

    fn sfetch(&mut self, curhei: u64, gst: &GasExtra, sk: &ValueKey) -> VmrtRes<Option<ValueSto>> {
        let Some(mut v) = self.ctrtkvdb(sk) else {
            return Ok(None);
        };
        v.settle(curhei, gst)?;
        if v.is_absent() {
            self.ctrtkvdb_del(sk);
            return Ok(None);
        }
        self.ctrtkvdb_set(sk, &v);
        Ok(Some(v))
    }

    fn sstat(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: &Value,
    ) -> VmrtRes<Value> {
        let sk = Self::skey(cadr, k, cap.kv_key_size)?;
        let Some(v) = self.sfetch(curhei, gst, &sk)? else {
            return Ok(Value::Nil);
        };
        let live = v.live_rest_blocks(gst)?;
        let recover = v.recover_rest_blocks(gst)?;
        Value::pack_tuple([Value::U64(live), Value::U64(recover)])
    }

    fn sload(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: &Value,
    ) -> VmrtRes<Value> {
        let sk = Self::skey(cadr, k, cap.kv_key_size)?;
        let Some(v) = self.sfetch(curhei, gst, &sk)? else {
            return Ok(Value::Nil);
        };
        if v.is_recoverable() {
            return itr_err_code!(StorageNotActive);
        }
        Ok(v.data)
    }

    fn snew(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: Value,
        v: Value,
        p: Value,
    ) -> VmrtRes<i64> {
        v.check_non_nil_scalar(StorageNilNotAllowed)?;
        validate_scalar_payload_len(&v, cap.value_size, StorageValSizeErr)?;
        let period = parse_period(p, cap.storage_live_max_periods)?;
        let sk = Self::skey(cadr, &k, cap.kv_key_size)?;
        if self.sfetch(curhei, gst, &sk)?.is_some() {
            return itr_err_code!(StorageKeyExists);
        }
        let unit = ValueSto::unit_for(gst, &v)?;
        let live_credit = period_credit(unit, period, cap.storage_period)?;
        let vobj = ValueSto::new(curhei, v, live_credit, 0)?;
        self.ctrtkvdb_set(&sk, &vobj);
        let gas = gst
            .storage_key_cost
            .saturating_add(u64_to_i64_sat(unit).saturating_mul(period as i64));
        Ok(gas)
    }

    fn sedit(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: Value,
        v: Value,
    ) -> VmrtRes<(i64, i64)> {
        v.check_non_nil_scalar(StorageNilNotAllowed)?;
        validate_scalar_payload_len(&v, cap.value_size, StorageValSizeErr)?;
        let sk = Self::skey(cadr, &k, cap.kv_key_size)?;
        let Some(mut old) = self.sfetch(curhei, gst, &sk)? else {
            return itr_err_code!(StorageKeyNotFind);
        };
        if !old.is_active() {
            return itr_err_code!(StorageNotActive);
        }
        old.data = v;
        old.charge = BlockHeight::from(curhei);
        let unit = ValueSto::unit_for(gst, &old.data)?;
        let live_cap = credit_cap_for_blocks(
            unit,
            cap.storage_live_max_blocks(),
            "live credit cap overflow",
        )?;
        let recover_cap = credit_cap_for_blocks(
            unit,
            cap.storage_recv_max_blocks(),
            "recover credit cap overflow",
        )?;
        let (live_credit, trimmed_live) =
            clamp_credit_to_cap(old.live_credit.uint() as u64, live_cap);
        let (recover_credit, _) =
            clamp_credit_to_cap(old.recover_credit.uint() as u64, recover_cap);
        old.live_credit = Uint4::from(ValueSto::credit_u32(
            live_credit,
            "edit live credit overflow",
        )?);
        old.recover_credit = Uint4::from(ValueSto::credit_u32(
            recover_credit,
            "edit recover credit overflow",
        )?);
        self.ctrtkvdb_set(&sk, &old);
        let fee = u64_to_i64_sat(unit).saturating_mul(gst.storage_edit_mul);
        let rebate = refund_for_live_credit(trimmed_live, cap.storage_period);
        Ok((fee, rebate))
    }

    fn srent(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: Value,
        p: Value,
    ) -> VmrtRes<i64> {
        let period = parse_period(p, cap.storage_live_max_periods)?;
        let sk = Self::skey(cadr, &k, cap.kv_key_size)?;
        let Some(mut v) = self.sfetch(curhei, gst, &sk)? else {
            return itr_err_code!(StorageKeyNotFind);
        };
        let unit = v.unit(gst)?;
        let add_credit = period_credit(unit, period, cap.storage_period)?;
        let add_blocks = period
            .checked_mul(cap.storage_period)
            .ok_or_else(|| ItrErr::new(StorageError, "rent blocks overflow"))?;
        let cur_blocks = rest_blocks(v.live_credit.uint() as u64, unit)?;
        let next_blocks = cur_blocks
            .checked_add(add_blocks)
            .ok_or_else(|| ItrErr::new(StorageError, "rent overflow"))?;
        if next_blocks > cap.storage_live_max_blocks() {
            return itr_err_fmt!(
                StoragePeriodErr,
                "live block budget exceeded, max {} blocks",
                cap.storage_live_max_blocks()
            );
        }
        let next_credit = (v.live_credit.uint() as u64)
            .checked_add(add_credit)
            .ok_or_else(|| ItrErr::new(StorageError, "rent credit overflow"))?;
        v.live_credit = Uint4::from(ValueSto::credit_u32(next_credit, "rent credit overflow")?);
        v.charge = BlockHeight::from(curhei);
        self.ctrtkvdb_set(&sk, &v);
        Ok(u64_to_i64_sat(unit).saturating_mul(period as i64))
    }

    fn srecv(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: Value,
        p: Value,
    ) -> VmrtRes<i64> {
        let period = parse_period(p, cap.storage_recv_max_periods)?;
        let sk = Self::skey(cadr, &k, cap.kv_key_size)?;
        let Some(mut v) = self.sfetch(curhei, gst, &sk)? else {
            return itr_err_code!(StorageKeyNotFind);
        };
        let unit = v.unit(gst)?;
        let add_credit = period_credit(unit, period, cap.storage_period)?;
        let add_blocks = period
            .checked_mul(cap.storage_period)
            .ok_or_else(|| ItrErr::new(StorageError, "recover blocks overflow"))?;
        let cur_blocks = rest_blocks(v.recover_credit.uint() as u64, unit)?;
        let next_blocks = cur_blocks
            .checked_add(add_blocks)
            .ok_or_else(|| ItrErr::new(StorageError, "recover overflow"))?;
        if next_blocks > cap.storage_recv_max_blocks() {
            return itr_err_fmt!(
                StoragePeriodErr,
                "recover block budget exceeded, max {} blocks",
                cap.storage_recv_max_blocks()
            );
        }
        let next_credit = (v.recover_credit.uint() as u64)
            .checked_add(add_credit)
            .ok_or_else(|| ItrErr::new(StorageError, "recover credit overflow"))?;
        v.recover_credit = Uint4::from(ValueSto::credit_u32(
            next_credit,
            "recover credit overflow",
        )?);
        v.charge = BlockHeight::from(curhei);
        self.ctrtkvdb_set(&sk, &v);
        Ok(u64_to_i64_sat(unit)
            .saturating_mul(period as i64)
            .saturating_div(3))
    }

    fn sdel(
        &mut self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: Value,
    ) -> VmrtRes<i64> {
        let sk = Self::skey(cadr, &k, cap.kv_key_size)?;
        let Some(mut v) = self.ctrtkvdb(&sk) else {
            return Ok(0);
        };
        v.settle(curhei, gst)?;
        if v.is_absent() {
            self.ctrtkvdb_del(&sk);
            return Ok(0);
        }
        let refund = refund_for_live_credit(v.live_credit.uint() as u64, cap.storage_period);
        self.ctrtkvdb_del(&sk);
        let refund = refund
            .checked_add(gst.storage_key_cost)
            .ok_or_else(|| ItrErr::new(StorageError, "delete refund overflow"))?;
        Ok(refund)
    }
}

impl VMStateRead<'_> {
    pub fn debug_storage_get(
        &self,
        gst: &GasExtra,
        cap: &SpaceCap,
        curhei: u64,
        cadr: &Address,
        k: &Value,
    ) -> VmrtRes<Option<StorageDebug>> {
        let sk = VMState::skey(cadr, k, cap.kv_key_size)?;
        let Some(mut v) = self.ctrtkvdb(&sk) else {
            return Ok(None);
        };
        v.settle(curhei, gst)?;
        if v.is_absent() {
            return Ok(None);
        }
        let live = v.live_rest_blocks(gst)?;
        let recover = v.recover_rest_blocks(gst)?;
        Ok(Some(StorageDebug {
            value: v.data.clone(),
            live_blocks: live,
            recover_blocks: recover,
            active: v.is_active(),
            recoverable: v.is_recoverable(),
        }))
    }

    pub fn debug_status_get(&self, cap: &SpaceCap, cadr: &Address, k: &Value) -> VmrtRes<Value> {
        let caddr = VMState::status_contract_addr(cadr)?;
        let status = match self.ctrtstatus(&caddr) {
            Some(sto) => {
                let status = sto.to_status_map()?;
                status.validate_key_lengths(VMState::status_key_max(cap), StorageError)?;
                status
            }
            None => StatusMap::default(),
        };
        let key = VMState::status_key_bytes(cap, k)?;
        Ok(status.get(&key))
    }
}
