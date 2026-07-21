pub struct ContextInst<'a> {
    pub env: Env,
    pub exec_from: ExecFrom,
    pub txr: &'a dyn TransactionRead,
    pub tex_ledger: TexLedger,
    gas: GasCounter,
    log: Box<dyn Logs>,
    sta: Box<dyn State>,
    psh: HashMap<Address, Box<dyn P2sh>>,
    check_sign_cache: HashMap<Address, Ret<bool>>,
    // vm
    vm: Option<Box<dyn VM>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDiag {
    pub exec_from: ExecFrom,
    pub p2sh_count: usize,
    pub gas: GasDiag,
    pub vm_initialized: bool,
}

impl<'a> ContextInst<'a> {
    pub fn new(
        env: Env,
        sta: Box<dyn State>,
        log: Box<dyn Logs>,
        txr: &'a dyn TransactionRead,
    ) -> ContextInst<'a> {
        Self {
            gas: GasCounter::new(),
            env,
            sta,
            log,
            txr,
            exec_from: ExecFrom::Top,
            check_sign_cache: HashMap::new(),
            psh: HashMap::new(),
            tex_ledger: TexLedger::default(),
            vm: None,
        }
    }

    #[inline]
    fn reset_vm_slot(&mut self) {
        self.vm = None;
    }

    pub fn reset_for_new_tx(&mut self, txr: &dyn TransactionRead) {
        self.bind_tx(txr);
        self.reset_tx_runtime_state();
        self.reset_vm_slot();
    }

    pub fn gas_max_charge(&self) -> Ret<Amount> {
        self.gas.max_charge()
    }

    pub fn gas_used_charge(&self) -> Ret<Amount> {
        let price = GasPrice::from_tx(self.txr, self.env.block.height)?;
        self.gas.used_charge(&price)
    }

    pub fn test_set_vm(&mut self, vm: Box<dyn VM>) {
        self.vm = Some(vm);
    }

    pub fn gas_diag(&self) -> GasDiag {
        self.gas.diag()
    }

    pub fn diag(&self) -> ContextDiag {
        ContextDiag {
            exec_from: self.exec_from,
            p2sh_count: self.psh.len(),
            gas: self.gas_diag(),
            vm_initialized: self.vm.is_some(),
        }
    }

    fn vm_unavailable_error(&self) -> String {
        let txty = self.env.tx.ty;
        let gmx = self.txr.gas_max_byte().unwrap_or(0);
        format!(
            "vm not initialized for this tx (tx_type={}, gas_max_byte={})",
            txty, gmx
        )
    }

    fn ensure_vm_assigned(&mut self) -> Rerr {
        if self.vm.is_some() {
            return Ok(());
        }
        let registry = crate::setup::current_setup();
        let assign = registry
            .vm_assigner
            .unwrap_or_else(|| panic!("{}", self.vm_unavailable_error()));
        self.vm = Some(assign(self.env.block.height));
        Ok(())
    }

    #[inline]
    fn vm_mut(&mut self) -> Option<&mut (dyn VM + '_)> {
        match self.vm.as_mut() {
            Some(vm) => Some(vm.as_mut()),
            None => None,
        }
    }

    pub fn release(self) -> (Box<dyn State>, Box<dyn Logs>) {
        (self.sta, self.log)
    }

    #[inline]
    fn debug_assert_tx_bound_consistent(&self) {
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(self.env.tx.ty, self.txr.ty());
            debug_assert_eq!(self.env.tx.main, self.txr.main());
            debug_assert_eq!(self.env.tx.addrs, self.txr.addrs());
        }
    }

    #[inline]
    fn reset_tx_runtime_state(&mut self) {
        self.psh.clear();
        self.check_sign_cache.clear();
        self.tex_ledger = TexLedger::default();
        self.gas.reset();
        self.exec_from = ExecFrom::Top;
    }

    #[inline]
    fn bind_tx(&mut self, txr: &dyn TransactionRead) {
        // SAFETY: `txr` is borrowed from the caller for the entire lifetime of this bound context.
        // We only store it while the context is executing that tx and replace it on the next bind/reset.
        self.txr = unsafe {
            std::mem::transmute::<&dyn TransactionRead, &'static dyn TransactionRead>(txr)
        };
        self.env.replace_tx(create_tx_info(txr));
        self.debug_assert_tx_bound_consistent();
    }

    fn snapshot_volatile_inner(&self) -> Box<dyn Any> {
        Box::new(self.gas.rebated_checkpoint())
    }

    fn restore_volatile_inner(&mut self, snap: Box<dyn Any>) {
        let rebated = *snap
            .downcast::<i64>()
            .expect("gas snapshot type mismatch");
        // On AST rollback, keep gas_charge effects but roll back gas_rebate to avoid refundable-gas replay.
        self.gas.restore_rebated(rebated);
    }

    fn addr_resolve(&self, ptr: &AddrOrPtr) -> Ret<Address> {
        self.debug_assert_tx_bound_consistent();
        ptr.real(&self.env.tx.addrs)
    }

    fn check_sign_cached(&mut self, adr: &Address) -> Rerr {
        self.debug_assert_tx_bound_consistent();
        if let Some(isok) = self.check_sign_cache.get(adr) {
            return isok.clone().map(|_| ());
        }
        if adr.is_privakey_unknown() {
            return errf!(
                "address {} is a system address (value < u32::MAX) with unknown private key",
                adr
            );
        }
        let isok = if self.txr.ty() == crate::transaction::TransactionType4::TYPE {
            if !adr.is_user_signing_address() {
                return errf!("address {} is not a user signing address", adr);
            }
            let res = crate::transaction::verify_target_hybrid_signature(adr, self.txr);
            if res.as_ref().ok().copied() == Some(true) {
                crate::metrics::emit(crate::metrics::PqcMetricEvent::Type4SignVerified {
                    hybrid: adr.is_hybrid(),
                });
            }
            res
        } else {
            adr.must_privakey()?;
            if self.env.chain.fast_sync && self.txr.ty() == TransactionType3::TYPE {
                match self.txr.declared_signer_contains(adr)? {
                    Some(true) => Ok(true),
                    Some(false) => errf!("{} signature verification failed", adr),
                    None => errf!("Type3 declared_signer_contains returned None"),
                }
            } else {
                verify_target_signature(adr, self.txr)
            }
        };
        self.check_sign_cache.insert(*adr, isok.clone());
        isok.map(|_| ())
    }

    fn p2sh_get(&self, adr: &Address) -> Ret<&dyn P2sh> {
        let e = format!("p2sh '{}' not found", adr);
        self.psh.get(adr).map(|boxed| boxed.as_ref()).ok_or(e)
    }

    fn p2sh_insert(&mut self, adr: Address, p2sh: Box<dyn P2sh>) -> Rerr {
        if self.exec_from != ExecFrom::Top {
            return errf!("p2sh_set only allowed in TOP context, got {}", self.exec_from)
        }
        adr.must_scriptmh()?;
        if self.psh.contains_key(&adr) {
            return errf!("p2sh '{}' already proved in current tx", adr);
        }
        self.psh.insert(adr, p2sh);
        Ok(())
    }

}

impl StateOperat for ContextInst<'_> {
    fn state(&mut self) -> &mut dyn State {
        self.sta.as_mut()
    }

    fn state_fork(&mut self) -> Arc<Box<dyn State>> {
        let nil = Box::new(EmptyState {});
        let old: Arc<Box<dyn State>> = std::mem::replace(&mut self.sta, nil).into();
        let sub = old.fork_sub(Arc::downgrade(&old));
        self.sta = sub;
        old
    }

    fn state_merge(&mut self, old: Arc<Box<dyn State>>) {
        let nil = Box::new(EmptyState {});
        let mut sub = std::mem::replace(&mut self.sta, nil);
        sub.detach();
        let mut old = ctx_state_into_box(old);
        old.merge_sub(sub);
        self.sta = old;
    }

    fn state_recover(&mut self, old: Arc<Box<dyn State>>) {
        self.sta.detach();
        self.sta = ctx_state_into_box(old);
    }
}

impl Context for ContextInst<'_> {
    fn action_call(&mut self, k: u16, b: Vec<u8>) -> XRet<(u32, Vec<u8>)> {
        ctx_action_call(self, k, b)
    }

    fn exec_from(&self) -> ExecFrom {
        self.exec_from
    }

    fn exec_from_set(&mut self, src: ExecFrom) {
        self.exec_from = src;
    }

    fn env(&self) -> &Env {
        &self.env
    }

    fn addr(&self, ptr: &AddrOrPtr) -> Ret<Address> {
        self.addr_resolve(ptr)
    }

    fn check_sign(&mut self, adr: &Address) -> Rerr {
        self.check_sign_cached(adr)
    }

    fn tx(&self) -> &dyn TransactionRead {
        self.txr
    }

    fn vm_call(&mut self, req: Box<dyn Any>) -> XRet<(VmGasBuckets, Box<dyn Any>)> {
        self.ensure_vm_assigned()?;
        unsafe {
            let ctx = self as *mut Self;
            let Some(vm) = (*ctx).vm.as_deref_mut() else {
                return xerrf!("vm state invalid after assign")
            };
            // SAFETY: we must re-enter the same VM instance while also passing the same context as host.
            // `vm` is not moved during this call, `ctx` stays at a stable address, and neither reference escapes.
            vm.call(&mut *ctx as &mut dyn Context, req)
        }
    }

    fn vm_current_intent_scope(&mut self) -> Option<Option<usize>> {
        self.vm_mut().and_then(|vm| vm.current_intent_scope())
    }

    fn vm_runtime_config(&mut self) -> Option<Box<dyn Any>> {
        if self.ensure_vm_assigned().is_err() {
            return None;
        }
        self.vm_mut().and_then(|vm| vm.runtime_config())
    }

    fn vm_snapshot_volatile(&mut self) -> Option<Box<dyn Any>> {
        self.vm_mut().map(|vm| vm.snapshot_volatile())
    }

    fn vm_restore_volatile(&mut self, snap: Box<dyn Any>) {
        if let Some(vm) = self.vm_mut() {
            vm.restore_volatile(snap);
        }
    }

    fn vm_rollback_volatile_preserve_warm_and_gas(&mut self) {
        if let Some(vm) = self.vm_mut() {
            vm.rollback_volatile_preserve_warm_and_gas();
        }
    }

    fn vm_invalidate_contract_cache(&mut self, addr: &Address) {
        if let Some(vm) = self.vm_mut() {
            vm.invalidate_contract_cache(addr);
        }
    }

    fn gas_remaining(&self) -> i64 {
        self.gas.remaining()
    }

    fn gas_charge(&mut self, gas: i64) -> Rerr {
        self.gas.charge(gas)
    }

    fn gas_rebate(&mut self, gas: i64) -> Rerr {
        self.gas.rebate(gas)
    }

    fn gas_initialize(&mut self, budget: i64) -> Rerr {
        ContextInst::gas_initialize(self, budget)
    }

    fn gas_refund(&mut self) -> Rerr {
        ContextInst::gas_refund(self)
    }

    fn snapshot_volatile(&self) -> Box<dyn Any> {
        self.snapshot_volatile_inner()
    }

    fn restore_volatile(&mut self, snap: Box<dyn Any>) {
        self.restore_volatile_inner(snap)
    }

    fn tex_ledger(&self) -> &TexLedger {
        &self.tex_ledger
    }

    fn tex_ledger_mut_top(&mut self) -> Ret<&mut TexLedger> {
        if self.exec_from != ExecFrom::Top {
            return errf!("tex ledger write only allowed in TOP context, got {}", self.exec_from)
        }
        Ok(&mut self.tex_ledger)
    }

    fn logs(&mut self) -> &mut dyn Logs {
        self.log.as_mut()
    }

    fn p2sh(&self, adr: &Address) -> Ret<&dyn P2sh> {
        self.p2sh_get(adr)
    }

    fn p2sh_count(&self) -> usize {
        self.psh.len()
    }

    fn p2sh_set(&mut self, adr: Address, p2sh: Box<dyn P2sh>) -> Rerr {
        self.p2sh_insert(adr, p2sh)
    }

    fn run_deferred_phase(&mut self) -> Rerr {
        if self.vm.is_some() {
            unsafe {
                let ctx = self as *mut Self;
                let vm = (*ctx).vm.as_mut().unwrap();
                // SAFETY: deferred replay must use the same live VM instance and the same context object.
                // The VM stays owned by this context for the duration of the call and no alias escapes.
                vm.drain_deferred(&mut *ctx)
            }
        } else {
            Ok(())
        }
    }
}
