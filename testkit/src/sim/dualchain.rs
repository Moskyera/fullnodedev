use std::fmt::Debug;
use std::sync::MutexGuard;

use basis::interface::Action;
use field::{Address, Amount, AssetAmt, DiamondName, DiamondNumber, Fold64, Hash, Satoshi};
use sys::{Account, Ret};

use crate::sim::integration::test_guard;
use crate::sim::memchain::{
    BlockReceipt, ConfirmedBlockReceipt, ContractAddress, ContractEdit, ContractSto, MemChain,
    SandboxResult, SandboxSpec, TxOutput, TxReceipt, Value,
};
use crate::sim::state::StateBackendKind;

pub struct DualMemChain {
    independent: MemChain,
    production: MemChain,
    _guard: MutexGuard<'static, ()>,
}

impl Default for DualMemChain {
    fn default() -> Self {
        Self::new()
    }
}

impl DualMemChain {
    pub fn new() -> Self {
        let guard = test_guard();
        MemChain::install_standard_setup();
        Self {
            independent: MemChain::with_state_backend_unlocked(
                StateBackendKind::IndependentMem,
                None,
            ),
            production: MemChain::with_state_backend_unlocked(
                StateBackendKind::ProductionStateInst,
                None,
            ),
            _guard: guard,
        }
    }

    pub fn independent(&self) -> &MemChain {
        &self.independent
    }

    pub fn production(&self) -> &MemChain {
        &self.production
    }

    pub fn independent_mut(&mut self) -> &mut MemChain {
        &mut self.independent
    }

    pub fn production_mut(&mut self) -> &mut MemChain {
        &mut self.production
    }

    pub fn height(&self) -> u64 {
        let left = self.independent.height();
        let right = self.production.height();
        assert_same("height", &left, &right);
        left
    }

    pub fn set_height(&mut self, height: u64) {
        self.independent.set_height(height);
        self.production.set_height(height);
        self.height();
    }

    pub fn last_block_hash(&self) -> Hash {
        let left = self.independent.last_block_hash();
        let right = self.production.last_block_hash();
        assert_same("last_block_hash", &left, &right);
        left
    }

    pub fn pending_len(&self) -> usize {
        let left = self.independent.pending_len();
        let right = self.production.pending_len();
        assert_same("pending_len", &left, &right);
        left
    }

    /// Drop all pending transactions from both backends.
    pub fn clear_pending(&mut self) {
        self.independent.clear_pending();
        self.production.clear_pending();
    }

    pub fn production_disk_entry_count(&self) -> Option<usize> {
        self.production.disk_entry_count()
    }

    pub fn production_disk_entries(&self) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        self.production.disk_entries()
    }

    pub fn assert_state_entries_match(&self) {
        let left = self.independent.state_entries();
        let right = self.production.state_entries();
        assert_same("effective_state_entries", &left, &right);
    }

    pub fn drop_pending(&mut self, hash: &Hash) -> bool {
        let left = self.independent.drop_pending(hash);
        let right = self.production.drop_pending(hash);
        assert_same("drop_pending", &left, &right);
        left
    }

    pub fn pending_signature_report(
        &self,
        hash: &Hash,
    ) -> Ret<Option<protocol::transaction::TxSignatureReport>> {
        let left = self.independent.pending_signature_report(hash);
        let right = self.production.pending_signature_report(hash);
        assert_same("pending_signature_report", &left, &right);
        left
    }

    pub fn pending_action_topology(
        &self,
        hash: &Hash,
    ) -> Ret<Option<protocol::action::TxActionTopology>> {
        let left = self.independent.pending_action_topology(hash);
        let right = self.production.pending_action_topology(hash);
        assert_same("pending_action_topology", &left, &right);
        left
    }

    pub fn pending_raw(&self, hash: &Hash) -> Option<Vec<u8>> {
        let left = self.independent.pending_raw(hash);
        let right = self.production.pending_raw(hash);
        assert_same("pending_raw", &left, &right);
        left
    }

    pub fn mint_hac(&mut self, addr: &Address, sat: u64) {
        self.independent.mint_hac(addr, sat);
        self.production.mint_hac(addr, sat);
        self.balance(addr);
        self.assert_state_entries_match();
    }

    pub fn fund(&mut self, addr: &Address, amount: Amount) {
        self.independent.fund(addr, amount.clone());
        self.production.fund(addr, amount);
        self.balance(addr);
        self.assert_state_entries_match();
    }

    pub fn fund_sat(&mut self, addr: &Address, sat: u64) {
        self.independent.fund_sat(addr, sat);
        self.production.fund_sat(addr, sat);
        self.satoshi(addr);
        self.assert_state_entries_match();
    }

    pub fn fund_diamond(&mut self, addr: &Address, diamond: DiamondName) {
        self.independent.fund_diamond(addr, diamond);
        self.production.fund_diamond(addr, diamond);
        self.diamond_owner(&diamond);
        self.assert_state_entries_match();
    }

    pub fn fund_asset(&mut self, addr: &Address, asset: AssetAmt) {
        self.independent.fund_asset(addr, asset.clone());
        self.production.fund_asset(addr, asset.clone());
        self.asset_balance(addr, asset.serial);
        self.assert_state_entries_match();
    }

    pub fn balance(&self, addr: &Address) -> Amount {
        let left = self.independent.balance(addr);
        let right = self.production.balance(addr);
        assert_same("balance", &left, &right);
        left
    }

    pub fn satoshi(&self, addr: &Address) -> Satoshi {
        let left = self.independent.satoshi(addr);
        let right = self.production.satoshi(addr);
        assert_same("satoshi", &left, &right);
        left
    }

    pub fn diamond_count(&self, addr: &Address) -> DiamondNumber {
        let left = self.independent.diamond_count(addr);
        let right = self.production.diamond_count(addr);
        assert_same("diamond_count", &left, &right);
        left
    }

    pub fn diamond_owner(&self, diamond: &DiamondName) -> Option<Address> {
        let left = self.independent.diamond_owner(diamond);
        let right = self.production.diamond_owner(diamond);
        assert_same("diamond_owner", &left, &right);
        left
    }

    pub fn asset_balance(&self, addr: &Address, serial: Fold64) -> AssetAmt {
        let left = self.independent.asset_balance(addr, serial);
        let right = self.production.asset_balance(addr, serial);
        assert_same("asset_balance", &left, &right);
        left
    }

    pub fn submit_main_call(&mut self, main: Address, codes: Vec<u8>, gas_max: u8) -> Hash {
        self.submit_main_call_with_addrs(main, vec![main], codes, gas_max)
    }

    pub fn submit_main_call_with_addrs(
        &mut self,
        main: Address,
        addrs: Vec<Address>,
        codes: Vec<u8>,
        gas_max: u8,
    ) -> Hash {
        let left = self.independent.submit_main_call_with_addrs(
            main,
            addrs.clone(),
            codes.clone(),
            gas_max,
        );
        let right = self
            .production
            .submit_main_call_with_addrs(main, addrs, codes, gas_max);
        assert_same("submit_main_call", &left, &right);
        left
    }

    pub fn submit_deploy(
        &mut self,
        contract: ContractSto,
        deployer: Address,
        nonce: u32,
    ) -> (Hash, ContractAddress) {
        self.submit_deploy_with_gas(contract, deployer, nonce, u8::MAX)
    }

    pub fn submit_deploy_with_gas(
        &mut self,
        contract: ContractSto,
        deployer: Address,
        nonce: u32,
        gas_max: u8,
    ) -> (Hash, ContractAddress) {
        let left =
            self.independent
                .submit_deploy_with_gas(contract.clone(), deployer, nonce, gas_max);
        let right = self
            .production
            .submit_deploy_with_gas(contract, deployer, nonce, gas_max);
        assert_same("submit_deploy", &left, &right);
        left
    }

    pub fn submit_update(
        &mut self,
        addr: ContractAddress,
        edit: ContractEdit,
        updater: Address,
        gas_max: u8,
    ) -> Hash {
        let left = self
            .independent
            .submit_update(addr.clone(), edit.clone(), updater, gas_max);
        let right = self.production.submit_update(addr, edit, updater, gas_max);
        assert_same("submit_update", &left, &right);
        left
    }

    pub fn analyze_contract_update(
        &mut self,
        addr: ContractAddress,
        edit: ContractEdit,
        updater: Address,
    ) -> Ret<vm::action::ContractUpdateAnalysis> {
        let left = self
            .independent
            .analyze_contract_update(addr.clone(), edit.clone(), updater);
        let right = self.production.analyze_contract_update(addr, edit, updater);
        assert_same("analyze_contract_update", &left, &right);
        left
    }

    pub fn submit_contract_call(
        &mut self,
        caller: Address,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let left = self.independent.submit_contract_call(
            caller,
            addr.clone(),
            func,
            args.clone(),
            gas_max,
        );
        let right = self
            .production
            .submit_contract_call(caller, addr, func, args, gas_max);
        assert_same("submit_contract_call", &left, &right);
        left
    }

    pub fn submit_formal_deploy(
        &mut self,
        account: &Account,
        contract: ContractSto,
        nonce: u32,
    ) -> Ret<(Hash, ContractAddress)> {
        self.submit_formal_deploy_with_gas(account, contract, nonce, u8::MAX)
    }

    pub fn submit_formal_deploy_with_gas(
        &mut self,
        account: &Account,
        contract: ContractSto,
        nonce: u32,
        gas_max: u8,
    ) -> Ret<(Hash, ContractAddress)> {
        let left = self.independent.submit_formal_deploy_with_gas(
            account,
            contract.clone(),
            nonce,
            gas_max,
        );
        let right = self
            .production
            .submit_formal_deploy_with_gas(account, contract, nonce, gas_max);
        assert_same("submit_formal_deploy", &left, &right);
        left
    }

    pub fn submit_formal_contract_call(
        &mut self,
        account: &Account,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let left = self.independent.submit_formal_contract_call(
            account,
            addr.clone(),
            func,
            args.clone(),
            gas_max,
        );
        let right = self
            .production
            .submit_formal_contract_call(account, addr, func, args, gas_max);
        assert_same("submit_formal_contract_call", &left, &right);
        left
    }

    pub fn submit_formal_main_call(
        &mut self,
        account: &Account,
        addrs: Vec<Address>,
        codes: Vec<u8>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let left = self.independent.submit_formal_main_call(
            account,
            addrs.clone(),
            codes.clone(),
            gas_max,
        );
        let right = self
            .production
            .submit_formal_main_call(account, addrs, codes, gas_max);
        assert_same("submit_formal_main_call", &left, &right);
        left
    }

    pub fn submit_formal_main_call_with_signers(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        codes: Vec<u8>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let left = self.independent.submit_formal_main_call_with_signers(
            account,
            extra_signers,
            addrs.clone(),
            codes.clone(),
            gas_max,
        );
        let right = self.production.submit_formal_main_call_with_signers(
            account,
            extra_signers,
            addrs,
            codes,
            gas_max,
        );
        assert_same("submit_formal_main_call_with_signers", &left, &right);
        left
    }

    pub fn submit_formal_main_call_fitsh(
        &mut self,
        account: &Account,
        addrs: Vec<Address>,
        src: &str,
        gas_max: u8,
    ) -> Ret<Hash> {
        let left =
            self.independent
                .submit_formal_main_call_fitsh(account, addrs.clone(), src, gas_max);
        let right = self
            .production
            .submit_formal_main_call_fitsh(account, addrs, src, gas_max);
        assert_same("submit_formal_main_call_fitsh", &left, &right);
        left
    }

    pub fn submit_formal_main_call_fitsh_with_signers(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        src: &str,
        gas_max: u8,
    ) -> Ret<Hash> {
        let left = self.independent.submit_formal_main_call_fitsh_with_signers(
            account,
            extra_signers,
            addrs.clone(),
            src,
            gas_max,
        );
        let right = self.production.submit_formal_main_call_fitsh_with_signers(
            account,
            extra_signers,
            addrs,
            src,
            gas_max,
        );
        assert_same("submit_formal_main_call_fitsh_with_signers", &left, &right);
        left
    }

    /// Read-only fitsh main call (no state commit). Delegates to both
    /// backends and asserts matching return values.
    pub fn sandbox_main_call_fitsh(
        &mut self,
        main: Address,
        addrs: Vec<Address>,
        src: &str,
        gas_max: u8,
    ) -> Ret<Value> {
        let left = self
            .independent
            .sandbox_main_call_fitsh(main, addrs.clone(), src, gas_max);
        let right = self
            .production
            .sandbox_main_call_fitsh(main, addrs, src, gas_max);
        assert_same("sandbox_main_call_fitsh", &left, &right);
        left
    }

    pub fn submit_formal_actions<F>(
        &mut self,
        account: &Account,
        addrs: Vec<Address>,
        gas_max: u8,
        output: TxOutput,
        mut build_actions: F,
    ) -> Ret<Hash>
    where
        F: FnMut() -> Ret<Vec<Box<dyn Action>>>,
    {
        let left = self.independent.submit_formal_actions(
            account,
            addrs.clone(),
            build_actions()?,
            gas_max,
            output.clone(),
        );
        let right = self.production.submit_formal_actions(
            account,
            addrs,
            build_actions()?,
            gas_max,
            output,
        );
        assert_same("submit_formal_actions", &left, &right);
        left
    }

    pub fn submit_formal_actions_with_signers<F>(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        gas_max: u8,
        output: TxOutput,
        mut build_actions: F,
    ) -> Ret<Hash>
    where
        F: FnMut() -> Ret<Vec<Box<dyn Action>>>,
    {
        let left = self.independent.submit_formal_actions_with_signers(
            account,
            extra_signers,
            addrs.clone(),
            build_actions()?,
            gas_max,
            output.clone(),
        );
        let right = self.production.submit_formal_actions_with_signers(
            account,
            extra_signers,
            addrs,
            build_actions()?,
            gas_max,
            output,
        );
        assert_same("submit_formal_actions_with_signers", &left, &right);
        left
    }

    pub fn submit_formal_raw(&mut self, raw: &[u8], output: TxOutput) -> Ret<Hash> {
        let left = self.independent.submit_formal_raw(raw, output.clone());
        let right = self.production.submit_formal_raw(raw, output);
        assert_same("submit_formal_raw", &left, &right);
        left
    }

    pub fn build_formal_actions_raw<F>(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        gas_max: u8,
        mut build_actions: F,
    ) -> Ret<Vec<u8>>
    where
        F: FnMut() -> Ret<Vec<Box<dyn Action>>>,
    {
        let left = self.independent.build_formal_actions_raw(
            account,
            extra_signers,
            addrs.clone(),
            build_actions()?,
            gas_max,
        );
        let right = self.production.build_formal_actions_raw(
            account,
            extra_signers,
            addrs,
            build_actions()?,
            gas_max,
        );
        assert_same("build_formal_actions_raw", &left, &right);
        left
    }

    pub fn mine_block(&mut self) -> BlockReceipt {
        let left = self.independent.mine_block();
        let right = self.production.mine_block();
        assert_same("mine_block", &left, &right);
        self.assert_state_entries_match();
        left
    }

    pub fn confirm_formal_block(&mut self, miner: Address) -> Ret<ConfirmedBlockReceipt> {
        let left = self.independent.confirm_formal_block(miner);
        let right = self.production.confirm_formal_block(miner);
        assert_same_result("confirm_formal_block", &left, &right);
        if left.is_ok() {
            self.assert_state_entries_match();
        }
        left
    }

    pub fn confirm_formal_block_observing_failures(
        &mut self,
        miner: Address,
    ) -> Ret<ConfirmedBlockReceipt> {
        let left = self
            .independent
            .confirm_formal_block_observing_failures(miner);
        let right = self
            .production
            .confirm_formal_block_observing_failures(miner);
        assert_same_block_result("confirm_formal_block_observing_failures", &left, &right);
        if left.is_ok() {
            self.assert_state_entries_match();
        }
        left
    }

    pub fn mine_block_containing(&mut self, hash: &Hash) -> Ret<TxReceipt> {
        let left = self.independent.mine_block_containing(hash);
        let right = self.production.mine_block_containing(hash);
        assert_same("mine_block_containing", &left, &right);
        if left.is_ok() {
            self.assert_state_entries_match();
        }
        left
    }

    pub fn call_func(
        &mut self,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
    ) -> Ret<SandboxResult> {
        let left = self.independent.call_func(addr.clone(), func, args.clone());
        let right = self.production.call_func(addr, func, args);
        assert_same("call_func", &left, &right);
        left
    }

    pub fn call_func_from(
        &mut self,
        caller: Address,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
    ) -> Ret<SandboxResult> {
        let left = self
            .independent
            .call_func_from(caller, addr.clone(), func, args.clone());
        let right = self.production.call_func_from(caller, addr, func, args);
        assert_same("call_func_from", &left, &right);
        left
    }

    pub fn sandbox(&mut self, spec: SandboxSpec) -> Ret<SandboxResult> {
        let left = self.independent.sandbox(spec.clone());
        let right = self.production.sandbox(spec);
        assert_same("sandbox", &left, &right);
        left
    }

    pub fn storage(&self, addr: &ContractAddress, key: &Value) -> Value {
        let left = self.independent.storage(addr, key);
        let right = self.production.storage(addr, key);
        assert_same("storage", &left, &right);
        left
    }
}

fn assert_same<T>(label: &str, independent: &T, production: &T)
where
    T: PartialEq + Debug,
{
    assert_eq!(
        independent, production,
        "dual backend mismatch during {label}: independent={independent:?}, production={production:?}"
    );
}

fn assert_same_result<T>(label: &str, independent: &Ret<T>, production: &Ret<T>)
where
    T: PartialEq + Debug,
{
    match (independent, production) {
        (Ok(left), Ok(right)) => assert_same(label, left, right),
        (Err(left), Err(right)) if left == right => {}
        (Err(left), Err(right))
            if normalize_dual_error(left) == normalize_dual_error(right) => {}
        _ => assert_same(label, independent, production),
    }
}

fn assert_same_block_result(
    label: &str,
    independent: &Ret<ConfirmedBlockReceipt>,
    production: &Ret<ConfirmedBlockReceipt>,
) {
    match (independent, production) {
        (Ok(left), Ok(right)) if blocks_match_with_normalized_errors(left, right) => {}
        _ => assert_same_result(label, independent, production),
    }
}

fn blocks_match_with_normalized_errors(
    independent: &ConfirmedBlockReceipt,
    production: &ConfirmedBlockReceipt,
) -> bool {
    independent.height == production.height
        && independent.block_hash == production.block_hash
        && independent.report == production.report
        && receipts_match_with_normalized_errors(&independent.receipts, &production.receipts)
}

fn receipts_match_with_normalized_errors(
    independent: &[TxReceipt],
    production: &[TxReceipt],
) -> bool {
    independent.len() == production.len()
        && independent
            .iter()
            .zip(production)
            .all(|(left, right)| receipt_matches_with_normalized_error(left, right))
}

fn receipt_matches_with_normalized_error(independent: &TxReceipt, production: &TxReceipt) -> bool {
    independent.tx_hash == production.tx_hash
        && independent.height == production.height
        && independent.success == production.success
        && independent.output == production.output
        && independent.log_count == production.log_count
        && independent.gas_used == production.gas_used
        && normalized_error_matches(&independent.error, &production.error)
}

fn normalized_error_matches(independent: &Option<String>, production: &Option<String>) -> bool {
    match (independent, production) {
        (Some(left), Some(right)) => left == right || normalize_dual_error(left) == normalize_dual_error(right),
        (None, None) => true,
        _ => false,
    }
}

fn normalize_dual_error(err: &str) -> String {
    normalize_intent_not_found_handle(err)
}

fn normalize_intent_not_found_handle(err: &str) -> String {
    let mut out = String::with_capacity(err.len());
    let mut rest = err;
    while let Some(pos) = rest.find("intent ") {
        out.push_str(&rest[..pos + "intent ".len()]);
        let after = &rest[pos + "intent ".len()..];
        let digit_len = after
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digit_len > 0 && after[digit_len..].starts_with(" not found") {
            out.push_str("<handle>");
            rest = &after[digit_len..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}
