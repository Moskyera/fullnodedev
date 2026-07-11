field::combi_struct! { TransactionType3,
    ty         : Uint1
    timestamp  : Timestamp
    addrlist   : AddrOrList
    fee        : Amount
    actions    : DynListActionW2
    signs      : SignW2
    gas_max    : Uint1
    ano_mark   : Fixed1
}

impl TransactionRead for TransactionType3 {
    fn hash(&self) -> Hash {
        self.hash_ex(vec![])
    }

    fn hash_with_fee(&self) -> Hash {
        self.hash_ex(self.fee.serialize())
    }

    fn ty(&self) -> u8 {
        *self.ty
    }

    fn main(&self) -> Address {
        self.addrs()[0]
    }

    fn addrs(&self) -> Vec<Address> {
        self.addrlist.to_list()
    }

    fn timestamp(&self) -> &Timestamp {
        &self.timestamp
    }

    fn fee(&self) -> &Amount {
        &self.fee
    }

    fn fee_pay(&self) -> Amount {
        self.fee.clone()
    }

    fn fee_got(&self) -> Amount {
        // Type3 does not use the legacy extra9 fee-split / burn path.
        // Miner-side fee view stays equal to the full transaction fee.
        self.fee.clone()
    }

    fn gas_max_byte(&self) -> Option<u8> {
        Some(*self.gas_max)
    }

    fn fee_purity(&self) -> u64 {
        let Ok(txsz) = self.billing_size() else {
            return 0;
        };
        if txsz == 0 {
            return 0;
        }
        let fee238 = self.fee.to_238_u128().unwrap_or(u128::MAX);
        let purity = fee238 / txsz as u128;
        purity.min(u64::MAX as u128) as u64
    }

    fn billing_size(&self) -> Ret<usize> {
        self.canonical_billing_size()
    }

    fn action_count(&self) -> usize {
        self.actions.length()
    }

    fn actions(&self) -> &Vec<Box<dyn Action>> {
        self.actions.as_list()
    }

    fn signs(&self) -> &Vec<Sign> {
        self.signs.as_list()
    }

    fn req_sign(&self) -> Ret<HashSet<Address>> {
        self.deterministic_signers()
    }

    fn declared_signer_contains(&self, adr: &Address) -> Ret<Option<bool>> {
        Ok(Some(self.deterministic_signers()?.contains(adr)))
    }

    fn verify_signature(&self) -> Rerr {
        verify_type3_signatures_exact(self)
    }
}

impl Transaction for TransactionType3 {
    fn as_read(&self) -> &dyn TransactionRead {
        self
    }

    fn set_fee(&mut self, fee: Amount) {
        self.fee = fee;
    }

    fn fill_sign(&mut self, acc: &Account) -> Ret<Sign> {
        let mut fhx = self.hash();
        if acc.address() == self.main().as_bytes() {
            fhx = self.hash_with_fee();
        }
        let signobj = Sign::create_by(acc, &fhx);
        self.insert_sign(signobj.clone())?;
        Ok(signobj)
    }

    fn push_sign(&mut self, signobj: Sign) -> Rerr {
        self.insert_sign(signobj)
    }

    fn push_action(&mut self, act: Box<dyn Action>) -> Rerr {
        self.actions.push(act)
    }
}

impl TxExec for TransactionType3 {
    fn execute(&self, ctx: &mut dyn Context) -> Rerr {
        do_tx_execute_type3(self, ctx)
    }
}

impl TransactionType3 {
    pub const TYPE: u8 = 3u8;
    pub const SIGN_ITEM_SIZE: usize = 97;

    pub fn new_by(addr: Address, fee: Amount, ts: u64) -> Self {
        Self {
            ty: Uint1::from(Self::TYPE),
            timestamp: Timestamp::from(ts),
            addrlist: AddrOrList::from_addr(addr),
            fee,
            actions: DynListActionW2::default(),
            signs: SignW2::default(),
            gas_max: Uint1::default(),
            ano_mark: Fixed1::default(),
        }
    }

    /// Intrinsic R0: main ∪ static action req_sign, excluding ReqSignList.
    pub fn intrinsic_req_sign(&self) -> Ret<HashSet<Address>> {
        let addrs = &self.addrs();
        let mut adrsets = HashSet::from([self.main()]);
        for act in self.actions() {
            if act.kind() == ReqSignList::KIND {
                continue;
            }
            for ptr in act.req_sign() {
                let adr = ptr.real(addrs)?;
                if adr.is_privakey() {
                    adrsets.insert(adr);
                }
            }
        }
        Ok(adrsets)
    }

    /// Extra signers E from the unique top-level ReqSignList (if any).
    pub fn declared_extra_signers(&self) -> Ret<HashSet<Address>> {
        let addrs = self.addrs();
        let mut found: Option<&ReqSignList> = None;
        for act in self.actions() {
            if let Some(list) = ReqSignList::downcast(act) {
                if found.is_some() {
                    return errf!("ReqSignList must be TOP_GUARD_UNIQUE (duplicate found)");
                }
                found = Some(list);
            }
        }
        match found {
            None => Ok(HashSet::new()),
            Some(list) => list.validate_against(&addrs),
        }
    }

    /// D = R0 ∪ E with overlap and MAX_TYPE3_SIGNERS checks.
    pub fn deterministic_signers(&self) -> Ret<HashSet<Address>> {
        let r0 = self.intrinsic_req_sign()?;
        let e = self.declared_extra_signers()?;
        for adr in &e {
            if r0.contains(adr) {
                return errf!(
                    "ReqSignList address {} overlaps intrinsic req_sign",
                    adr.to_readable()
                );
            }
        }
        let mut d = r0;
        d.extend(e);
        if d.len() > crate::params::MAX_TYPE3_SIGNERS {
            return errf!(
                "Type3 signer count {} exceeds MAX_TYPE3_SIGNERS {}",
                d.len(),
                crate::params::MAX_TYPE3_SIGNERS
            );
        }
        Ok(d)
    }

    pub fn missing_signers(&self) -> Ret<HashSet<Address>> {
        let d = self.deterministic_signers()?;
        let mut present = HashSet::new();
        for sig in self.signs() {
            let adr = Address::from(Account::get_address_by_public_key(*sig.publickey));
            present.insert(adr);
        }
        Ok(d.difference(&present).copied().collect())
    }

    fn canonical_billing_size(&self) -> Ret<usize> {
        let d = self.deterministic_signers()?;
        let base_size = self
            .size()
            .checked_sub(self.signs.size())
            .ok_or_else(|| "Type3 billing size underflow".to_owned())?;
        let sign_item_size = Sign::default().size();
        if sign_item_size != Self::SIGN_ITEM_SIZE {
            return errf!(
                "Type3 Sign encoding size must be {}, got {}",
                Self::SIGN_ITEM_SIZE,
                sign_item_size
            );
        }
        let prefix_size = SignW2::default().size();
        let canonical_signs_size = prefix_size
            .checked_add(
                d.len()
                    .checked_mul(sign_item_size)
                    .ok_or_else(|| "Type3 canonical signs size overflow".to_owned())?,
            )
            .ok_or_else(|| "Type3 canonical signs size overflow".to_owned())?;
        base_size
            .checked_add(canonical_signs_size)
            .ok_or_else(|| "Type3 billing size overflow".to_owned())
    }

    fn hash_ex(&self, adfe: Vec<u8>) -> Hash {
        let mut stuff = Vec::with_capacity(
            self.ty.size()
                + self.timestamp.size()
                + self.addrlist.size()
                + adfe.len()
                + self.actions.size()
                + self.gas_max.size()
                + self.ano_mark.size(),
        );
        self.ty.serialize_to(&mut stuff);
        self.timestamp.serialize_to(&mut stuff);
        self.addrlist.serialize_to(&mut stuff);
        stuff.extend_from_slice(&adfe);
        self.actions.serialize_to(&mut stuff);
        self.gas_max.serialize_to(&mut stuff);
        self.ano_mark.serialize_to(&mut stuff);
        let hx = sys::calculate_hash(stuff);
        Hash::must(&hx[..])
    }

    fn insert_sign(&mut self, signobj: Sign) -> Rerr {
        let plen = self.signs.length();
        if plen >= u16::MAX as usize - 1 {
            return errf!("too many sign objects");
        }
        let curaddr = Address::from(Account::get_address_by_public_key(*signobj.publickey));
        let d = self.deterministic_signers()?;
        if !d.contains(&curaddr) {
            return errf!("undeclared Type3 signer {}", curaddr.to_readable());
        }
        let apbk = signobj.publickey.as_ref();
        let mut istid = usize::MAX;
        let sglist = self.signs.as_list();
        for i in 0..plen {
            let pbk = sglist[i].publickey.as_bytes();
            if apbk == pbk {
                istid = i;
                break;
            }
        }
        if istid == usize::MAX {
            self.signs.push(signobj)?;
        } else {
            self.signs.as_mut()[istid] = signobj;
        }
        if let Ok(yes) = verify_target_signature(&curaddr, self) {
            if yes {
                return Ok(());
            }
        }
        errf!("address {} signature verification failed", curaddr)
    }
}

fn do_tx_execute_type3(tx: &TransactionType3, ctx: &mut dyn Context) -> Rerr {
    let prep = prepare_tx_execute(tx, ctx)?;
    if tx.ano_mark[0] != 0 {
        return errf!("tx type {} ano_mark must be zero", prep.txty);
    }
    mark_tx_exist(ctx, &prep.hx, prep.blkhei);
    {
        let mut state = CoreState::wrap(ctx.state());
        crate::operate::total_add_tx_fee_pay(&mut state, tx)?;
    }
    let gas_initialized = tx_gas_initialize(ctx)?;
    for action in tx.actions() {
        ctx.exec_from_set(ExecFrom::Top);
        let (ret_gas, _retv) = action.execute(ctx)?;
        // Type3 applies extra9 only as a delta-only returned-gas surcharge here;
        // it does not reuse legacy fee-split semantics.
        ctx.gas_charge(extra9_surcharge(action.extra9(), ret_gas) as i64)?;
    }
    super::tex::do_settlement(ctx)?;
    ctx.run_deferred_phase()?;
    // Commit semantics: gas settlement/statistics are committed only on the tx success path.
    // Upper layers roll back failed transaction state, so refund is only executed on success and cannot leave inconsistent state behind.
    if gas_initialized {
        ctx.gas_refund()?;
    }
    operate::hac_sub(ctx, &prep.main, &prep.fee)?;
    // Safety: clear leaked HAC/SAT/Asset on SETTLEMENT_ADDR after all balance operations.
    crate::tex::settlement_addr_postsettle_cleanup(ctx);
    Ok(())
}

// init gas
pub fn tx_gas_initialize(ctx: &mut dyn Context) -> Ret<bool> {
    let tx = ctx.tx();
    let txty = tx.ty();
    let Some(gas_max_byte) = tx.gas_max_byte() else {
        return errf!("tx type {} gas_max must exist", txty);
    };
    let budget = decode_gas_budget(gas_max_byte.min(TX_GAS_BUDGET_CAP_BYTE));
    if budget <= 0 {
        // `gas_max=0` is intentional and means "do not initialize tx gas".
        // This is valid because not every action path consumes gas. Callers must not
        // reinterpret this branch as an invalid transaction; if a later action actually
        // charges gas, the execution path will fail with the normal "gas not initialized"
        // error at the first real gas use.
        return Ok(false);
    }
    ctx.gas_initialize(budget)?;
    Ok(true)
}
