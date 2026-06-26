use std::collections::HashSet;

use field::sign_alg;

field::combi_struct! { TransactionType4,
    ty         : Uint1
    timestamp  : Timestamp
    addrlist   : AddrOrList
    fee        : Amount
    actions    : DynListActionW2
    signs      : HybridSignW2
    gas_max    : Uint1
    ano_mark   : Fixed1
}

impl TransactionRead for TransactionType4 {
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
        self.fee.clone()
    }

    fn gas_max_byte(&self) -> Option<u8> {
        Some(*self.gas_max)
    }

    fn fee_purity(&self) -> u64 {
        let txsz = self.size() as u64;
        if txsz == 0 {
            return 0;
        }
        let fee238 = self.fee.to_238_u128().unwrap_or(u128::MAX);
        let purity = fee238 / txsz as u128;
        purity.min(u64::MAX as u128) as u64
    }

    fn action_count(&self) -> usize {
        self.actions.length()
    }

    fn actions(&self) -> &Vec<Box<dyn Action>> {
        self.actions.as_list()
    }

    fn hybrid_signs(&self) -> &Vec<HybridSign> {
        self.signs.as_list()
    }

    fn req_sign(&self) -> Ret<HashSet<Address>> {
        let addrs = &self.addrs();
        let mut adrsets = HashSet::from([self.main()]);
        for act in self.actions() {
            for ptr in act.req_sign() {
                let adr = ptr.real(addrs)?;
                if adr.is_user_signing_address() {
                    adrsets.insert(adr);
                }
            }
        }
        Ok(adrsets)
    }

    fn verify_signature(&self) -> Rerr {
        verify_tx_hybrid_signature(self)
    }
}

impl Transaction for TransactionType4 {
    fn as_read(&self) -> &dyn TransactionRead {
        self
    }

    fn set_fee(&mut self, fee: Amount) {
        self.fee = fee;
    }

    fn fill_hybrid_sign(&mut self, acc: &HybridAccount) -> Ret<HybridSign> {
        let mut fhx = self.hash();
        if acc.address() == self.main().as_bytes() {
            fhx = self.hash_with_fee();
        }
        let signobj = self.create_hybrid_sign_by(acc, &fhx)?;
        self.insert_hybrid_sign(signobj.clone())?;
        Ok(signobj)
    }

    fn push_hybrid_sign(&mut self, signobj: HybridSign) -> Rerr {
        self.insert_hybrid_sign(signobj)
    }

    fn push_action(&mut self, act: Box<dyn Action>) -> Rerr {
        self.actions.push(act)
    }
}

impl TxExec for TransactionType4 {
    fn execute(&self, ctx: &mut dyn Context) -> Rerr {
        do_tx_execute_type4(self, ctx)
    }
}

impl TransactionType4 {
    pub const TYPE: u8 = 4u8;
    pub const MAX_WIRE_SIZE: usize = 256 * 1024;

    pub fn new_by(addr: Address, fee: Amount, ts: u64) -> Self {
        Self {
            ty: Uint1::from(Self::TYPE),
            timestamp: Timestamp::from(ts),
            addrlist: AddrOrList::from_addr(addr),
            fee,
            actions: DynListActionW2::default(),
            signs: HybridSignW2::default(),
            gas_max: Uint1::default(),
            ano_mark: Fixed1::default(),
        }
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

    pub fn create_hybrid_sign_by(&self, acc: &HybridAccount, hash: &Hash) -> Ret<HybridSign> {
        let body = acc.sign_hash(hash.as_array())?;
        let alg = acc.sign_alg_id();
        if self.main().is_pqckey() && alg != sign_alg::MLDSA65 {
            return errf!("PQCKEY main address requires ML-DSA-65 signature alg");
        }
        if self.main().is_hybrid() && alg != sign_alg::HYBRID_SECP_MLDSA65 {
            return errf!("HYBRID main address requires hybrid signature alg");
        }
        let mut signobj = HybridSign::new();
        signobj.alg = Uint1::from(alg);
        signobj.body = BytesW2::from(body)?;
        signobj.check_wire()?;
        Ok(signobj)
    }

    pub fn fill_legacy_secp_hybrid_sign(&mut self, acc: &Account) -> Ret<HybridSign> {
        let mut fhx = self.hash();
        if acc.address() == self.main().as_bytes() {
            fhx = self.hash_with_fee();
        }
        let body = sys::legacy_secp_sign_body(acc, fhx.as_array());
        let mut signobj = HybridSign::new();
        signobj.alg = Uint1::from(sign_alg::LEGACY_SECP);
        signobj.body = BytesW2::from(body)?;
        self.insert_hybrid_sign(signobj.clone())?;
        Ok(signobj)
    }

    fn insert_hybrid_sign(&mut self, signobj: HybridSign) -> Rerr {
        signobj.check_wire()?;
        if self.size() > Self::MAX_WIRE_SIZE {
            return errf!(
                "type 4 transaction wire size {} exceeds cap {}",
                self.size(),
                Self::MAX_WIRE_SIZE
            );
        }
        let plen = self.signs.length();
        if plen >= u16::MAX as usize - 1 {
            return errf!("too many hybrid sign objects");
        }
        let curaddr = hybrid_sign_address(&signobj)?;
        let mut istid = usize::MAX;
        let sglist = self.signs.as_list();
        for i in 0..plen {
            if let Ok(adr) = hybrid_sign_address(&sglist[i]) {
                if adr == curaddr {
                    istid = i;
                    break;
                }
            }
        }
        if istid == usize::MAX {
            self.signs.push(signobj)?;
        } else {
            self.signs.as_mut()[istid] = signobj;
        }
        if let Ok(yes) = verify_target_hybrid_signature(&curaddr, self) {
            if yes {
                return Ok(());
            }
        }
        errf!(
            "address {} hybrid signature verification failed",
            curaddr
        )
    }
}

fn do_tx_execute_type4(tx: &TransactionType4, ctx: &mut dyn Context) -> Rerr {
    let prep = prepare_tx_execute_type4(tx, ctx)?;
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
        ctx.gas_charge(extra9_surcharge(action.extra9(), ret_gas) as i64)?;
    }
    super::tex::do_settlement(ctx)?;
    ctx.run_deferred_phase()?;
    if gas_initialized {
        ctx.gas_refund()?;
    }
    operate::hac_sub(ctx, &prep.main, &prep.fee)?;
    crate::tex::settlement_addr_postsettle_cleanup(ctx);
    Ok(())
}

struct TxExecutePrep4 {
    blkhei: u64,
    txty: u8,
    hx: Hash,
    main: Address,
    fee: Amount,
}

fn prepare_tx_execute_type4(tx: &TransactionType4, ctx: &mut dyn Context) -> Ret<TxExecutePrep4> {
    let env = ctx.env();
    let blkhei = env.block.height;
    crate::upgrade::check_gated_tx(env.chain.id, blkhei, tx.ty())?;
    let not_fast_sync = !env.chain.fast_sync;
    let hx = tx.hash();
    let main = tx.main();
    let fee = tx.fee().clone();
    precheck_tx_actions(tx.ty(), tx.actions())?;
    let state = CoreState::wrap(ctx.state());
    if not_fast_sync {
        if !main.is_pqckey() && !main.is_hybrid() {
            return errf!("tx type 4 main address must be PQCKEY or HYBRID");
        }
        if main.is_privakey_unknown() {
            return errf!(
                "tx main address {} is a system address with unknown private key",
                main
            );
        }
        for adr in tx.addrs() {
            adr.check_version()?;
        }
        if blkhei > 20_0000 {
            fee.check_6_long().map_err(|_| {
                "tx fee size cannot exceed 6 bytes when block height above 200,000".to_string()
            })?;
        }
        if tx.size() > TransactionType4::MAX_WIRE_SIZE {
            return errf!(
                "type 4 transaction wire size {} exceeds cap {}",
                tx.size(),
                TransactionType4::MAX_WIRE_SIZE
            );
        }
        tx.verify_signature()?;
        if let Some(exhei) = state.tx_exist(&hx) {
            return errf!("tx {} already exists in height {}", hx, *exhei);
        }
    }
    Ok(TxExecutePrep4 {
        blkhei,
        txty: tx.ty(),
        hx,
        main,
        fee,
    })
}