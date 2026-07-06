
use protocol::operate::*;

macro_rules! vmsto { ($ctx: expr) => {
    VMState::wrap($ctx.state())
}}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractStoreAnalysis {
    pub address: ContractAddress,
    pub contract_size: usize,
    pub inherit_count: usize,
    pub library_count: usize,
    pub has_construct: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractUpdateAnalysis {
    pub address: ContractAddress,
    pub old_contract_size: usize,
    pub new_contract_size: usize,
    pub edit_size: usize,
    pub did_structural_change: bool,
    pub did_effective_lookup_change: bool,
    pub update_hook: AbstCall,
    pub required_protocol_cost: Amount,
}


action_define! { ContractDeploy, 40,
    ActScope::TOP_ONLY_CAN_WITH_GUARD, 3, false, [],
    {
        protocol_cost: Amount
        nonce: Uint4
        construct_argv: BytesW2 // checked by SpaceCap::value_size at runtime
        _marks_:   Fixed4 // zero
        contract: ContractSto
    },
    (self, {
        format!("Deploy smart contract with nonce {}", *self.nonce)
    }),
    (self, ctx, _gas {
        if self._marks_.not_zero() { // reserved marks must stay zero
            return xerrf!("marks bytes invalid")
        }
        let hei = ctx.env().block.height;
        let (gst, cap) = peek_vm_runtime_limits(ctx, hei);
        let maddr = ctx.env().tx.main;
        // check contract
        let caddr = ContractAddress::calculate(&maddr, &self.nonce);
        if vmsto!(ctx).contract_exist(&caddr) {
            return xerrf!("contract {} already exists", (*caddr).to_readable())
        }
        // check
        self.contract.check(hei, &cap, &gst)?;
        if self.contract.metas.revision.uint() != 0 {
            return xerrf!("contract revision must be 0 on deploy")
        }
        let has_construct = precheck_contract_store(&caddr, &self.contract, &gst, ctx)?;
        let cargv = self.construct_argv.to_vec();
        if cargv.len() > cap.value_size {
            return xerrf!("construct argv size overflow")
        }
        if !has_construct && !cargv.is_empty() {
            return xerrf!("construct argv provided but Construct hook not found")
        }
        if self.contract.size() == 0 {
            return xerrf!("contract content cannot be empty");
        }
        let charge_bytes = self.contract.size();
        // spend protocol fee
        check_sub_contract_protocol_cost(
            ctx,
            &self.protocol_cost,
            charge_bytes,
            protocol::params::CONTRACT_STORE_PERM_PERIODS,
        )?;
        if self.protocol_cost.is_positive() {
            let mut state = CoreState::wrap(ctx.state());
            with_total_count(&mut state, |ttcount| {
                total_add_amount_238(
                    &mut ttcount.contract_protocol_cost_burn_238,
                    &self.protocol_cost,
                    "contract_protocol_cost_burn_238",
                )?;
                total_add_u8(
                    &mut ttcount.contract_deploy_count,
                    1,
                    "contract_deploy_count",
                )?;
                total_add_u12(
                    &mut ttcount.contract_charge_bytes_total,
                    charge_bytes as u128,
                    "contract_charge_bytes_total",
                )?;
                Ok(())
            })?;
        } else {
            let mut state = CoreState::wrap(ctx.state());
            with_total_count(&mut state, |ttcount| {
                total_add_u8(
                    &mut ttcount.contract_deploy_count,
                    1,
                    "contract_deploy_count",
                )
            })?;
        }
        // save the contract first; tx-level rollback owns final unwind if Construct fails.
        vmsto!(ctx).contract_set_sync_edition(&caddr, &self.contract);
        if has_construct {
            let _ = run_abst_entry(
                ctx,
                AbstCall::Construct,
                caddr.to_addr(),
                Value::Bytes(cargv),
            )?;
        }
        // ok finish
        Ok(vec![])
    })
}

action_define! { ContractUpdate, 41,
    ActScope::TOP_ONLY_CAN_WITH_GUARD, 3, false, [],
    {
        protocol_cost: Amount
        address: Address // contract address
        _marks_: Fixed2 // zero
        edit: ContractEdit
    },
    (self, format!("Update smart contract {}", self.address)),
    (self, ctx, _gas {
        use AbstCall::*;
        if self._marks_.not_zero() {
            return xerrf!("marks bytes invalid")
        }
        let hei = ctx.env().block.height;
        let (gst, cap) = peek_vm_runtime_limits(ctx, hei);
        // load old
        let caddr = ContractAddress::from_addr(self.address)?;
        let Some(contract) = vmsto!(ctx).contract(&caddr) else {
            return xerrf!("contract {} does not exist", (*caddr).to_readable())
        };
        // apply edit (in memory)
        let mut new_contract = contract.clone();
        let did_structural_change = new_contract.apply_edit(&self.edit, hei, &cap, &gst)?;
        let _ = precheck_contract_store(&caddr, &new_contract, &gst, ctx)?;
        if new_contract.size() == 0 {
            return xerrf!("contract content cannot be empty");
        }
        let did_effective_lookup_change = effective_userfn_lookup_changed(
            &mut vmsto!(ctx),
            &caddr,
            &contract,
            &new_contract,
        )?;
        // Final dispatch is driven by whether any existing visible selector semantics changed.
        // Purely additive edits (e.g. inherit/library append, or new local funcs with no shadowing)
        // stay Append; structural replacements or selector-owner changes are Change.
        let is_change = did_structural_change || did_effective_lookup_change;
        // Modification tax: charge the edit payload at perm periods (edit.size() >= chain delta).
        let edit_bytes = self.edit.size();
        let edit_periods = protocol::params::CONTRACT_STORE_PERM_PERIODS;
        let total_fee = calc_contract_protocol_cost_min_with_periods(ctx, edit_bytes, edit_periods)?;
        let pcost = &self.protocol_cost;
        if pcost.is_negative() {
            return xerrf!("protocol fee cannot be negative");
        }
        if *pcost < total_fee {
            return xerrf!(
                "protocol fee must be at least {} (edit_bytes={}, edit_periods={}) but got {}",
                &total_fee,
                edit_bytes,
                edit_periods,
                &self.protocol_cost
            );
        }
        if !pcost.is_zero() {
            let maddr = ctx.env().tx.main;
            operate::hac_sub(ctx, &maddr, pcost)?;
        }
        let mut state = CoreState::wrap(ctx.state());
        with_total_count(&mut state, |ttcount| {
            total_add_u8(
                &mut ttcount.contract_update_count,
                1,
                "contract_update_count",
            )?;
            total_add_u12(
                &mut ttcount.contract_charge_bytes_total,
                edit_bytes as u128,
                "contract_charge_bytes_total",
            )?;
            if pcost.is_positive() {
                total_add_amount_238(
                    &mut ttcount.contract_protocol_cost_burn_238,
                    pcost,
                    "contract_protocol_cost_burn_238",
                )?;
            }
            Ok(())
        })?;
        let sys = maybe!(is_change, Change, Append); // Change or Append
        // Authorization is intentionally delegated to the current contract's Change/Append hook.
        // Run the selected hook on the current on-chain contract; failure means the update is not allowed.
        let _ = run_abst_entry(ctx, sys, caddr.to_addr(), Value::Nil)?;
        // save the new
        vmsto!(ctx).contract_set_sync_edition(&caddr, &new_contract);
        let caddr_real = caddr.to_addr();
        ctx.vm_invalidate_contract_cache(&caddr_real);
        Ok(vec![])
    })
}

/**************************************/

fn check_contract_self_reference(root_addr: &ContractAddress, root_contract: &ContractSto) -> Rerr {
    macro_rules! any_same { ($key: ident) => {
        root_contract.$key.as_list().iter().any(|a| a == root_addr)
    }}
    if any_same!(inherit) {
        return errf!("contract cannot inherit itself {}", root_addr.to_readable())
    }
    if any_same!(library) {
        return errf!("contract cannot link itself as library {}", root_addr.to_readable())
    }
    Ok(())
}

fn precheck_contract_store(
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    gst: &GasExtra,
    ctx: &mut dyn Context,
) -> Ret<bool> {
    Ok(analyze_contract_store(ctx, root_addr, root_contract, gst)?.has_construct)
}

pub fn analyze_contract_store(
    ctx: &mut dyn Context,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    gst: &GasExtra,
) -> Ret<ContractStoreAnalysis> {
    check_contract_self_reference(root_addr, root_contract)?;
    let mut vmsta = VMState::wrap(ctx.state());
    check_link_contracts_exist(&mut vmsta, root_addr, root_contract)?;
    check_inherits_direct_parents_flat(&mut vmsta, root_addr, root_contract)?;
    let has_construct = detect_effective_abst_presence(
        &mut vmsta,
        root_addr,
        root_contract,
        AbstCall::Construct,
    )?;
    check_static_call_targets(&mut vmsta, root_addr, root_contract, gst)?;
    Ok(ContractStoreAnalysis {
        address: root_addr.clone(),
        contract_size: root_contract.size(),
        inherit_count: root_contract.inherit.length(),
        library_count: root_contract.library.length(),
        has_construct,
    })
}

pub fn analyze_contract_update(
    ctx: &mut dyn Context,
    address: &ContractAddress,
    edit: &ContractEdit,
) -> Ret<ContractUpdateAnalysis> {
    use AbstCall::*;
    let hei = ctx.env().block.height;
    let (gst, cap) = peek_vm_runtime_limits(ctx, hei);
    let Some(contract) = VMState::wrap(ctx.state()).contract(address) else {
        return errf!("contract {} does not exist", address.to_readable())
    };
    let mut new_contract = contract.clone();
    let did_structural_change = new_contract.apply_edit(edit, hei, &cap, &gst)?;
    let _ = analyze_contract_store(ctx, address, &new_contract, &gst)?;
    if new_contract.size() == 0 {
        return errf!("contract content cannot be empty");
    }
    let did_effective_lookup_change = effective_userfn_lookup_changed(
        &mut VMState::wrap(ctx.state()),
        address,
        &contract,
        &new_contract,
    )?;
    let is_change = did_structural_change || did_effective_lookup_change;
    let edit_size = edit.size();
    Ok(ContractUpdateAnalysis {
        address: address.clone(),
        old_contract_size: contract.size(),
        new_contract_size: new_contract.size(),
        edit_size,
        did_structural_change,
        did_effective_lookup_change,
        update_hook: maybe!(is_change, Change, Append),
        required_protocol_cost: calc_contract_protocol_cost_min_with_periods(
            ctx,
            edit_size,
            protocol::params::CONTRACT_STORE_PERM_PERIODS,
        )?,
    })
}

fn load_contract_for_check(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    addr: &ContractAddress,
    role: &str,
) -> Ret<ContractSto> {
    if addr == root_addr {
        return Ok(root_contract.clone());
    }
    match vmsta.contract(addr) {
        Some(c) => Ok(c),
        None => errf!("{} contract {} does not exist", role, addr.to_readable()),
    }
}

fn detect_effective_abst_presence(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    abst: AbstCall,
) -> Ret<bool> {
    if root_contract.have_abst_call(abst) {
        return Ok(true);
    }
    for parent in root_contract.inherit.as_list() {
        let sto = load_contract_for_check(vmsta, root_addr, root_contract, parent, "inherit")?;
        if sto.have_abst_call(abst) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_link_contracts_exist(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
) -> Rerr {
    for a in root_contract.library.as_list() {
        let _ = load_contract_for_check(vmsta, root_addr, root_contract, a, "library")?;
    }
    for a in root_contract.inherit.as_list() {
        let _ = load_contract_for_check(vmsta, root_addr, root_contract, a, "inherit")?;
    }
    Ok(())
}

fn check_inherits_direct_parents_flat(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
) -> Rerr {
    for p in root_contract.inherit.as_list() {
        let sto = load_contract_for_check(vmsta, root_addr, root_contract, p, "inherit")?;
        if sto.inherit.length() > 0 {
            return errf!(
                "inherit parent {} cannot have parent inherit",
                p.to_readable()
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct UserfnMeta {
    is_external: bool,
}

fn contract_userfn_meta(contract: &ContractSto, sign: &FnSign) -> Option<UserfnMeta> {
    let f = contract
        .userfuncs
        .as_list()
        .iter()
        .find(|f| f.sign.to_array() == *sign)?;
    let ext_mark = FnConf::External as u8;
    Some(UserfnMeta {
        is_external: f.fncnf[0] & ext_mark == ext_mark,
    })
}

fn collect_effective_userfn_owners(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
) -> Ret<std::collections::HashMap<FnSign, ContractAddress>> {
    let mut owners = std::collections::HashMap::new();
    for f in root_contract.userfuncs.as_list() {
        owners.entry(f.sign.to_array()).or_insert(root_addr.clone());
    }
    for parent in root_contract.inherit.as_list() {
        let sto = load_contract_for_check(vmsta, root_addr, root_contract, parent, "inherit")?;
        for f in sto.userfuncs.as_list() {
            owners.entry(f.sign.to_array()).or_insert(parent.clone());
        }
    }
    Ok(owners)
}

fn effective_userfn_lookup_changed(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    old_contract: &ContractSto,
    new_contract: &ContractSto,
) -> Ret<bool> {
    let old_table = collect_effective_userfn_owners(vmsta, root_addr, old_contract)?;
    let new_table = collect_effective_userfn_owners(vmsta, root_addr, new_contract)?;
    for (sign, old_owner) in old_table {
        match new_table.get(&sign) {
            Some(new_owner) if new_owner == &old_owner => {}
            _ => return Ok(true),
        }
    }
    Ok(false)
}

fn scan_call_sites(codes: &[u8], mut check: impl FnMut(Bytecode, &[u8]) -> Rerr) -> Rerr {
    let mut i = 0usize;
    while i < codes.len() {
        let inst: Bytecode = std_mem_transmute!(codes[i]);
        let meta = inst.metadata();
        if !meta.valid {
            return errf!("invalid bytecode {}", codes[i]);
        }
        i += 1;
        let pms = meta.param as usize;
        if i + pms > codes.len() {
            return errf!("instruction param overflow at {}", i - 1);
        }
        let params = &codes[i..i + pms];
        match inst {
            _ if is_user_call_inst(inst) => {
                check(inst, params)?;
            }
            Bytecode::PBUF => {
                let l = params[0] as usize;
                if i + pms + l > codes.len() {
                    return errf!("PBUF overflow at {}", i - 1);
                }
                i += l;
            }
            Bytecode::PBUFL => {
                let l = u16::from_be_bytes([params[0], params[1]]) as usize;
                if i + pms + l > codes.len() {
                    return errf!("PBUFL overflow at {}", i - 1);
                }
                i += l;
            }
            _ => {}
        }
        i += pms;
    }
    Ok(())
}

fn resolve_userfn_meta_on_owner(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    owner: &ContractAddress,
    sign: &FnSign,
) -> Ret<Option<(ContractAddress, UserfnMeta)>> {
    let sto = load_contract_for_check(vmsta, root_addr, root_contract, owner, "lookup")?;
    Ok(contract_userfn_meta(&sto, sign).map(|meta| (owner.clone(), meta)))
}

fn resolve_lookup_anchor_for_check(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    func_tag: &str,
    call: &CallSpec,
) -> Ret<ContractAddress> {
    let lib_addrs: Vec<Address> = root_contract
        .library
        .as_list()
        .iter()
        .map(|a| a.to_addr())
        .collect();
    // Static precheck binds `this` to the contract being stored, so `this.*` must not be purely virtual: a default implementation must already exist on self or an inherited parent.
    let anchor = call
        .resolve_anchor_from(Some(root_addr), Some(root_addr), &lib_addrs)
        .map_err(|e| format!("{}: {}", func_tag, e))?;
    if call.lib_index().is_some() {
        let _ = load_contract_for_check(vmsta, root_addr, root_contract, &anchor, "lookup")?;
    }
    Ok(anchor)
}

fn resolve_lookup_entries_for_check(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    anchor: &ContractAddress,
    call: &CallSpec,
) -> Ret<Vec<ContractAddress>> {
    let parents = if call.needs_inherit_chain() {
        load_contract_for_check(vmsta, root_addr, root_contract, anchor, "inherit")?
            .inherit
            .as_list()
            .to_vec()
    } else {
        vec![]
    };
    Ok(call.resolve_candidates(anchor, &parents))
}

fn resolve_userfn_meta_by_lookup_for_check(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    func_tag: &str,
    call: &CallSpec,
    sign: &FnSign,
) -> Ret<Option<(ContractAddress, UserfnMeta)>> {
    let anchor = resolve_lookup_anchor_for_check(vmsta, root_addr, root_contract, func_tag, call)?;
    let entries = resolve_lookup_entries_for_check(vmsta, root_addr, root_contract, &anchor, call)?;
    for owner in entries {
        if let Some(hit) =
            resolve_userfn_meta_on_owner(vmsta, root_addr, root_contract, &owner, sign)?
        {
            return Ok(Some(hit));
        }
    }
    Ok(None)
}

fn check_static_call_targets(
    vmsta: &mut VMState,
    root_addr: &ContractAddress,
    root_contract: &ContractSto,
    gst: &GasExtra,
) -> Rerr {
    let check_one = |func_tag: String, codes: &[u8], vmsta: &mut VMState| -> Rerr {
        let check_call = |call: CallSpec, vmsta: &mut VMState| -> Rerr {
            let sign = call.selector();
            let found = resolve_userfn_meta_by_lookup_for_check(
                vmsta,
                root_addr,
                root_contract,
                &func_tag,
                &call,
                &sign,
            )?;
            let Some((owner, meta)) = found else {
                return errf!(
                    "{}: call target function 0x{} not found",
                    func_tag,
                    hex::encode(sign)
                );
            };
            if call.requires_external_visibility() && !meta.is_external {
                return errf!(
                    "{}: target function 0x{} resolved in {} is not external",
                    func_tag,
                    hex::encode(sign),
                    owner.to_readable()
                );
            }
            Ok(())
        };
        scan_call_sites(codes, |inst, params| {
            check_call(
                decode_user_call_site(inst, params).map_err(|e| e.to_string())?,
                vmsta,
            )
        })
    };

    for f in root_contract.userfuncs.as_list() {
        let code_pkg = CodePkg::try_from(&f.code_stuff).map_err(|e| e.to_string())?;
        let ctype = code_pkg.code_type().map_err(|e| e.to_string())?;
        let codes = match ctype {
            CodeType::Bytecode => code_pkg.data,
            CodeType::IRNode => {
                runtime_irs_to_bytecodes(&code_pkg.data, gst).map_err(|e| e.to_string())?
            }
        };
        let tag = format!("userfn 0x{}", hex::encode(f.sign.to_array()));
        check_one(tag, &codes, vmsta)?;
    }

    for f in root_contract.abstcalls.as_list() {
        let code_pkg = CodePkg::try_from(&f.code_stuff).map_err(|e| e.to_string())?;
        let ctype = code_pkg.code_type().map_err(|e| e.to_string())?;
        let codes = match ctype {
            CodeType::Bytecode => code_pkg.data,
            CodeType::IRNode => {
                runtime_irs_to_bytecodes(&code_pkg.data, gst).map_err(|e| e.to_string())?
            }
        };
        let tag = format!("abstcall {}", f.sign[0]);
        check_one(tag, &codes, vmsta)?;
    }

    Ok(())
}

fn check_sub_contract_protocol_cost(
    ctx: &mut dyn Context,
    pfee: &Amount,
    charge_bytes: usize,
    periods: u64,
) -> Rerr {
    if pfee.is_negative() {
        return errf!("protocol fee cannot be negative");
    }
    if charge_bytes == 0 {
        return Ok(());
    }
    let min_fee = calc_contract_protocol_cost_min_with_periods(ctx, charge_bytes, periods)?;
    let maddr = ctx.env().tx.main;
    if pfee < &min_fee {
        return errf!(
            "protocol fee must be at least {} (bytes={}, periods={}) but got {}",
            &min_fee,
            charge_bytes,
            periods,
            pfee
        );
    }
    operate::hac_sub(ctx, &maddr, pfee)?;
    Ok(())
}

fn calc_contract_protocol_cost_min_with_periods(
    ctx: &dyn Context,
    charge_bytes: usize,
    periods: u64,
) -> Ret<Amount> {
    if charge_bytes == 0 {
        return Ok(Amount::zero());
    }
    let fee_purity = protocol::params::vm_effective_fee_purity(
        ctx.env().block.height,
        ctx.tx().fee_purity(),
    ) as u128; // unit-238 per tx byte
    let periods = periods as u128;
    if periods == 0 || fee_purity == 0 {
        return errf!(
            "contract protocol fee calculate failed: periods={} fee_purity={}",
            periods,
            fee_purity
        );
    }
    let bytes = charge_bytes as u128;
    let Some(need) = fee_purity.checked_mul(bytes) else {
        return errf!(
            "contract protocol fee calculate failed: fee_purity * bytes overflow ({} * {})",
            fee_purity,
            bytes
        );
    };
    let Some(need) = need.checked_mul(periods) else {
        return errf!(
            "contract protocol fee calculate failed: required * periods overflow ({} * {})",
            need,
            periods
        );
    };
    Ok(Amount::coin_u128(need, UNIT_238))
}

/// Minimum on-chain `protocol_cost` for `charge_bytes` stored `periods` times.
pub fn contract_protocol_cost_min(
    ctx: &dyn Context,
    charge_bytes: usize,
    periods: u64,
) -> Ret<Amount> {
    calc_contract_protocol_cost_min_with_periods(ctx, charge_bytes, periods)
}

#[cfg(test)]
mod contract_test {
    use super::*;
    use crate::contract::{Abst, Contract, Func};
    use crate::rt::Bytecode;
    use basis::component::Env;
    use basis::interface::ActExec;
    use field::{Address, Amount, Uint4};
    use protocol::context::decode_gas_budget;
    use protocol::transaction::TransactionType3;
    use std::cell::RefCell;
    use testkit::sim::context::make_ctx_with_state;
    use testkit::sim::state::FlatMemState as StateMem;
    use testkit::sim::tx::StubTxBuilder;

    fn test_main_addr() -> Address {
        Address::from_readable("1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9").unwrap()
    }

    fn test_contract(base: &Address, nonce: u32) -> ContractAddress {
        ContractAddress::calculate(base, &Uint4::from(nonce))
    }

    thread_local! {
        static VM_TEST_SETUP_SCOPE: RefCell<Option<protocol::setup::TestSetupScopeGuard>> = const { RefCell::new(None) };
    }

    fn init_vm_assigner_once() {
        let mut setup = protocol::setup::new_standard_protocol_setup(|_, stuff| sys::calculate_hash(stuff));
        mint::setup::register_protocol_extensions(&mut setup);
        crate::setup::register_protocol_extensions(&mut setup);
        let guard = protocol::setup::install_test_scope(setup);
        VM_TEST_SETUP_SCOPE.with(|cell| {
            *cell.borrow_mut() = Some(guard);
        });
    }

    fn run_deploy_with_preloaded(
        nonce: u32,
        construct_argv: Vec<u8>,
        preload: Vec<(ContractAddress, ContractSto)>,
        deploy_contract: ContractSto,
    ) -> Ret<()> {
        init_vm_assigner_once();
        let main = test_main_addr();
        let tx = StubTxBuilder::new()
            .ty(TransactionType3::TYPE)
            .main(main)
            .addrs(vec![main])
            .fee(Amount::unit238(1_000_000))
            .gas_max(17)
            .tx_size(128)
            .fee_purity(1)
            .build();

        let mut env = Env::default();
        env.block.height = 1;
        env.chain.id = 1; // non-mainnet: bypasses online-upgrade height gating in tests
        env.chain.fast_sync = true; // skip action-level check in direct action tests
        env.tx.ty = tx.ty();
        env.tx.main = tx.main();
        env.tx.addrs = tx.addrs();

        let mut ext_state = StateMem::default();
        {
            let mut vmsta = VMState::wrap(&mut ext_state);
            for (addr, sto) in preload {
                vmsta.contract_set_sync_edition(&addr, &sto);
            }
        }

        let mut ctx = make_ctx_with_state(env, Box::new(ext_state), &tx);
        hac_add(&mut ctx, &main, &Amount::unit238(10_000_000_000_000))?;
        ctx.gas_initialize(decode_gas_budget(17))?;

        let mut act = ContractDeploy::new();
        act.nonce = Uint4::from(nonce);
        act.construct_argv = BytesW2::from(construct_argv)?;
        act.contract = deploy_contract;
        act.protocol_cost = contract_protocol_cost_min(
            &ctx,
            act.contract.size(),
            protocol::params::CONTRACT_STORE_PERM_PERIODS,
        )?;
        let _ = act.execute(&mut ctx)?;
        Ok(())
    }

    fn run_update_with_preloaded(
        target: ContractAddress,
        preload: Vec<(ContractAddress, ContractSto)>,
        edit: ContractEdit,
    ) -> Ret<()> {
        init_vm_assigner_once();
        let main = test_main_addr();
        let tx = StubTxBuilder::new()
            .ty(TransactionType3::TYPE)
            .main(main)
            .addrs(vec![main])
            .fee(Amount::unit238(1_000_000))
            .gas_max(17)
            .tx_size(128)
            .fee_purity(1)
            .build();

        let mut env = Env::default();
        env.block.height = 1;
        env.chain.id = 1; // non-mainnet: bypasses online-upgrade height gating in tests
        env.chain.fast_sync = true;
        env.tx.ty = tx.ty();
        env.tx.main = tx.main();
        env.tx.addrs = tx.addrs();

        let mut ext_state = StateMem::default();
        {
            let mut vmsta = VMState::wrap(&mut ext_state);
            for (addr, sto) in preload {
                vmsta.contract_set_sync_edition(&addr, &sto);
            }
        }

        let mut ctx = make_ctx_with_state(env, Box::new(ext_state), &tx);
        hac_add(&mut ctx, &main, &Amount::unit238(10_000_000_000_000))?;
        ctx.gas_initialize(decode_gas_budget(17))?;

        let mut act = ContractUpdate::new();
        act.address = target.to_addr();
        act.edit = edit;
        act.protocol_cost = contract_protocol_cost_min(
            &ctx,
            act.edit.size(),
            protocol::params::CONTRACT_STORE_PERM_PERIODS,
        )?;
        let _ = act.execute(&mut ctx)?;
        Ok(())
    }

    #[test]
    fn deploy_construct_inherits_parent_when_local_absent() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 31);
        let child_nonce = 32;

        let parent = Contract::new()
            .syst(Abst::new(AbstCall::Construct).fitsh("return 1").unwrap())
            .into_sto();
        let child = Contract::new().inh(parent_addr.to_addr()).into_sto();

        let err = run_deploy_with_preloaded(child_nonce, vec![], vec![(parent_addr, parent)], child)
            .expect_err("inherited parent Construct should execute and fail deploy");
        assert!(
            err.contains("Construct") && err.contains("return error code 1"),
            "unexpected deploy error: {err}"
        );
    }

    #[test]
    fn deploy_construct_prefers_local_over_parent() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 33);
        let child_nonce = 34;

        let parent = Contract::new()
            .syst(Abst::new(AbstCall::Construct).fitsh("return 1").unwrap())
            .into_sto();
        let child = Contract::new()
            .inh(parent_addr.to_addr())
            .syst(Abst::new(AbstCall::Construct).fitsh("return 0").unwrap())
            .into_sto();

        let res = run_deploy_with_preloaded(child_nonce, vec![], vec![(parent_addr, parent)], child);
        assert!(res.is_ok(), "local Construct must override inherited one");
    }

    #[test]
    fn deploy_rejects_parent_with_nested_inherit() {
        let main = test_main_addr();
        let grand_addr = test_contract(&main, 35);
        let parent_addr = test_contract(&main, 36);
        let child_nonce = 37;

        let grand = Contract::new().into_sto();
        let parent = Contract::new().inh(grand_addr.to_addr()).into_sto();
        let child = Contract::new().inh(parent_addr.to_addr()).into_sto();

        let err = run_deploy_with_preloaded(
            child_nonce,
            vec![],
            vec![(grand_addr, grand), (parent_addr, parent)],
            child,
        )
        .expect_err("deploy must reject parent with nested inherit");
        assert!(
            err.contains("cannot have parent inherit"),
            "unexpected deploy error: {err}"
        );
    }

    #[test]
    fn deploy_allows_missing_construct_when_construct_argv_empty() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 38);
        let child_nonce = 39;

        let parent = Contract::new().into_sto();
        let child = Contract::new().inh(parent_addr.to_addr()).into_sto();

        let res = run_deploy_with_preloaded(child_nonce, vec![], vec![(parent_addr, parent)], child);
        assert!(res.is_ok(), "deploy should allow missing Construct when argv is empty");
    }

    #[test]
    fn deploy_rejects_non_empty_construct_argv_without_construct() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 40);
        let child_nonce = 41;

        let parent = Contract::new().into_sto();
        let child = Contract::new().inh(parent_addr.to_addr()).into_sto();

        let err = run_deploy_with_preloaded(
            child_nonce,
            vec![0xAA],
            vec![(parent_addr, parent)],
            child,
        )
        .expect_err("deploy should reject non-empty construct argv without Construct");
        assert!(
            err.contains("construct argv provided but Construct hook not found"),
            "unexpected deploy error: {err}"
        );
    }

    #[test]
    fn deploy_construct_auto_runs_when_local_present() {
        let child_nonce = 44;
        let child = Contract::new()
            .syst(Abst::new(AbstCall::Construct).fitsh("return 1").unwrap())
            .into_sto();

        let err = run_deploy_with_preloaded(child_nonce, vec![], vec![], child)
            .expect_err("local Construct should auto-run");
        assert!(
            err.contains("Construct") && err.contains("return error code 1"),
            "unexpected deploy error: {err}"
        );
    }

    #[test]
    fn deploy_construct_auto_runs_when_inherited_present() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 45);
        let child_nonce = 46;

        let parent = Contract::new()
            .syst(Abst::new(AbstCall::Construct).fitsh("return 1").unwrap())
            .into_sto();
        let child = Contract::new().inh(parent_addr.to_addr()).into_sto();

        let err = run_deploy_with_preloaded(child_nonce, vec![], vec![(parent_addr, parent)], child)
            .expect_err("inherited Construct should auto-run");
        assert!(
            err.contains("Construct") && err.contains("return error code 1"),
            "unexpected deploy error: {err}"
        );
    }

    #[test]
    fn update_shadowing_parent_function_dispatches_change() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 40);
        let child_addr = test_contract(&main, 41);

        let parent = Contract::new()
            .func(Func::new("f1").unwrap().fitsh("return 1").unwrap())
            .into_sto();
        let child = Contract::new()
            .inh(parent_addr.to_addr())
            .syst(Abst::new(AbstCall::Append).fitsh("return 1").unwrap())
            .syst(Abst::new(AbstCall::Change).fitsh("return 0").unwrap())
            .into_sto();
        let edit = Contract::new()
            .func(Func::new("f1").unwrap().fitsh("return 2").unwrap())
            .into_edit(1);

        run_update_with_preloaded(
            child_addr.clone(),
            vec![(parent_addr, parent), (child_addr, child)],
            edit,
        )
        .expect("shadowing parent selector should update successfully");
    }

    #[test]
    fn update_new_local_function_without_parent_conflict_stays_append() {
        let main = test_main_addr();
        let parent_addr = test_contract(&main, 42);
        let child_addr = test_contract(&main, 43);

        let parent = Contract::new()
            .func(Func::new("parent_only").unwrap().fitsh("return 1").unwrap())
            .into_sto();
        let child = Contract::new()
            .inh(parent_addr.to_addr())
            .syst(Abst::new(AbstCall::Append).fitsh("return 0").unwrap())
            .syst(Abst::new(AbstCall::Change).fitsh("return 1").unwrap())
            .into_sto();
        let edit = Contract::new()
            .func(Func::new("child_only").unwrap().fitsh("return 2").unwrap())
            .into_edit(1);

        run_update_with_preloaded(
            child_addr.clone(),
            vec![(parent_addr, parent), (child_addr, child)],
            edit,
        )
        .expect("new selector without parent conflict should update successfully");
    }

    /// Compare edit.size() (update tax base) vs on-chain delta after apply_edit.
    #[test]
    fn update_edit_size_usually_covers_chain_delta() {
        init_vm_assigner_once();
        let hei = 1u64;
        let (gst, cap) = {
            let tx = StubTxBuilder::new()
                .ty(TransactionType3::TYPE)
                .main(test_main_addr())
                .fee_purity(1)
                .build();
            let mut env = Env::default();
            env.chain.id = 1;
            let mut ctx = make_ctx_with_state(env, Box::new(StateMem::default()), &tx);
            peek_vm_runtime_limits(&mut ctx, hei)
        };

        let base = || {
            Contract::new()
                .syst(Abst::new(AbstCall::Construct).fitsh("return 0").unwrap())
                .syst(Abst::new(AbstCall::Append).fitsh("return 0").unwrap())
        };

        let measure = |name: &str, old: ContractSto, edit: ContractEdit| {
            let edit_bytes = edit.size();
            let mut new = old.clone();
            new.apply_edit(&edit, hei, &cap, &gst).unwrap();
            let delta = new.size().saturating_sub(old.size());
            assert!(
                edit_bytes >= delta,
                "{name}: edit_bytes={edit_bytes} < chain_delta={delta}"
            );
        };

        measure(
            "add_userfunc",
            base().into_sto(),
            Contract::new()
                .func(
                    Func::new("grow")
                        .unwrap()
                        .external()
                        .fitsh("return 0")
                        .unwrap(),
                )
                .into_edit(1),
        );

        let lib_addr = test_contract(&test_main_addr(), 99);
        measure(
            "library_add",
            base().into_sto(),
            {
                let mut e = ContractEdit::new();
                e.new_revision = Uint2::from(1);
                e.library_add.push(lib_addr).unwrap();
                e
            },
        );

        let parent = test_contract(&test_main_addr(), 98);
        measure(
            "inherit_add",
            base().into_sto(),
            {
                let mut e = ContractEdit::new();
                e.new_revision = Uint2::from(1);
                e.inherit_add.push(parent).unwrap();
                e
            },
        );

        let mut pad_codes = vec![Bytecode::PNIL as u8; 400];
        pad_codes.push(Bytecode::END as u8);
        let large_func = Func::new("big")
            .unwrap()
            .external()
            .bytecode(pad_codes.clone())
            .unwrap();
        measure(
            "add_large_userfunc",
            base().into_sto(),
            Contract::new().func(large_func).into_edit(1),
        );

        let old_with_f = base()
            .func(Func::new("f").unwrap().fitsh("return 0").unwrap())
            .into_sto();
        measure(
            "replace_userfunc_smaller",
            old_with_f.clone(),
            Contract::new()
                .func(Func::new("f").unwrap().fitsh("return 1").unwrap())
                .into_edit(1),
        );
        measure(
            "replace_userfunc_larger",
            old_with_f,
            Contract::new()
                .func(
                    Func::new("f")
                        .unwrap()
                        .external()
                        .bytecode(pad_codes)
                        .unwrap(),
                )
                .into_edit(1),
        );
    }

    fn make_protocol_cost_ctx(
        fee_purity: u64,
    ) -> (
        &'static testkit::sim::tx::StubTx,
        &'static mut protocol::context::ContextInst<'static>,
    ) {
        init_vm_assigner_once();
        let main = test_main_addr();
        let tx = Box::leak(Box::new(
            StubTxBuilder::new()
                .ty(TransactionType3::TYPE)
                .main(main)
                .addrs(vec![main])
                .fee(Amount::unit238(1_000_000))
                .gas_max(17)
                .tx_size(128)
                .fee_purity(fee_purity)
                .build(),
        ));
        let mut env = Env::default();
        env.block.height = 1;
        env.chain.id = 1;
        env.chain.fast_sync = true;
        env.tx.ty = tx.ty();
        env.tx.main = tx.main();
        env.tx.addrs = tx.addrs();
        let ctx = Box::leak(Box::new(make_ctx_with_state(
            env,
            Box::new(StateMem::default()),
            tx,
        )));
        (tx, ctx)
    }

    #[test]
    fn contract_protocol_cost_min_uses_vm_fee_purity_floor() {
        let (_tx, ctx) = make_protocol_cost_ctx(1);
        let min =
            contract_protocol_cost_min(ctx, 48, protocol::params::CONTRACT_STORE_PERM_PERIODS)
                .unwrap();
        let expect = Amount::coin_u128(
            protocol::params::VM_LOWEST_FEE_PURITY as u128 * 48 * 10_000,
            field::UNIT_238,
        );
        assert_eq!(min, expect);
        assert!(
            Amount::coin(10000, 244) < min,
            "legacy test fixture 1:248 must stay below deploy floor"
        );
    }

    #[test]
    fn contract_protocol_cost_min_scales_with_charge_bytes_and_periods() {
        let (_tx, ctx) = make_protocol_cost_ctx(80_000);
        let fee_purity = protocol::params::vm_effective_fee_purity(
            ctx.env().block.height,
            ctx.tx().fee_purity(),
        ) as u128;
        assert_eq!(fee_purity, 80_000);
        let min = contract_protocol_cost_min(ctx, 25, 10_000).unwrap();
        assert_eq!(min, Amount::coin_u128(fee_purity * 25 * 10_000, field::UNIT_238));
    }

    #[test]
    fn contract_protocol_cost_min_zero_when_charge_bytes_zero() {
        let (_tx, ctx) = make_protocol_cost_ctx(1);
        assert_eq!(
            contract_protocol_cost_min(ctx, 0, protocol::params::CONTRACT_STORE_PERM_PERIODS)
                .unwrap(),
            Amount::zero()
        );
    }
}
