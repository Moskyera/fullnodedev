//! `MemChain` — an in-memory simulated blockchain for multi-round VM testing.
//!
//! The simulator keeps a single long-lived [`State`] (a [`FlatMemState`]) and
//! [`MemLogs`] across many transactions, so contract deployments, storage
//! writes and main-call effects persist from one tx to the next. Nothing is
//! written to disk — every read and write stays in memory, keeping tests fast.
//!
//! Each transaction runs through the real protocol/VM stack: the context is
//! built over a clone of the persistent state, gas is initialised, the action
//! (or `Executor::main_call`) is executed, the deferred phase drains, and on
//! success the (possibly mutated) state/logs are carried back into the chain
//! — on failure the original state is preserved.
//!
//! `MemChain` is the only addition to `testkit`; nothing else here changes.

use basis::component::{ChainInfo, Env};
use basis::interface::{
    ActExec, Block, BlockRead, Context, Logs, State, StateOperat, Transaction, TransactionRead,
};
use field::{
    AddrOrList, Address, Amount, AssetAmt, AssetSmelt, BlockHeight, BytesW1, DIAMOND_STATUS_NORMAL,
    DiamondName, DiamondNumber, DiamondSmelt, DiamondSto, Field, Fixed8, Fixed16, Fold64, Hash,
    Satoshi, Serialize, Timestamp, Uint1, Uint2, Uint4,
};
use protocol::block::BlockV1;
use protocol::context::{ContextInst, TX_GAS_BUDGET_CAP_BYTE, decode_gas_budget};
use protocol::transaction::{DefaultPreludeTx, TransactionType3, transaction_create};
use std::sync::MutexGuard;
use sys::{Account, Ret};

use crate::sim::integration::{
    enable_default_vm_setup, ensure_standard_protocol_setup_for_tests, test_guard,
};
use crate::sim::logs::MemLogs;
use crate::sim::state::{ChainState, ChainStateSnapshot, StateBackendKind};
use crate::sim::tx::{StubTx, StubTxBuilder};

// Re-exported vm types for ergonomic test code.
pub use vm::machine::{self, Executor, Runtime, SandboxResult, SandboxSpec};
pub use vm::value::Value;
pub use vm::{ContractAddress, ContractEdit, ContractSto, VMState, VMStateRead};

pub const FORMAL_TX_FEE_238: u64 = 10_000_000;
pub const FORMAL_CONTRACT_PROTOCOL_COST_238: u64 = 1_000_000_000_000;
const FORMAL_TX_TIMESTAMP_BASE: u64 = 1_730_000_000;

/// The standard block hasher used across tests (mirrors `sys::calculate_hash`).
fn std_block_hasher() -> protocol::setup::FnBlockHasherFunc {
    |_, stuff| sys::calculate_hash(stuff)
}

/// A simulated in-memory chain: persistent state + logs across many txs.
///
/// Construct with [`MemChain::new`]; drive with [`MemChain::main_call`],
/// [`MemChain::deploy`], [`MemChain::tx_run`], [`MemChain::call_func`], …
pub struct MemChain {
    state: ChainState,
    logs: MemLogs,
    pending: Vec<PendingTx>,
    receipts: Vec<TxReceipt>,
    next_tx_seq: u64,
    height: u64,
    last_block_hash: Hash,
    // Held for the lifetime of the chain: serialises tests (protocol setup is
    // global mutable state). The standard VM+mint setup is installed via
    // `enable_default_vm_setup`, which stores its scope guard in thread-local
    // state so it lives as long as the test thread.
    _guard: Option<MutexGuard<'static, ()>>,
}

impl Default for MemChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of a single transaction: the merged state/logs and any produced
/// logs. Returned from the inner run helper so the caller can decide whether
/// to commit or discard.
struct TxOutcome {
    state: Box<dyn State>,
    logs: MemLogs,
    /// Net gas used by this tx (protocol-side `GasCounter.used_net`).
    /// `0` when gas was never initialised (e.g. coinbase / ty < 3).
    gas_used: i64,
}

struct PendingTx {
    tx: PendingTxKind,
    op: PendingOp,
}

enum PendingTxKind {
    Stub(StubTx),
    Formal(Box<dyn Transaction>),
}

impl PendingTxKind {
    fn hash(&self) -> Hash {
        match self {
            PendingTxKind::Stub(tx) => tx.hash(),
            PendingTxKind::Formal(tx) => tx.hash(),
        }
    }

    fn as_read(&self) -> &dyn TransactionRead {
        match self {
            PendingTxKind::Stub(tx) => tx,
            PendingTxKind::Formal(tx) => tx.as_read(),
        }
    }
}

enum PendingOp {
    MainCall {
        codes: Vec<u8>,
    },
    Deploy {
        contract: ContractSto,
        nonce: u32,
    },
    Update {
        addr: ContractAddress,
        edit: ContractEdit,
    },
    Action {
        action: Box<dyn basis::interface::Action>,
    },
    ContractCall {
        codes: Vec<u8>,
    },
    FormalTx {
        output: TxOutput,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOutput {
    None,
    Value(Value),
    ContractAddress(ContractAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxReceipt {
    pub tx_hash: Hash,
    pub height: u64,
    pub success: bool,
    pub output: TxOutput,
    pub error: Option<String>,
    pub log_count: usize,
    /// Net gas consumed by this tx (`None` when the tx did not run through the
    /// gas-initialised path, e.g. the coinbase/prelude tx or the
    /// `confirm_formal_block` aggregate path that does not surface per-tx
    /// outcomes).
    pub gas_used: Option<i64>,
}

impl TxReceipt {
    pub fn is_success(&self) -> bool {
        self.success
    }

    pub fn is_error(&self) -> bool {
        !self.success
    }

    pub fn expect_success(&self) -> &Self {
        assert!(
            self.success,
            "expected tx {} success at height {}, got error: {}",
            self.tx_hash,
            self.height,
            self.error.as_deref().unwrap_or("")
        );
        self
    }

    pub fn expect_value(&self, expected: Value) -> &Self {
        self.expect_success();
        assert_eq!(
            self.output,
            TxOutput::Value(expected),
            "tx {} output mismatch",
            self.tx_hash
        );
        self
    }

    pub fn expect_contract_address(&self, expected: &ContractAddress) -> &Self {
        self.expect_success();
        assert_eq!(
            self.output,
            TxOutput::ContractAddress(expected.clone()),
            "tx {} output mismatch",
            self.tx_hash
        );
        self
    }

    pub fn expect_error_contains(&self, needle: &str) -> &Self {
        assert!(
            !self.success,
            "expected tx {} to fail containing '{}', but it succeeded",
            self.tx_hash, needle
        );
        let error = self.error.as_deref().unwrap_or("");
        assert!(
            error.contains(needle),
            "expected tx {} error to contain '{}', got '{}'",
            self.tx_hash,
            needle,
            error
        );
        self
    }

    /// Assert that this tx consumed at least `min` gas.
    pub fn expect_gas_at_least(&self, min: i64) -> &Self {
        let used = self.gas_used.unwrap_or(0);
        assert!(
            used >= min,
            "expected tx {} gas_used >= {}, got {}",
            self.tx_hash,
            min,
            used
        );
        self
    }

    /// Assert that this tx consumed strictly more than `threshold` gas.
    pub fn expect_gas_greater_than(&self, threshold: i64) -> &Self {
        let used = self.gas_used.unwrap_or(0);
        assert!(
            used > threshold,
            "expected tx {} gas_used > {}, got {}",
            self.tx_hash,
            threshold,
            used
        );
        self
    }

    /// Return the net gas used by this tx, or `0` if unavailable.
    pub fn gas_used_value(&self) -> i64 {
        self.gas_used.unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockReceipt {
    pub height: u64,
    pub receipts: Vec<TxReceipt>,
}

impl BlockReceipt {
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    pub fn receipt(&self, hash: &Hash) -> Option<&TxReceipt> {
        self.receipts
            .iter()
            .find(|receipt| &receipt.tx_hash == hash)
    }

    pub fn expect_all_success(&self) -> &Self {
        for receipt in &self.receipts {
            receipt.expect_success();
        }
        self
    }

    pub fn expect_success(&self, hash: &Hash) -> &TxReceipt {
        self.receipt(hash)
            .unwrap_or_else(|| panic!("tx {} was not included in block {}", hash, self.height))
            .expect_success()
    }

    pub fn expect_error_contains(&self, hash: &Hash, needle: &str) -> &TxReceipt {
        self.receipt(hash)
            .unwrap_or_else(|| panic!("tx {} was not included in block {}", hash, self.height))
            .expect_error_contains(needle)
    }
}

impl IntoIterator for BlockReceipt {
    type Item = TxReceipt;
    type IntoIter = std::vec::IntoIter<TxReceipt>;

    fn into_iter(self) -> Self::IntoIter {
        self.receipts.into_iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedBlockReceipt {
    pub height: u64,
    pub block_hash: Hash,
    pub receipts: Vec<TxReceipt>,
    pub report: protocol::block::BlockExecutionReport,
}

impl ConfirmedBlockReceipt {
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    pub fn receipt(&self, hash: &Hash) -> Option<&TxReceipt> {
        self.receipts
            .iter()
            .find(|receipt| &receipt.tx_hash == hash)
    }

    pub fn expect_all_success(&self) -> &Self {
        for receipt in &self.receipts {
            receipt.expect_success();
        }
        self
    }

    pub fn expect_success(&self, hash: &Hash) -> &TxReceipt {
        self.receipt(hash)
            .unwrap_or_else(|| {
                panic!(
                    "tx {} was not included in confirmed block {}",
                    hash, self.height
                )
            })
            .expect_success()
    }

    pub fn tx_report(&self, hash: &Hash) -> Option<&protocol::block::BlockTxExecutionReport> {
        self.report.txs.iter().find(|tx| &tx.tx_hash == hash)
    }

    pub fn expect_tx_report(&self, hash: &Hash) -> &protocol::block::BlockTxExecutionReport {
        self.tx_report(hash).unwrap_or_else(|| {
            panic!(
                "tx {} has no execution report in block {}",
                hash, self.height
            )
        })
    }

    pub fn user_tx_count(&self) -> usize {
        self.report.tx_count.saturating_sub(1)
    }
}

/// Summary for a long run of empty formal blocks.
///
/// Each block is still confirmed through [`BlockV1::execute_with_report`];
/// this type just keeps callers from having to store thousands of identical
/// no-user-tx receipts when a test only needs to move the chain clock forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedEmptyBlockBatchReceipt {
    pub start_height: u64,
    pub end_height: u64,
    pub count: u64,
    pub first_block: Option<ConfirmedBlockReceipt>,
    pub last_block: Option<ConfirmedBlockReceipt>,
}

impl ConfirmedEmptyBlockBatchReceipt {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn first_height(&self) -> Option<u64> {
        self.first_block.as_ref().map(|block| block.height)
    }

    pub fn last_height(&self) -> Option<u64> {
        self.last_block.as_ref().map(|block| block.height)
    }
}

impl MemChain {
    /// Create an empty chain at height 1 with the standard protocol + VM setup
    /// already installed (mint + vm actions + default vm assigner).
    pub fn new() -> Self {
        Self::with_state_backend(StateBackendKind::IndependentMem)
    }

    pub fn with_state_backend(state_backend: StateBackendKind) -> Self {
        let guard = test_guard();
        Self::install_standard_setup();
        Self::with_state_backend_unlocked(state_backend, Some(guard))
    }

    pub(crate) fn install_standard_setup() {
        ensure_standard_protocol_setup_for_tests(std_block_hasher(), true);
        enable_default_vm_setup();
    }

    pub(crate) fn with_state_backend_unlocked(
        state_backend: StateBackendKind,
        guard: Option<MutexGuard<'static, ()>>,
    ) -> Self {
        MemChain {
            state: ChainState::new(state_backend),
            logs: MemLogs::default(),
            pending: Vec::new(),
            receipts: Vec::new(),
            next_tx_seq: 1,
            height: 1,
            last_block_hash: Hash::default(),
            _guard: guard,
        }
    }

    pub fn production_state_inst() -> Self {
        Self::with_state_backend(StateBackendKind::ProductionStateInst)
    }

    // ───────────────────────── block / account ─────────────────────────

    /// Current block height.
    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn last_block_hash(&self) -> Hash {
        self.last_block_hash
    }

    pub fn state_backend(&self) -> StateBackendKind {
        self.state.kind()
    }

    pub fn disk_entry_count(&self) -> Option<usize> {
        self.state.disk_entry_count()
    }

    pub fn disk_entries(&self) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        self.state.disk_entries()
    }

    pub fn state_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.state.effective_entries()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Drop all pending transactions from the pool.
    ///
    /// Used by test harnesses when a formal block reverts: the failed tx is
    /// left in `pending` because `confirm_formal_block` returns early on
    /// `execute_with_report` errors (before the `pending.clear()` cleanup),
    /// and without an explicit drain the reverted tx would poison the next
    /// mint.
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub fn pending_signature_report(
        &self,
        hash: &Hash,
    ) -> Ret<Option<protocol::transaction::TxSignatureReport>> {
        let Some(pending) = self
            .pending
            .iter()
            .find(|pending| &pending.tx.hash() == hash)
        else {
            return Ok(None);
        };
        Ok(Some(protocol::transaction::signature_report(
            pending.tx.as_read(),
        )?))
    }

    pub fn pending_action_topology(
        &self,
        hash: &Hash,
    ) -> Ret<Option<protocol::action::TxActionTopology>> {
        let Some(pending) = self
            .pending
            .iter()
            .find(|pending| &pending.tx.hash() == hash)
        else {
            return Ok(None);
        };
        let tx = pending.tx.as_read();
        Ok(Some(protocol::action::precheck_tx_actions_report(
            tx.ty(),
            tx.actions(),
        )?))
    }

    pub fn pending_raw(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.pending
            .iter()
            .find(|pending| &pending.tx.hash() == hash)
            .map(|pending| pending.tx.as_read().serialize())
    }

    pub fn drop_pending(&mut self, hash: &Hash) -> bool {
        let before = self.pending.len();
        self.pending.retain(|pending| &pending.tx.hash() != hash);
        self.pending.len() != before
    }

    pub fn receipts(&self) -> &[TxReceipt] {
        &self.receipts
    }

    pub fn receipt(&self, hash: &Hash) -> Option<&TxReceipt> {
        self.receipts
            .iter()
            .find(|receipt| &receipt.tx_hash == hash)
    }

    /// Set the current block height.
    pub fn set_height(&mut self, h: u64) {
        self.height = h;
    }

    /// Advance to the next block (`height += 1`). This also advances the
    /// effective height for rentable-storage expiry checks.
    pub fn advance_block(&mut self) {
        self.height += 1;
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
        self.submit_pending(main, addrs, gas_max, PendingOp::MainCall { codes })
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
        let caddr = ContractAddress::calculate(&deployer, &Uint4::from(nonce));
        let hash = self.submit_pending(
            deployer,
            vec![deployer],
            gas_max,
            PendingOp::Deploy { contract, nonce },
        );
        (hash, caddr)
    }

    pub fn submit_update(
        &mut self,
        addr: ContractAddress,
        edit: ContractEdit,
        updater: Address,
        gas_max: u8,
    ) -> Hash {
        self.submit_pending(
            updater,
            vec![updater, addr.to_addr()],
            gas_max,
            PendingOp::Update { addr, edit },
        )
    }

    pub fn submit_action(
        &mut self,
        main: Address,
        gas_max: u8,
        action: Box<dyn basis::interface::Action>,
    ) -> Hash {
        self.submit_pending(main, vec![main], gas_max, PendingOp::Action { action })
    }

    pub fn submit_contract_call(
        &mut self,
        caller: Address,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let codes = build_commit_call_codes(func, &args)?;
        Ok(self.submit_pending(
            caller,
            vec![caller, addr.to_addr()],
            gas_max,
            PendingOp::ContractCall { codes },
        ))
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
        let deployer = account_address(account);
        let caddr = ContractAddress::calculate(&deployer, &Uint4::from(nonce));
        let mut act = vm::action::ContractDeploy::new();
        act.nonce = Uint4::from(nonce);
        act.contract = contract;
        act.protocol_cost = Amount::unit238(FORMAL_CONTRACT_PROTOCOL_COST_238);
        let tx =
            self.build_formal_type3_tx(account, &[], vec![deployer], gas_max, vec![Box::new(act)])?;
        let hash = self.submit_formal_pending(tx, TxOutput::ContractAddress(caddr.clone()));
        Ok((hash, caddr))
    }

    pub fn submit_formal_contract_call(
        &mut self,
        account: &Account,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let caller = account_address(account);
        let codes = build_commit_call_codes(func, &args)?;
        let act = vm::action::ContractMainCall::from_bytecode(codes)?;
        let tx = self.build_formal_type3_tx(
            account,
            &[],
            vec![caller, addr.to_addr()],
            gas_max,
            vec![Box::new(act)],
        )?;
        Ok(self.submit_formal_pending(tx, TxOutput::None))
    }

    pub fn submit_formal_main_call(
        &mut self,
        account: &Account,
        addrs: Vec<Address>,
        codes: Vec<u8>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let act = vm::action::ContractMainCall::from_bytecode(codes)?;
        self.submit_formal_actions(account, addrs, vec![Box::new(act)], gas_max, TxOutput::None)
    }

    pub fn submit_formal_main_call_with_signers(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        codes: Vec<u8>,
        gas_max: u8,
    ) -> Ret<Hash> {
        let act = vm::action::ContractMainCall::from_bytecode(codes)?;
        self.submit_formal_actions_with_signers(
            account,
            extra_signers,
            addrs,
            vec![Box::new(act)],
            gas_max,
            TxOutput::None,
        )
    }

    pub fn submit_formal_main_call_fitsh(
        &mut self,
        account: &Account,
        addrs: Vec<Address>,
        src: &str,
        gas_max: u8,
    ) -> Ret<Hash> {
        let codes = vm::lang::lang_to_bytecode(src)?;
        self.submit_formal_main_call(account, addrs, codes, gas_max)
    }

    pub fn submit_formal_main_call_fitsh_with_signers(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        src: &str,
        gas_max: u8,
    ) -> Ret<Hash> {
        let codes = vm::lang::lang_to_bytecode(src)?;
        self.submit_formal_main_call_with_signers(account, extra_signers, addrs, codes, gas_max)
    }

    /// Submit a signed Type3 transaction with an arbitrary top-level action list.
    ///
    /// `addrs` is the transaction address list and must start with the account's
    /// address, because Type3 derives `tx.main()` from `addrlist[0]`.
    pub fn submit_formal_actions(
        &mut self,
        account: &Account,
        addrs: Vec<Address>,
        actions: Vec<Box<dyn basis::interface::Action>>,
        gas_max: u8,
        output: TxOutput,
    ) -> Ret<Hash> {
        let tx = self.build_formal_type3_tx(account, &[], addrs, gas_max, actions)?;
        Ok(self.submit_formal_pending(tx, output))
    }

    /// Submit a signed Type3 transaction with additional non-main signers.
    ///
    /// This covers normal user-submittable transactions where the fee payer is
    /// not the only privkey address required by the action list.
    pub fn submit_formal_actions_with_signers(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        actions: Vec<Box<dyn basis::interface::Action>>,
        gas_max: u8,
        output: TxOutput,
    ) -> Ret<Hash> {
        let tx = self.build_formal_type3_tx(account, extra_signers, addrs, gas_max, actions)?;
        Ok(self.submit_formal_pending(tx, output))
    }

    /// Submit an externally-built raw TransactionType3 byte stream.
    ///
    /// This is the closest testkit entry to a wallet/RPC submission path: the
    /// transaction is parsed through the production codec and later executed by
    /// `BlockV1::execute_with_report`. The caller supplies the expected test
    /// output because the chain does not infer VM return values from raw bytes.
    pub fn submit_formal_raw(&mut self, raw: &[u8], output: TxOutput) -> Ret<Hash> {
        let parsed = Self::parse_formal_type3_raw(raw)?;
        let hash = parsed.hash();
        self.pending.push(PendingTx {
            tx: PendingTxKind::Formal(parsed),
            op: PendingOp::FormalTx { output },
        });
        Ok(hash)
    }

    pub fn build_formal_actions_raw(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        actions: Vec<Box<dyn basis::interface::Action>>,
        gas_max: u8,
    ) -> Ret<Vec<u8>> {
        let tx = self.build_formal_type3_tx(account, extra_signers, addrs, gas_max, actions)?;
        Ok(tx.serialize())
    }

    pub fn formal_raw_hash(raw: &[u8]) -> Ret<Hash> {
        let parsed = Self::parse_formal_type3_raw(raw)?;
        Ok(parsed.hash())
    }

    pub fn mine_block(&mut self) -> BlockReceipt {
        self.height += 1;
        let pending = std::mem::take(&mut self.pending);
        let mut receipts = Vec::with_capacity(pending.len());
        for tx in pending {
            let receipt = self.apply_pending_tx(tx);
            self.receipts.push(receipt.clone());
            receipts.push(receipt);
        }
        BlockReceipt {
            height: self.height,
            receipts,
        }
    }

    pub fn mine_block_containing(&mut self, hash: &Hash) -> Ret<TxReceipt> {
        if !self
            .pending
            .iter()
            .any(|pending| &pending.tx.hash() == hash)
        {
            return Err(format!("tx {} is not pending", hash));
        }
        self.mine_block()
            .into_iter()
            .find(|receipt| &receipt.tx_hash == hash)
            .ok_or_else(|| format!("tx {} was not included in mined block", hash))
    }

    pub fn confirm_formal_block(&mut self, miner: Address) -> Ret<ConfirmedBlockReceipt> {
        let next_height = self.height.saturating_add(1);
        let mut block = BlockV1::new();
        block.intro.head.height = BlockHeight::from(next_height);
        block.intro.head.timestamp = Timestamp::from(FORMAL_TX_TIMESTAMP_BASE + next_height);
        block.intro.head.prevhash = self.last_block_hash;

        let mut prelude = DefaultPreludeTx::default();
        prelude.address = miner;
        prelude.message = Fixed16::default();
        block.push_transaction(Box::new(prelude))?;

        let mut receipt_inputs = Vec::with_capacity(self.pending.len());
        for pending in &self.pending {
            match (&pending.tx, &pending.op) {
                (PendingTxKind::Formal(tx), PendingOp::FormalTx { output }) => {
                    receipt_inputs.push((tx.hash(), output.clone()));
                    block.push_transaction(tx.clone())?;
                }
                _ => {
                    return Err(
                        "confirm_formal_block only accepts formal TransactionType3 pending txs"
                            .to_owned(),
                    );
                }
            }
        }
        block.update_mrklroot();
        let block_hash = block.hash();

        let tx_state = self.state.fork_for_tx();
        let logs_box: Box<dyn Logs> = Box::new(self.logs.clone());
        let old_log_len = logs_box.snapshot_len();
        let executed = block.execute_with_report(
            ChainInfo {
                id: 0,
                fast_sync: false,
                diamond_form: false,
            },
            tx_state.state,
            logs_box,
        )?;
        let report = executed.report.clone();
        assert_eq!(
            report.tx_count,
            receipt_inputs.len() + 1,
            "confirmed block report tx count mismatch"
        );

        let new_logs = collect_new_logs(executed.logs.as_ref(), old_log_len);
        self.state.commit(executed.state);
        self.logs.extend_from(&new_logs);
        self.height = next_height;
        self.last_block_hash = block_hash;
        self.pending.clear();

        let receipts: Vec<_> = receipt_inputs
            .into_iter()
            .map(|(tx_hash, output)| TxReceipt {
                tx_hash,
                height: self.height,
                success: true,
                output,
                error: None,
                log_count: new_logs.snapshot_len(),
                gas_used: None,
            })
            .collect();
        self.receipts.extend(receipts.clone());

        Ok(ConfirmedBlockReceipt {
            height: self.height,
            block_hash,
            receipts,
            report,
        })
    }

    /// Confirm one formal block with only the prelude/coinbase transaction.
    ///
    /// This keeps long-range height advancement on the real block execution
    /// path without manufacturing unrelated user transfers. Pending user
    /// transactions must be confirmed or dropped first so tests do not skip
    /// work accidentally.
    pub fn confirm_empty_formal_block(&mut self, miner: Address) -> Ret<ConfirmedBlockReceipt> {
        if !self.pending.is_empty() {
            return Err(format!(
                "confirm_empty_formal_block requires empty pending pool, got {} txs",
                self.pending.len()
            ));
        }

        let next_height = self.height.saturating_add(1);
        let mut block = BlockV1::new();
        block.intro.head.height = BlockHeight::from(next_height);
        block.intro.head.timestamp = Timestamp::from(FORMAL_TX_TIMESTAMP_BASE + next_height);
        block.intro.head.prevhash = self.last_block_hash;

        let mut prelude = DefaultPreludeTx::default();
        prelude.address = miner;
        prelude.message = Fixed16::default();
        block.push_transaction(Box::new(prelude))?;
        block.update_mrklroot();
        let block_hash = block.hash();

        let tx_state = self.state.fork_for_tx();
        let logs_box: Box<dyn Logs> = Box::new(self.logs.clone());
        let old_log_len = logs_box.snapshot_len();
        let executed = block.execute_with_report(
            ChainInfo {
                id: 0,
                fast_sync: false,
                diamond_form: false,
            },
            tx_state.state,
            logs_box,
        )?;
        let report = executed.report.clone();
        assert_eq!(
            report.tx_count, 1,
            "empty confirmed block report tx count mismatch"
        );

        let new_logs = collect_new_logs(executed.logs.as_ref(), old_log_len);
        self.state.commit(executed.state);
        self.logs.extend_from(&new_logs);
        self.height = next_height;
        self.last_block_hash = block_hash;

        Ok(ConfirmedBlockReceipt {
            height: self.height,
            block_hash,
            receipts: Vec::new(),
            report,
        })
    }

    /// Confirm `blocks` empty formal blocks.
    ///
    /// This is a batch convenience over [`MemChain::confirm_empty_formal_block`]:
    /// it keeps the real formal block execution/report path, requires an empty
    /// pending pool, and returns only the first and last block receipts.
    pub fn confirm_empty_formal_blocks(
        &mut self,
        miner: Address,
        blocks: u64,
    ) -> Ret<ConfirmedEmptyBlockBatchReceipt> {
        let start_height = self.height;
        if blocks == 0 {
            return Ok(ConfirmedEmptyBlockBatchReceipt {
                start_height,
                end_height: self.height,
                count: 0,
                first_block: None,
                last_block: None,
            });
        }
        if !self.pending.is_empty() {
            return Err(format!(
                "confirm_empty_formal_blocks requires empty pending pool, got {} txs",
                self.pending.len()
            ));
        }

        let mut first_block = None;
        let mut last_block = None;
        for idx in 0..blocks {
            let block = self.confirm_empty_formal_block(miner)?;
            if idx == 0 {
                first_block = Some(block.clone());
            }
            last_block = Some(block);
        }

        Ok(ConfirmedEmptyBlockBatchReceipt {
            start_height,
            end_height: self.height,
            count: blocks,
            first_block,
            last_block,
        })
    }

    /// Confirm empty formal blocks until the chain reaches `target_height`.
    ///
    /// If the chain is already at or above `target_height`, this is a no-op.
    pub fn confirm_empty_formal_blocks_to_height(
        &mut self,
        miner: Address,
        target_height: u64,
    ) -> Ret<ConfirmedEmptyBlockBatchReceipt> {
        let blocks = target_height.saturating_sub(self.height);
        self.confirm_empty_formal_blocks(miner, blocks)
    }

    /// Confirm all pending formal transactions while preserving a receipt for
    /// transactions that fail during execution.
    ///
    /// `BlockV1::execute_with_report` is still used by
    /// [`confirm_formal_block`] for the all-success path. This helper is a
    /// testkit observability path for negative tests: each pending formal
    /// Type3 transaction is executed through the same `tx.execute(ctx)` entry,
    /// success commits its forked state/logs, and failure rolls that tx back
    /// while still returning a failed receipt and draining it from pending.
    pub fn confirm_formal_block_observing_failures(
        &mut self,
        miner: Address,
    ) -> Ret<ConfirmedBlockReceipt> {
        let next_height = self.height.saturating_add(1);
        let mut block = BlockV1::new();
        block.intro.head.height = BlockHeight::from(next_height);
        block.intro.head.timestamp = Timestamp::from(FORMAL_TX_TIMESTAMP_BASE + next_height);
        block.intro.head.prevhash = self.last_block_hash;

        let mut prelude = DefaultPreludeTx::default();
        prelude.address = miner;
        prelude.message = Fixed16::default();
        block.push_transaction(Box::new(prelude))?;

        for pending in &self.pending {
            match &pending.tx {
                PendingTxKind::Formal(tx) => block.push_transaction(tx.clone())?,
                PendingTxKind::Stub(_) => {
                    return Err(
                        "confirm_formal_block_observing_failures only accepts formal TransactionType3 pending txs"
                            .to_owned(),
                    );
                }
            }
        }
        block.update_mrklroot();
        let block_hash = block.hash();

        self.height = next_height;
        self.last_block_hash = block_hash;
        let pending = std::mem::take(&mut self.pending);
        let mut receipts = Vec::with_capacity(pending.len());
        let mut reports = Vec::with_capacity(pending.len() + 1);
        reports.push(protocol::block::BlockTxExecutionReport {
            index: 0,
            tx_hash: block.transactions()[0].hash(),
            tx_type: block.transactions()[0].ty(),
            fee_got: block.transactions()[0].fee_got(),
            gas_used: 0,
        });

        for (idx, pending) in pending.into_iter().enumerate() {
            let tx_hash = pending.tx.hash();
            let output = match pending.op {
                PendingOp::FormalTx { output } => output,
                _ => {
                    return Err(
                        "confirm_formal_block_observing_failures only accepts formal TransactionType3 pending txs"
                            .to_owned(),
                    );
                }
            };
            let result = self.run_formal_tx_with_tx(pending.tx.as_read());
            let receipt = match result {
                Ok(outcome) => {
                    let log_count = outcome.logs.snapshot_len();
                    let gas_used = if outcome.gas_used > 0 {
                        Some(outcome.gas_used)
                    } else {
                        None
                    };
                    self.commit(outcome);
                    reports.push(protocol::block::BlockTxExecutionReport {
                        index: idx + 1,
                        tx_hash,
                        tx_type: pending.tx.as_read().ty(),
                        fee_got: pending.tx.as_read().fee_got(),
                        gas_used: gas_used.unwrap_or(0),
                    });
                    TxReceipt {
                        tx_hash,
                        height: self.height,
                        success: true,
                        output,
                        error: None,
                        log_count,
                        gas_used,
                    }
                }
                Err(error) => TxReceipt {
                    tx_hash,
                    height: self.height,
                    success: false,
                    output: TxOutput::None,
                    error: Some(error),
                    log_count: 0,
                    gas_used: None,
                },
            };
            self.receipts.push(receipt.clone());
            receipts.push(receipt);
        }

        Ok(ConfirmedBlockReceipt {
            height: self.height,
            block_hash,
            receipts,
            report: protocol::block::BlockExecutionReport {
                height: self.height,
                hash: block_hash,
                tx_count: reports.len(),
                total_fee: Amount::zero(),
                fee_receiver: Some(miner),
                txs: reports,
            },
        })
    }

    /// Credit `sat` satoshis of HAC to `addr`.
    pub fn mint_hac(&mut self, addr: &Address, sat: u64) {
        self.fund(addr, Amount::unit238(sat));
    }

    /// Credit `amount` to `addr` directly on the persistent state.
    pub fn fund(&mut self, addr: &Address, amount: Amount) {
        let tx = StubTxBuilder::new().ty(3).main(*addr).gas_max(0).build();
        let outcome = self.run_with(&tx, |ctx| {
            protocol::operate::hac_add(ctx, addr, &amount)?;
            Ok(())
        });
        if let Ok((_, outcome)) = outcome {
            self.commit(outcome);
        }
    }

    /// Credit `sat` satoshis directly on the persistent state.
    pub fn fund_sat(&mut self, addr: &Address, sat: u64) {
        let tx = StubTxBuilder::new().ty(3).main(*addr).gas_max(0).build();
        let outcome = self.run_with(&tx, |ctx| {
            protocol::operate::sat_add(ctx, addr, &Satoshi::from(sat))?;
            Ok(())
        });
        if let Ok((_, outcome)) = outcome {
            self.commit(outcome);
        }
    }

    /// Seed one HACD diamond as owned by `addr`.
    pub fn fund_diamond(&mut self, addr: &Address, diamond: DiamondName) {
        self.fund_diamond_with_smelt(addr, diamond, 1, 1);
    }

    /// Seed one HACD diamond plus its smelt metadata as owned by `addr`.
    ///
    /// Production-mined diamonds always have both `DiamondSto` and
    /// `DiamondSmelt`. VM tests that exercise inscription protocol costs need
    /// the smelt side too, otherwise the real action path fails before it can
    /// validate ownership/content/cooldown semantics.
    pub fn fund_diamond_with_smelt(
        &mut self,
        addr: &Address,
        diamond: DiamondName,
        number: u32,
        average_bid_burn_mei: u16,
    ) {
        let tx = StubTxBuilder::new().ty(3).main(*addr).gas_max(0).build();
        let height = self.height;
        let prev_hash = self.last_block_hash;
        let outcome = self.run_with(&tx, |ctx| {
            let mut state = protocol::state::CoreState::wrap(ctx.state());
            state.diamond_set(
                &diamond,
                &DiamondSto {
                    status: DIAMOND_STATUS_NORMAL,
                    address: *addr,
                    prev_engraved_height: BlockHeight::from(height),
                    inscripts: Default::default(),
                },
            );
            state.diamond_smelt_set(
                &diamond,
                &DiamondSmelt {
                    diamond,
                    number: DiamondNumber::from(number),
                    born_height: BlockHeight::from(height),
                    born_hash: Hash::default(),
                    prev_hash,
                    miner_address: *addr,
                    bid_fee: Amount::zero(),
                    nonce: Fixed8::default(),
                    average_bid_burn: Uint2::from(average_bid_burn_mei),
                    life_gene: Hash::default(),
                },
            );
            protocol::operate::hacd_add(&mut state, addr, &DiamondNumber::from(1u32))?;
            Ok(())
        });
        if let Ok((_, outcome)) = outcome {
            self.commit(outcome);
        }
    }

    /// Seed an issued asset balance for `addr`.
    pub fn fund_asset(&mut self, addr: &Address, asset: AssetAmt) {
        let tx = StubTxBuilder::new().ty(3).main(*addr).gas_max(0).build();
        let outcome = self.run_with(&tx, |ctx| {
            let mut state = protocol::state::CoreState::wrap(ctx.state());
            let smelt = AssetSmelt {
                serial: asset.serial,
                supply: Fold64::from(asset.amount.uint().max(1)).unwrap(),
                decimal: Uint1::from(0u8),
                issuer: *addr,
                ticket: BytesW1::from_str("TSTAST")?,
                name: BytesW1::from_str("Test Asset")?,
            };
            state.asset_set(&asset.serial, &smelt);
            protocol::operate::asset_add(&mut state, addr, &asset)?;
            Ok(())
        });
        if let Ok((_, outcome)) = outcome {
            self.commit(outcome);
        }
    }

    /// Read the HAC balance of `addr` from the persistent state.
    pub fn balance(&self, addr: &Address) -> Amount {
        let mut state = self.state.clone_state();
        let state_dyn: &mut dyn State = state.as_mut();
        let bal = protocol::state::CoreState::wrap(state_dyn).balance(addr);
        bal.map(|b| b.hacash).unwrap_or_default()
    }

    pub fn satoshi(&self, addr: &Address) -> Satoshi {
        let mut state = self.state.clone_state();
        let state_dyn: &mut dyn State = state.as_mut();
        protocol::state::CoreState::wrap(state_dyn)
            .balance(addr)
            .map(|b| b.satoshi.to_satoshi())
            .unwrap_or_default()
    }

    pub fn diamond_count(&self, addr: &Address) -> DiamondNumber {
        let mut state = self.state.clone_state();
        let state_dyn: &mut dyn State = state.as_mut();
        protocol::state::CoreState::wrap(state_dyn)
            .balance(addr)
            .and_then(|b| b.diamond.to_diamond().ok())
            .unwrap_or_default()
    }

    pub fn diamond_owner(&self, diamond: &DiamondName) -> Option<Address> {
        let mut state = self.state.clone_state();
        let state_dyn: &mut dyn State = state.as_mut();
        protocol::state::CoreState::wrap(state_dyn)
            .diamond(diamond)
            .map(|d| d.address)
    }

    pub fn asset_balance(&self, addr: &Address, serial: Fold64) -> AssetAmt {
        let mut state = self.state.clone_state();
        let state_dyn: &mut dyn State = state.as_mut();
        protocol::state::CoreState::wrap(state_dyn)
            .balance(addr)
            .map(|b| b.asset_must(serial))
            .unwrap_or_else(|| AssetAmt::from_serial(serial).unwrap())
    }

    // ────────────────────────── main call ──────────────────────────

    /// Execute a main call (raw bytecode) end-to-end: fork state, init gas,
    /// run `Executor::main_call`, drain deferred, commit on success.
    pub fn main_call(&mut self, main: Address, codes: Vec<u8>, gas_max: u8) -> Ret<Value> {
        let tx = build_tx(main, gas_max);
        self.main_call_with_tx(&tx, codes)
    }

    /// Execute a main call with an explicit transaction address list.
    pub fn main_call_with_addrs(
        &mut self,
        main: Address,
        addrs: Vec<Address>,
        codes: Vec<u8>,
        gas_max: u8,
    ) -> Ret<Value> {
        let tx = build_tx_with_addrs(main, gas_max, addrs);
        self.main_call_with_tx(&tx, codes)
    }

    fn main_call_with_tx(&mut self, tx: &dyn TransactionRead, codes: Vec<u8>) -> Ret<Value> {
        let (rv, outcome) = self.run_main_call_with_tx(tx, codes)?;
        self.commit(outcome);
        Ok(rv)
    }

    fn run_main_call_with_tx(
        &mut self,
        tx: &dyn TransactionRead,
        codes: Vec<u8>,
    ) -> Ret<(Value, TxOutcome)> {
        self.run_with(tx, |ctx| {
            let height = ctx.env().block.height;
            let mut machine = Executor::from_runtime(Runtime::create(height));
            let rv = machine.main_call(ctx, vm::rt::CodeType::Bytecode, codes.clone().into())?;
            ctx.run_deferred_phase()?;
            Ok(rv)
        })
    }

    /// Compile a fitsh source and run it as a main call.
    pub fn main_call_fitsh(&mut self, main: Address, src: &str, gas_max: u8) -> Ret<Value> {
        let codes = vm::lang::lang_to_bytecode(src)?;
        self.main_call(main, codes, gas_max)
    }

    /// Compile a fitsh source and run it as a main call with an explicit
    /// address list, WITHOUT committing state changes. This is the read-only
    /// simulation counterpart of `main_call_with_addrs` — it runs on a state
    /// fork (via `run_with`) and discards the outcome. Used by
    /// `SimulateContractFitsh` for assertion-style state queries that should
    /// not persist mutations.
    pub fn sandbox_main_call_fitsh(
        &mut self,
        main: Address,
        addrs: Vec<Address>,
        src: &str,
        gas_max: u8,
    ) -> Ret<Value> {
        let codes = vm::lang::lang_to_bytecode(src)?;
        let tx = build_tx_with_addrs(main, gas_max, addrs);
        // run_with forks state; we deliberately drop the outcome (no commit).
        let (rv, _) = self.run_with(&tx, |ctx| {
            let height = ctx.env().block.height;
            let mut machine = Executor::from_runtime(Runtime::create(height));
            let rv = machine.main_call(ctx, vm::rt::CodeType::Bytecode, codes.clone().into())?;
            ctx.run_deferred_phase()?;
            Ok(rv)
        })?;
        Ok(rv)
    }

    // ──────────────────────── contract deploy ────────────────────────

    /// Deploy a contract under `deployer` with the given `nonce`. Returns the
    /// resulting `ContractAddress` (deterministic from main+nonce).
    pub fn deploy(
        &mut self,
        contract: ContractSto,
        deployer: Address,
        nonce: u32,
    ) -> Ret<ContractAddress> {
        let caddr = ContractAddress::calculate(&deployer, &Uint4::from(nonce));
        let tx = build_tx(deployer, 17);
        let outcome = self.run_deploy_with_tx(&tx, contract, nonce);
        let (_, outcome) = outcome?;
        self.commit(outcome);
        Ok(caddr)
    }

    fn run_deploy_with_tx(
        &mut self,
        tx: &dyn TransactionRead,
        contract: ContractSto,
        nonce: u32,
    ) -> Ret<(ContractAddress, TxOutcome)> {
        let deployer = tx.main();
        let caddr = ContractAddress::calculate(&deployer, &Uint4::from(nonce));
        let ((), outcome) = self.run_with(tx, |ctx| {
            let mut act = vm::action::ContractDeploy::new();
            act.nonce = Uint4::from(nonce);
            act.contract = contract.clone();
            act.protocol_cost = vm::action::contract_protocol_cost_min(
                ctx,
                act.contract.size(),
                protocol::params::CONTRACT_STORE_PERM_PERIODS,
            )?;
            act.execute(ctx)?;
            ctx.run_deferred_phase()?;
            Ok(())
        })?;
        Ok((caddr, outcome))
    }

    // ──────────────────────── contract update ──────────────────────

    /// Update a deployed contract with `edit` (revision bump).
    pub fn update(
        &mut self,
        addr: ContractAddress,
        edit: ContractEdit,
        updater: Address,
    ) -> Ret<()> {
        let tx = build_tx(updater, 17);
        let (_, outcome) = self.run_update_with_tx(&tx, addr, edit)?;
        self.commit(outcome);
        Ok(())
    }

    pub fn analyze_contract_update(
        &mut self,
        addr: ContractAddress,
        edit: ContractEdit,
        updater: Address,
    ) -> Ret<vm::action::ContractUpdateAnalysis> {
        let tx = build_tx(updater, 17);
        let (analysis, _) = self.run_with(&tx, |ctx| {
            vm::action::analyze_contract_update(ctx, &addr, &edit)
        })?;
        Ok(analysis)
    }

    fn run_update_with_tx(
        &mut self,
        tx: &dyn TransactionRead,
        addr: ContractAddress,
        edit: ContractEdit,
    ) -> Ret<((), TxOutcome)> {
        self.run_with(tx, |ctx| {
            let mut act = vm::action::ContractUpdate::new();
            act.address = addr.to_addr();
            act.edit = edit.clone();
            act.protocol_cost = vm::action::contract_protocol_cost_min(
                ctx,
                act.edit.size(),
                protocol::params::CONTRACT_STORE_PERM_PERIODS,
            )?;
            act.execute(ctx)?;
            ctx.run_deferred_phase()?;
            Ok(())
        })
    }

    // ──────────────────────── arbitrary tx ──────────────────────────

    /// Run an arbitrary action as a full transaction.
    pub fn tx_run(
        &mut self,
        main: Address,
        gas_max: u8,
        action: Box<dyn basis::interface::Action>,
    ) -> Ret<()> {
        let tx = build_tx(main, gas_max);
        let outcome = self.run_action_with_tx(&tx, action);
        let (_, outcome) = outcome?;
        self.commit(outcome);
        Ok(())
    }

    fn run_action_with_tx(
        &mut self,
        tx: &dyn TransactionRead,
        action: Box<dyn basis::interface::Action>,
    ) -> Ret<((), TxOutcome)> {
        self.run_with(tx, |ctx| {
            action.execute(ctx)?;
            ctx.run_deferred_phase()?;
            Ok(())
        })
    }

    // ────────────────────────── sandbox ─────────────────────────────

    /// Run a sandboxed contract call against a snapshot of the state (no state
    /// changes persist to the chain). Returns gas used + return value.
    ///
    /// The caller is taken from `spec.caller`, or falls back to the chain's
    /// well-known test address `vm_main_addr` (already funded). Sandbox calls
    /// require the caller to carry a small HAC balance for the sandbox tx fee.
    pub fn sandbox(&mut self, spec: SandboxSpec) -> Ret<SandboxResult> {
        let caller = spec
            .caller
            .unwrap_or_else(|| crate::sim::integration::vm_main_addr());
        let tx = build_tx(caller, 17);
        // sandbox runs on its own state clone; do not commit.
        let outcome = self.run_with(&tx, |ctx| {
            let res = machine::sandbox_call(ctx, spec)?;
            Ok(res)
        });
        let (res, _) = outcome?;
        Ok(res)
    }

    /// Convenience: call a contract function by name with args (sandbox path).
    /// Note: the sandbox path runs on a state clone and does NOT persist state
    /// changes back to the chain. Use [`MemChain::call_func_commit`] for calls
    /// whose state mutations should be committed.
    pub fn call_func(
        &mut self,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
    ) -> Ret<SandboxResult> {
        let spec = SandboxSpec::new(addr, func).args(args);
        self.sandbox(spec)
    }

    pub fn call_func_from(
        &mut self,
        caller: Address,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
    ) -> Ret<SandboxResult> {
        let spec = SandboxSpec::new(addr, func).args(args).caller(caller);
        self.sandbox(spec)
    }

    /// Call a contract function as a real (committing) transaction: builds a
    /// main call that references the deployed contract as address-list index 1
    /// and invokes `func`, then commits the resulting state changes to the chain.
    /// The contract return value is discarded; the surrounding main call returns
    /// nil so successful mutating calls can return non-status values internally.
    pub fn call_func_commit(
        &mut self,
        caller: Address,
        addr: ContractAddress,
        func: &str,
        gas_max: u8,
    ) -> Ret<Value> {
        self.call_func_commit_args(caller, addr, func, vec![], gas_max)
    }

    /// Commit a contract function call with explicit VM values as arguments.
    pub fn call_func_commit_args(
        &mut self,
        caller: Address,
        addr: ContractAddress,
        func: &str,
        args: Vec<Value>,
        gas_max: u8,
    ) -> Ret<Value> {
        let codes = build_commit_call_codes(func, &args)?;
        self.main_call_with_addrs(caller, vec![caller, addr.to_addr()], codes, gas_max)
    }

    // ────────────────────────── queries ─────────────────────────────

    /// Read the persistent contract store for `addr`.
    pub fn contract(&self, addr: &ContractAddress) -> Option<ContractSto> {
        let mut state = self.state.clone_state();
        let state_dyn: &mut dyn State = state.as_mut();
        VMState::wrap(state_dyn).contract(addr)
    }

    /// Read a rentable storage value for `key` under contract `addr`.
    /// Returns `Value::Nil` when the key does not exist or is expired.
    pub fn storage(&self, addr: &ContractAddress, key: &Value) -> Value {
        let state = self.state.clone_state();
        let state_dyn: &dyn State = state.as_ref();
        let gst = vm::rt::GasExtra::new(self.height);
        let cap = vm::rt::SpaceCap::new(self.height);
        match VMStateRead::wrap(state_dyn).debug_storage_get(
            &gst,
            &cap,
            self.height,
            &addr.to_addr(),
            key,
        ) {
            Ok(Some(info)) => info.value,
            _ => Value::Nil,
        }
    }

    /// Borrow the persistent state (read-only).
    pub fn state(&self) -> &dyn State {
        self.state.state()
    }

    /// Number of log entries accumulated so far.
    pub fn log_count(&self) -> usize {
        self.logs.snapshot_len()
    }

    /// Read one raw VM log entry from the in-memory log backend.
    pub fn log_raw(&self, idx: usize) -> Option<Vec<u8>> {
        self.logs.get(idx)
    }

    /// Clear all VM logs in the in-memory log backend.
    pub fn clear_logs(&mut self) {
        self.logs.clear();
    }

    // ────────────────────────── snapshot ────────────────────────────

    /// Snapshot the current state + height for fork-style assertions.
    pub fn snapshot(&self) -> ChainSnapshot {
        assert!(
            self.pending.is_empty(),
            "MemChain::snapshot requires an empty pending tx pool; mine or discard pending txs first"
        );
        ChainSnapshot {
            state: self.state.snapshot(),
            height: self.height,
            last_block_hash: self.last_block_hash,
            log_len: self.logs.snapshot_len(),
            receipt_len: self.receipts.len(),
            next_tx_seq: self.next_tx_seq,
        }
    }

    /// Restore a previously taken snapshot.
    pub fn restore(&mut self, snap: ChainSnapshot) {
        self.state.restore(snap.state);
        self.height = snap.height;
        self.last_block_hash = snap.last_block_hash;
        self.logs.truncate(snap.log_len);
        self.receipts.truncate(snap.receipt_len);
        self.pending.clear();
        self.next_tx_seq = snap.next_tx_seq;
    }

    // ────────────────────────── internals ───────────────────────────

    fn submit_pending(
        &mut self,
        main: Address,
        addrs: Vec<Address>,
        gas_max: u8,
        op: PendingOp,
    ) -> Hash {
        let hash = self.next_tx_hash();
        let tx = build_tx_with_addrs_and_hash(main, gas_max, addrs, hash);
        self.pending.push(PendingTx {
            tx: PendingTxKind::Stub(tx),
            op,
        });
        hash
    }

    fn submit_formal_pending(&mut self, tx: TransactionType3, output: TxOutput) -> Hash {
        let hash = tx.hash();
        let raw = tx.serialize();
        let parsed = Self::parse_formal_type3_raw(&raw).expect("formal tx raw roundtrip failed");
        assert_eq!(
            parsed.as_read().hash(),
            hash,
            "formal tx raw roundtrip changed transaction hash"
        );
        self.pending.push(PendingTx {
            tx: PendingTxKind::Formal(parsed),
            op: PendingOp::FormalTx { output },
        });
        hash
    }

    fn parse_formal_type3_raw(raw: &[u8]) -> Ret<Box<dyn Transaction>> {
        let (parsed, used) = transaction_create(raw)?;
        if used != raw.len() {
            return Err(format!(
                "formal tx raw parse did not consume all bytes: used {}, total {}",
                used,
                raw.len()
            ));
        }
        if parsed.as_read().ty() != TransactionType3::TYPE {
            return Err(format!(
                "formal tx raw parse expected TransactionType3, got type {}",
                parsed.as_read().ty()
            ));
        }
        Ok(parsed)
    }

    fn build_formal_type3_tx(
        &mut self,
        account: &Account,
        extra_signers: &[&Account],
        addrs: Vec<Address>,
        gas_max: u8,
        actions: Vec<Box<dyn basis::interface::Action>>,
    ) -> Ret<TransactionType3> {
        let main = account_address(account);
        if addrs.first() != Some(&main) {
            return Err("formal Type3 addrlist must start with the signer/main address".to_owned());
        }
        let mut tx = TransactionType3::new_by(
            main,
            Amount::unit238(FORMAL_TX_FEE_238),
            self.next_formal_timestamp(),
        );
        tx.addrlist = AddrOrList::from_list(addrs)?;
        tx.gas_max = Uint1::from(gas_max);
        for action in actions {
            tx.push_action(action)?;
        }
        tx.fill_sign(account)?;
        for signer in extra_signers {
            tx.fill_sign(signer)?;
        }
        Ok(tx)
    }

    fn next_formal_timestamp(&mut self) -> u64 {
        let seq = self.next_tx_seq;
        self.next_tx_seq = self.next_tx_seq.saturating_add(1);
        FORMAL_TX_TIMESTAMP_BASE + self.height.saturating_mul(1_000_000) + seq
    }

    fn next_tx_hash(&mut self) -> Hash {
        let seq = self.next_tx_seq;
        self.next_tx_seq = self.next_tx_seq.saturating_add(1);
        let mut bytes = [0u8; Hash::SIZE];
        bytes[0..8].copy_from_slice(&self.height.to_be_bytes());
        bytes[24..32].copy_from_slice(&seq.to_be_bytes());
        Hash::from(bytes)
    }

    fn apply_pending_tx(&mut self, pending: PendingTx) -> TxReceipt {
        let tx_hash = pending.tx.hash();
        let result: Ret<(TxOutput, TxOutcome)> = match pending.op {
            PendingOp::MainCall { codes } => self
                .run_main_call_with_tx(pending.tx.as_read(), codes)
                .map(|(value, outcome)| (TxOutput::Value(value), outcome)),
            PendingOp::Deploy { contract, nonce } => self
                .run_deploy_with_tx(pending.tx.as_read(), contract, nonce)
                .map(|(addr, outcome)| (TxOutput::ContractAddress(addr), outcome)),
            PendingOp::Update { addr, edit } => self
                .run_update_with_tx(pending.tx.as_read(), addr, edit)
                .map(|(_, outcome)| (TxOutput::None, outcome)),
            PendingOp::Action { action } => self
                .run_action_with_tx(pending.tx.as_read(), action)
                .map(|(_, outcome)| (TxOutput::None, outcome)),
            PendingOp::ContractCall { codes } => self
                .run_main_call_with_tx(pending.tx.as_read(), codes)
                .map(|(value, outcome)| (TxOutput::Value(value), outcome)),
            PendingOp::FormalTx { output } => self
                .run_formal_tx_with_tx(pending.tx.as_read())
                .map(|outcome| (output, outcome)),
        };
        match result {
            Ok((output, outcome)) => {
                let log_count = outcome.logs.snapshot_len();
                let gas_used = if outcome.gas_used > 0 {
                    Some(outcome.gas_used)
                } else {
                    None
                };
                self.commit(outcome);
                TxReceipt {
                    tx_hash,
                    height: self.height,
                    success: true,
                    output,
                    error: None,
                    log_count,
                    gas_used,
                }
            }
            Err(error) => TxReceipt {
                tx_hash,
                height: self.height,
                success: false,
                output: TxOutput::None,
                error: Some(error),
                log_count: 0,
                gas_used: None,
            },
        }
    }

    /// Run `f` over a context built on a *clone* of the persistent state.
    /// Returns the function result plus the (possibly mutated) state/logs.
    fn run_with<R>(
        &mut self,
        tx: &dyn TransactionRead,
        f: impl FnOnce(&mut ContextInst<'_>) -> Ret<R>,
    ) -> Ret<(R, TxOutcome)> {
        self.run_with_gas_mode(tx, true, f)
    }

    fn run_formal_tx_with_tx(&mut self, tx: &dyn TransactionRead) -> Ret<TxOutcome> {
        let (_, outcome) = self.run_with_gas_mode(tx, false, |ctx| {
            tx.execute(ctx)?;
            Ok(())
        })?;
        Ok(outcome)
    }

    fn run_with_gas_mode<R>(
        &mut self,
        tx: &dyn TransactionRead,
        auto_init_gas: bool,
        f: impl FnOnce(&mut ContextInst<'_>) -> Ret<R>,
    ) -> Ret<(R, TxOutcome)> {
        let mut env = Env::default();
        env.block.height = self.height;
        env.tx = protocol::transaction::create_tx_info(tx);
        // Snapshot the per-tx state/logs: a fork so the persistent chain is
        // only mutated when the caller commits the outcome.
        let tx_state = self.state.fork_for_tx();
        let state = tx_state.state;
        let logs_box: Box<dyn Logs> = Box::new(self.logs.clone());
        let old_log_len = logs_box.snapshot_len();
        let mut ctx = ContextInst::new(env, state, logs_box, tx);
        if auto_init_gas {
            // Init the gas budget from the tx's own gas_max byte (falls back to
            // a sensible default when the tx carries no gas_max, e.g. ty < 3).
            let gas_max = ctx.tx().gas_max_byte().unwrap_or(17);
            self.init_gas(&mut ctx, gas_max)?;
        }
        let rv = f(&mut ctx)?;
        // Capture net gas used before the context is released, so per-tx
        // receipts can surface gas consumption for assertions.
        let gas_used = ctx.gas_diag().used_net;
        let (state, logs) = ctx.release();

        let new_logs = collect_new_logs(logs.as_ref(), old_log_len);
        Ok((
            rv,
            TxOutcome {
                state,
                logs: new_logs,
                gas_used,
            },
        ))
    }

    /// Commit a transaction outcome back into the persistent chain.
    fn commit(&mut self, outcome: TxOutcome) {
        self.state.commit(outcome.state);
        self.logs.extend_from(&outcome.logs);
    }

    /// Initialise the gas budget from a `gas_max` byte.
    fn init_gas(&self, ctx: &mut dyn Context, gas_max: u8) -> Ret<()> {
        let gmx = gas_max.min(TX_GAS_BUDGET_CAP_BYTE);
        if gmx > 0 {
            let budget = decode_gas_budget(gmx);
            ctx.gas_initialize(budget)?;
        }
        Ok(())
    }
}

/// A point-in-time snapshot of the chain for fork-style assertions.
pub struct ChainSnapshot {
    pub state: ChainStateSnapshot,
    pub height: u64,
    pub last_block_hash: Hash,
    pub log_len: usize,
    receipt_len: usize,
    next_tx_seq: u64,
}

// ─────────────────────────── helpers ───────────────────────────

/// Build a `StubTx` with the given main address and gas_max byte (mirrors the
/// transaction shape used by the existing action-coverage tests).
fn build_tx(main: Address, gas_max: u8) -> StubTx {
    build_tx_with_addrs(main, gas_max, vec![main])
}

fn build_tx_with_addrs(main: Address, gas_max: u8, addrs: Vec<Address>) -> StubTx {
    build_tx_with_addrs_and_hash(main, gas_max, addrs, Hash::default())
}

fn build_tx_with_addrs_and_hash(
    main: Address,
    gas_max: u8,
    addrs: Vec<Address>,
    hash: Hash,
) -> StubTx {
    StubTxBuilder::new()
        .ty(3)
        .hash(hash)
        .main(main)
        .addrs(addrs)
        .fee(Amount::unit238(10_000_000))
        .gas_max(gas_max)
        .tx_size(128)
        .fee_purity(3200)
        .build()
}

pub fn account_address(account: &Account) -> Address {
    Address::from(*account.address())
}

fn collect_new_logs(logs: &dyn Logs, old_log_len: usize) -> MemLogs {
    let new_len = logs.snapshot_len();
    let mut new_logs = MemLogs::default();
    for i in old_log_len..new_len {
        if let Some(entry) = logs.load(0, i) {
            new_logs.push(&entry as &dyn Serialize);
        }
    }
    new_logs
}

fn build_commit_call_codes(func: &str, args: &[Value]) -> Ret<Vec<u8>> {
    let mut codes = machine::build_call_codes(func, args)?;
    match codes.pop() {
        Some(op) if op == vm::rt::Bytecode::RET as u8 => {}
        _ => return Err("contract call bytecode shape invalid".to_owned()),
    }
    codes.push(vm::rt::Bytecode::POP as u8);
    codes.push(vm::rt::Bytecode::PNIL as u8);
    codes.push(vm::rt::Bytecode::RET as u8);
    Ok(codes)
}
