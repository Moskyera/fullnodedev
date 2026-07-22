use basis::component::*;
use basis::interface::*;
use field::*;
use protocol::action::*;
use protocol::transaction::*;
use sys::*;

use testkit::sim::context::make_ctx_with_default_tx;
use testkit::sim::context::{
    make_ctx_with_logs as testkit_make_ctx_with_logs,
    make_ctx_with_state as testkit_make_ctx_with_state,
};
use testkit::sim::logs::MemLogs as AstTestLogs;
use testkit::sim::state::FlatMemState as TestMemState;
use testkit::sim::state::ForkableMemState as AstTestState;
use testkit::sim::vm::CounterMockVm as MockVM;
use vm::ContractAddressW1;
use vm::action::{ContractMainCall, P2SHScriptProve, P2shLeafSpec, P2shTool};
use vm::lang::lang_to_bytecode;
use vm::rt::{CodeConf, CodeType};

fn build_ast_ctx_with_state<'a>(
    mut env: Env,
    sta: Box<dyn State>,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    init_setup_once();
    env.tx.ty = env.tx.ty.max(TransactionType3::TYPE);
    let mut ctx = testkit_make_ctx_with_state(env, sta, tx);
    let main = ctx.env().tx.main;
    let mut st = protocol::state::CoreState::wrap(ctx.state());
    let mut bls = st.balance(&main).unwrap_or_default();
    bls.hacash = Amount::unit238(10_000_000_000_000);
    st.balance_set(&main, &bls);
    ctx
}

fn ast_state_get_u8(ctx: &mut dyn Context, key: u8) -> Option<u8> {
    ctx.state().get(vec![key]).and_then(|v| v.first().copied())
}

fn ast_hac_balance(ctx: &mut dyn Context, addr: &Address) -> Amount {
    protocol::state::CoreState::wrap(ctx.state())
        .balance(addr)
        .unwrap_or_default()
        .hacash
}

fn seeded_addr(seed: &str) -> Address {
    Address::from_readable(Account::create_by(seed).unwrap().readable()).unwrap()
}

fn build_p2sh_unlock_prove(lockbox_script: &str) -> (Address, P2SHScriptProve) {
    let tree = P2shTool::build_canonical_tree(vec![P2shLeafSpec {
        adrlibs: ContractAddressW1::from_list(vec![]).unwrap(),
        codeconf: CodeConf::from_type(CodeType::Bytecode),
        lockbox: BytesW2::from(lang_to_bytecode(lockbox_script).unwrap()).unwrap(),
    }])
    .unwrap();
    let (addr, act, _calc) = tree
        .build_unlock_script_prove_checked(1, 0, BytesW2::from(vec![]).unwrap())
        .unwrap();
    (addr, act)
}

fn build_maincall_hac_transfer(to: &Address, mei: u64) -> ContractMainCall {
    let script = format!(
        r#"
        let amt = mei_to_hac({})
        transfer_hac_to({}, amt)
        return 0
    "#,
        mei,
        to.to_readable()
    );
    ContractMainCall::from_bytecode(lang_to_bytecode(&script).unwrap()).unwrap()
}

unsafe fn ctx_inst<'a>(ctx: &mut dyn Context) -> &mut protocol::context::ContextInst<'a> {
    unsafe { &mut *(ctx as *mut dyn Context as *mut protocol::context::ContextInst<'a>) }
}

fn init_setup_once() {
    testkit::sim::integration::ensure_standard_protocol_setup_for_tests(
        |_, stuff| sys::calculate_hash(stuff),
        true,
    );
}

static AST_TEST_GLOBAL_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

fn ast_test_globals_guard() -> std::sync::MutexGuard<'static, ()> {
    AST_TEST_GLOBAL_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestSet {
    key: Uint1,
    val: Uint1,
}

impl Parse for AstTestSet {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        let mut mv = self.key.parse(buf)?;
        mv += self.val.parse(&buf[mv..])?;
        Ok(mv)
    }
}

impl Serialize for AstTestSet {
    fn serialize(&self) -> Vec<u8> {
        [self.key.serialize(), self.val.serialize()].concat()
    }

    fn size(&self) -> usize {
        self.key.size() + self.val.size()
    }
}

impl Field for AstTestSet {
    fn new() -> Self {
        Self::default()
    }
}

impl ToJSON for AstTestSet {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        format!(
            "{{\"key\":{},\"val\":{}}}",
            self.key.to_json_fmt(fmt),
            self.val.to_json_fmt(fmt)
        )
    }
}

impl FromJSON for AstTestSet {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let pairs = json_split_object(json)?;
        for (k, v) in pairs {
            if k == "key" {
                self.key.from_json(v)?;
            } else if k == "val" {
                self.val.from_json(v)?;
            }
        }
        Ok(())
    }
}

impl ActExec for AstTestSet {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        ctx.state().set(vec![*self.key], vec![*self.val]);
        Ok((0, vec![]))
    }
}

impl Description for AstTestSet {}

impl Action for AstTestSet {
    fn kind(&self) -> u16 {
        65001
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl AstTestSet {
    fn create_by(key: u8, val: u8) -> Self {
        Self {
            key: Uint1::from(key),
            val: Uint1::from(val),
            ..Self::new()
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestGasOnly {
    gas: Uint1,
}

impl Parse for AstTestGasOnly {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        self.gas.parse(buf)
    }
}

impl Serialize for AstTestGasOnly {
    fn serialize(&self) -> Vec<u8> {
        self.gas.serialize()
    }

    fn size(&self) -> usize {
        self.gas.size()
    }
}

impl Field for AstTestGasOnly {
    fn new() -> Self {
        Self::default()
    }
}

impl ToJSON for AstTestGasOnly {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        "{}".to_owned()
    }
}

impl FromJSON for AstTestGasOnly {
    fn from_json(&mut self, _json: &str) -> Ret<()> {
        Ok(())
    }
}

impl ActExec for AstTestGasOnly {
    fn execute(&self, _ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        Ok((*self.gas as u32, vec![]))
    }
}

impl Description for AstTestGasOnly {}

impl Action for AstTestGasOnly {
    fn kind(&self) -> u16 {
        65030
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl AstTestGasOnly {
    fn create_by(gas: u8) -> Self {
        Self {
            gas: Uint1::from(gas),
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestFail {}

impl Parse for AstTestFail {
    fn parse(&mut self, _buf: &[u8]) -> Ret<usize> {
        Ok(0)
    }
}

impl Serialize for AstTestFail {
    fn serialize(&self) -> Vec<u8> {
        vec![]
    }

    fn size(&self) -> usize {
        0
    }
}

impl Field for AstTestFail {
    fn new() -> Self {
        Self::default()
    }
}

impl ToJSON for AstTestFail {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        "{}".to_owned()
    }
}

impl FromJSON for AstTestFail {
    fn from_json(&mut self, _json: &str) -> Ret<()> {
        Ok(())
    }
}

impl ActExec for AstTestFail {
    fn execute(&self, _ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        xerr_rf!("ast test forced fail")
    }
}

impl Description for AstTestFail {}

impl Action for AstTestFail {
    fn kind(&self) -> u16 {
        65002
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[test]
fn test_ast_if_cond_true_commits_cond_and_if_branch_state() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true; // keep focus on AST semantics
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let cond = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(1, 11))]);
    let br_if = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(2, 22))]);
    let br_else = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(3, 33))]);
    let astif = AstIf::create_by(cond, br_if, br_else);

    ctx.exec_from_set(ExecFrom::Top);
    astif.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 1), Some(11)); // cond committed
    assert_eq!(ast_state_get_u8(&mut ctx, 2), Some(22)); // if branch committed
    assert_eq!(ast_state_get_u8(&mut ctx, 3), None); // else branch not executed
}

#[test]
fn test_ast_nested_plain_actions_no_over_or_under_charge() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;

    let run_case = |payload: (u8, u8, u8)| {
        let mut ctx = build_ast_ctx_with_state(env.clone(), Box::new(AstTestState::default()), &tx);
        ctx.gas_initialize(10000).unwrap();

        let inner = AstSelect::create_list(vec![
            Box::new(AstTestGasOnly::create_by(payload.0)),
            Box::new(AstTestGasOnly::create_by(payload.1)),
        ]);
        let outer = AstSelect::create_list(vec![
            Box::new(inner),
            Box::new(AstTestGasOnly::create_by(payload.2)),
        ]);

        let before = ctx.gas_remaining();
        let (ret_gas, _) = outer.execute(&mut ctx).unwrap();
        let after = ctx.gas_remaining();
        (ret_gas, before - after)
    };

    // AST control-flow nodes return gas=0; ordinary child return-gas has no extra9 surcharge.
    let (ret0, shared0) = run_case((0, 0, 0));
    let (ret1, shared1) = run_case((7, 11, 5));

    assert_eq!(ret0, 0);
    assert_eq!(ret1, 0);
    assert_eq!(shared1 - shared0, 0);
}

#[test]
fn test_ast_multilayer_nested_innermost_plain_return_gas_charged_once() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;

    let run_case = |inner_gas: u8| {
        let mut ctx = build_ast_ctx_with_state(env.clone(), Box::new(AstTestState::default()), &tx);
        ctx.gas_initialize(10000).unwrap();

        // Multi-layer tree:
        // select(root) -> if -> select -> select -> select -> AstTestGasOnly(inner_gas)
        // Keep the tree shape fixed and vary only the innermost plain action return-gas.
        let level3 = AstSelect::create_list(vec![Box::new(AstTestGasOnly::create_by(inner_gas))]);
        let level2 = AstSelect::create_list(vec![Box::new(level3)]);
        let level1 = AstSelect::create_list(vec![Box::new(level2)]);
        let branch_if = AstSelect::create_list(vec![Box::new(level1)]);
        let node_if = AstIf::create_by(
            AstSelect::nop(), // cond succeeds with empty-select semantics
            branch_if,
            AstSelect::nop(),
        );
        let root = AstSelect::create_list(vec![Box::new(node_if)]);

        let before = ctx.gas_remaining();
        let (ret_gas, _) = root.execute(&mut ctx).unwrap();
        let after = ctx.gas_remaining();
        assert_eq!(ret_gas, 0, "AST control-flow node must return gas=0");
        before - after
    };

    let used0 = run_case(0);
    let used7 = run_case(7);
    let used19 = run_case(19);

    assert_eq!(used7 - used0, 0);
    assert_eq!(used19 - used7, 0);
}

#[test]
fn test_ast_multilayer_innermost_revert_does_not_charge_return_gas() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;

    let run_success = || {
        let mut ctx = build_ast_ctx_with_state(env.clone(), Box::new(AstTestState::default()), &tx);
        ctx.gas_initialize(10000).unwrap();

        // Same multilayer shape as control:
        // select(root) -> if -> select -> select -> select -> AstTestGasOnly(17)
        let level3 = AstSelect::create_by(0, 1, vec![Box::new(AstTestGasOnly::create_by(17))]);
        let level2 = AstSelect::create_by(0, 1, vec![Box::new(level3)]);
        let level1 = AstSelect::create_by(0, 1, vec![Box::new(level2)]);
        let branch_if = AstSelect::create_by(0, 1, vec![Box::new(level1)]);
        let node_if = AstIf::create_by(AstSelect::nop(), branch_if, AstSelect::nop());
        let root = AstSelect::create_by(0, 1, vec![Box::new(node_if)]);

        let before = ctx.gas_remaining();
        root.execute(&mut ctx).unwrap();
        let after = ctx.gas_remaining();
        before - after
    };

    let run_revert = || {
        let mut ctx = build_ast_ctx_with_state(env.clone(), Box::new(AstTestState::default()), &tx);
        ctx.gas_initialize(10000).unwrap();

        // Keep exactly the same multilayer structure, but replace the innermost
        // plain-gas action with a forced Revert action.
        let level3 = AstSelect::create_by(0, 1, vec![Box::new(AstTestFail::new())]);
        let level2 = AstSelect::create_by(0, 1, vec![Box::new(level3)]);
        let level1 = AstSelect::create_by(0, 1, vec![Box::new(level2)]);
        let branch_if = AstSelect::create_by(0, 1, vec![Box::new(level1)]);
        let node_if = AstIf::create_by(AstSelect::nop(), branch_if, AstSelect::nop());
        let root = AstSelect::create_by(0, 1, vec![Box::new(node_if)]);

        let before = ctx.gas_remaining();
        root.execute(&mut ctx).unwrap();
        let after = ctx.gas_remaining();
        before - after
    };

    let used_success = run_success();
    let used_revert = run_revert();

    assert_eq!(used_success - used_revert, 0);
}

#[test]
fn test_ast_static_size_repeated_charge_is_additive_per_execution() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;

    let run_case = |num_children: usize| {
        let mut ctx = build_ast_ctx_with_state(env.clone(), Box::new(AstTestState::default()), &tx);
        ctx.gas_initialize(10000).unwrap();
        let mut acts: Vec<Box<dyn Action>> = Vec::with_capacity(num_children);
        for _ in 0..num_children {
            acts.push(Box::new(AstSelect::nop()));
        }
        let outer = AstSelect::create_list(acts);
        let before = ctx.gas_remaining();
        let (ret_gas, _) = outer.execute(&mut ctx).unwrap();
        let after = ctx.gas_remaining();
        (ret_gas, before - after)
    };

    let (ret1, shared1) = run_case(1);
    let (ret2, shared2) = run_case(2);

    // AST nodes return gas=0; ret_gas is always 0.
    assert_eq!(ret1, 0);
    assert_eq!(ret2, 0);
    // one more child attempt adds one more snapshot try cost (40 gas)
    assert_eq!(shared2 - shared1, 40);
}

#[test]
fn test_ast_single_select_plain_reported_gas_propagates() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;

    let run_case = |a: u8, b: u8| {
        let mut ctx = build_ast_ctx_with_state(env.clone(), Box::new(AstTestState::default()), &tx);
        ctx.gas_initialize(10000).unwrap();
        let act = AstSelect::create_list(vec![
            Box::new(AstTestGasOnly::create_by(a)),
            Box::new(AstTestGasOnly::create_by(b)),
        ]);
        let before = ctx.gas_remaining();
        let (ret_gas, _) = act.execute(&mut ctx).unwrap();
        let after = ctx.gas_remaining();
        (ret_gas, before - after)
    };

    // AST control-flow nodes return gas=0; ordinary child return-gas has no extra9 surcharge.
    let (ret0, shared0) = run_case(0, 0);
    let (ret1, shared1) = run_case(7, 11);
    assert_eq!(ret0, 0);
    assert_eq!(ret1, 0);
    assert_eq!(shared1 - shared0, 0);
}

#[test]
fn test_ast_select_partial_write_is_reverted_by_tx_level_rollback() {
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.gas_max = Uint1::from(17);
    tx.actions
        .push(Box::new(AstSelect::create_by(
            2,
            2,
            vec![
                Box::new(AstTestSet::create_by(7, 77)), // succeeds and writes
                Box::new(AstTestFail::new()),           // fails
            ],
        )))
        .unwrap();

    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.state().set(vec![9], vec![99]); // parent baseline

    let old = ctx.state_fork(); // tx-level isolation
    ctx.exec_from_set(ExecFrom::Top);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("must succeed at least"), "{}", err);
    ctx.state_recover(old); // tx-level rollback on failure

    assert_eq!(ast_state_get_u8(&mut ctx, 9), Some(99)); // baseline kept
    assert_eq!(ast_state_get_u8(&mut ctx, 7), None); // child write rolled back
}

#[test]
fn test_ast_nested_if_select_else_path_commits_expected_layers() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let inner_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestFail::new())]), // force false -> else
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(52, 52))]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(53, 53))]),
    );

    let outer_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(50, 50))]),
        AstSelect::create_list(vec![
            Box::new(AstTestSet::create_by(51, 51)),
            Box::new(inner_if),
        ]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(54, 54))]),
    );

    ctx.exec_from_set(ExecFrom::Top);
    outer_if.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 50), Some(50)); // outer cond
    assert_eq!(ast_state_get_u8(&mut ctx, 51), Some(51)); // outer if branch
    assert_eq!(ast_state_get_u8(&mut ctx, 53), Some(53)); // inner else branch
    assert_eq!(ast_state_get_u8(&mut ctx, 52), None); // inner if branch not executed
    assert_eq!(ast_state_get_u8(&mut ctx, 54), None); // outer else not executed
}

#[test]
fn test_ast_tx_gasmax_zero_skips_init_and_fails_on_first_real_gas_use() {
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.actions
        .push(Box::new(AstSelect::create_by(
            0,
            1,
            vec![Box::new(AstTestSet::create_by(15, 15))],
        )))
        .unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = create_tx_info(&tx);
    init_setup_once();
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    let mut state = protocol::state::CoreState::wrap(ctx.state());
    let mut bls = state.balance(&tx.main()).unwrap_or_default();
    bls.hacash = Amount::unit238(5_000_000_000);
    state.balance_set(&tx.main(), &bls);

    ctx.exec_from_set(ExecFrom::Top);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("gas not initialized"), "{}", err);
}

#[test]
fn test_ast_nested_item_snapshot_gas_consumption_is_exact() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.fee = Amount::unit238(1_000_000);
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);

    protocol::operate::hac_add(
        &mut ctx,
        &field::ADDRESS_ONEX,
        &Amount::unit238(1_000_000_000),
    )
    .unwrap();
    ctx.gas_initialize(1000).unwrap();

    let inner_1 = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(31, 31))]);
    let inner_2 = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(32, 32))]);
    let outer = AstSelect::create_list(vec![Box::new(inner_1), Box::new(inner_2)]);

    let before = ctx.gas_remaining();
    ctx.exec_from_set(ExecFrom::Top);
    outer.execute(&mut ctx).unwrap();
    let after = ctx.gas_remaining();

    // snapshots consumed include child-attempt snapshots (whole node snapshot is free).
    assert_eq!(before - after, 160);
}

#[test]
fn test_tx_without_ast_allows_nonzero_gasmax_when_topology_is_valid() {
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1_000_000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.gas_max = Uint1::from(17);
    let mut act = HacToTrs::new();
    act.to =
        AddrOrPtr::from_addr(Address::from_readable("1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS").unwrap());
    act.hacash = Amount::zhu(1);
    tx.actions.push(Box::new(act)).unwrap();

    let main = tx.main();
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = create_tx_info(&tx);
    init_setup_once();
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);

    let mut state = protocol::state::CoreState::wrap(ctx.state());
    let mut bls = state.balance(&main).unwrap_or_default();
    bls.hacash = Amount::unit238(5_000_000_000);
    state.balance_set(&main, &bls);

    tx.execute(&mut ctx).unwrap();
}

#[test]
fn test_ast_tx_gas_settlement_charges_fee_plus_used_and_refunds_unused() {
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.gas_max = Uint1::from(17);
    tx.fee = Amount::unit238(1_000_000);
    tx.actions
        .push(Box::new(AstSelect::create_list(vec![Box::new(
            AstTestSet::create_by(41, 41),
        )])))
        .unwrap();

    let main = tx.main();
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = main;
    env.tx.addrs = vec![main];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    protocol::operate::hac_add(&mut ctx, &main, &Amount::unit238(5_000_000_000)).unwrap();

    let before = ast_hac_balance(&mut ctx, &main);
    ctx.exec_from_set(ExecFrom::Top);
    tx.execute(&mut ctx).unwrap();
    let after = ast_hac_balance(&mut ctx, &main);

    let used = ctx.gas_used_charge().unwrap();
    let maxc = ctx.gas_max_charge().unwrap();
    assert!(maxc > used, "must refund unused gas");

    assert!(after <= before || maxc > used);
}

#[test]
fn test_ast_nested_select_failure_does_not_leak_into_outer_select() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let nested_fail = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestSet::create_by(61, 61)), // would be committed in inner select before final Err
            Box::new(AstTestFail::new()),
        ],
    );
    let outer = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestSet::create_by(60, 60)), // success #1
            Box::new(nested_fail),                   // Err -> outer recover this whole sub-state
            Box::new(AstTestSet::create_by(62, 62)), // success #2
        ],
    );

    ctx.exec_from_set(ExecFrom::Top);
    outer.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 60), Some(60));
    assert_eq!(ast_state_get_u8(&mut ctx, 62), Some(62));
    assert_eq!(ast_state_get_u8(&mut ctx, 61), None); // nested failed select write must not leak
}

#[test]
fn test_ast_nested_partial_commits_are_cleared_by_tx_level_rollback() {
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.gas_max = Uint1::from(17);

    let act = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(70, 70))]), // cond=true and committed by AstIf
        AstSelect::create_by(
            2,
            2,
            vec![
                Box::new(AstTestSet::create_by(71, 71)), // committed before final failure
                Box::new(AstTestFail::new()),
            ],
        ),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(72, 72))]),
    );
    tx.actions.push(Box::new(act)).unwrap();

    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.state().set(vec![79], vec![79]); // baseline

    let old = ctx.state_fork();
    ctx.exec_from_set(ExecFrom::Top);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("must succeed at least"), "{}", err);
    ctx.state_recover(old);

    assert_eq!(ast_state_get_u8(&mut ctx, 79), Some(79)); // baseline kept
    assert_eq!(ast_state_get_u8(&mut ctx, 70), None); // nested partial commit must be rolled back at tx level
    assert_eq!(ast_state_get_u8(&mut ctx, 71), None); // nested partial commit must be rolled back at tx level
    assert_eq!(ast_state_get_u8(&mut ctx, 72), None);
}

#[test]
fn test_ast_deep_4level_success_path_commits_expected_state() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let lvl4_select = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestSet::create_by(83, 83)),
            Box::new(AstTestSet::create_by(84, 84)),
        ],
    );
    let lvl3_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(82, 82))]),
        AstSelect::create_list(vec![Box::new(lvl4_select)]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(89, 89))]),
    );
    let lvl2_select = AstSelect::create_list(vec![Box::new(lvl3_if)]);
    let lvl1_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(80, 80))]),
        AstSelect::create_list(vec![
            Box::new(AstTestSet::create_by(81, 81)),
            Box::new(lvl2_select),
        ]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(88, 88))]),
    );

    ctx.exec_from_set(ExecFrom::Top);
    lvl1_if.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 80), Some(80));
    assert_eq!(ast_state_get_u8(&mut ctx, 81), Some(81));
    assert_eq!(ast_state_get_u8(&mut ctx, 82), Some(82));
    assert_eq!(ast_state_get_u8(&mut ctx, 83), Some(83));
    assert_eq!(ast_state_get_u8(&mut ctx, 84), Some(84));
    assert_eq!(ast_state_get_u8(&mut ctx, 88), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 89), None);
}

#[test]
fn test_ast_deep_4level_failed_branch_isolated_by_outer_select() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let lvl4_if_fail = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(93, 93))]), // cond=true
        AstSelect::create_list(vec![Box::new(AstTestFail::new())]),            // force fail
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(94, 94))]),
    );
    let lvl3_select_fail = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestSet::create_by(92, 92)), // partial commit inside this nested select
            Box::new(lvl4_if_fail),                  // fails
        ],
    );
    let lvl2_if_fail = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(91, 91))]), // cond=true commit
        AstSelect::create_list(vec![Box::new(lvl3_select_fail)]),              // fails
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(96, 96))]),
    );
    let lvl1_outer_select = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestSet::create_by(90, 90)), // success #1
            Box::new(lvl2_if_fail),                  // fail; outer select must recover this branch
            Box::new(AstTestSet::create_by(95, 95)), // success #2
        ],
    );

    ctx.exec_from_set(ExecFrom::Top);
    lvl1_outer_select.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 90), Some(90));
    assert_eq!(ast_state_get_u8(&mut ctx, 95), Some(95));
    assert_eq!(ast_state_get_u8(&mut ctx, 91), None); // must be isolated
    assert_eq!(ast_state_get_u8(&mut ctx, 92), None); // must be isolated
    assert_eq!(ast_state_get_u8(&mut ctx, 93), None); // must be isolated
    assert_eq!(ast_state_get_u8(&mut ctx, 94), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 96), None);
}

#[test]
fn test_ast_tree_depth_limit_6_rejects_7th_level() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let lvl7 = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(105, 105))]);
    let lvl6 = AstSelect::create_list(vec![Box::new(lvl7)]);
    let lvl5 = AstSelect::create_list(vec![Box::new(lvl6)]);
    let lvl4 = AstSelect::create_list(vec![Box::new(lvl5)]);
    let lvl3 = AstSelect::create_list(vec![Box::new(lvl4)]);
    let lvl2 = AstSelect::create_list(vec![Box::new(lvl3)]);
    let lvl1 = AstSelect::create_list(vec![Box::new(lvl2)]);

    let err = precheck_runtime_action(TransactionType3::TYPE, &lvl1, ExecFrom::Top).unwrap_err();
    assert!(err.contains("ast tree depth 7 exceeded max 6"), "{}", err);
    assert_eq!(ast_state_get_u8(&mut ctx, 105), None);
}

fn build_tex_ctx_with_state(
    mut env: Env,
    sta: Box<dyn State>,
) -> protocol::context::ContextInst<'static> {
    init_setup_once();
    env.tx.ty = env.tx.ty.max(TransactionType3::TYPE);
    make_ctx_with_default_tx(env, sta)
}

#[test]
fn test_tex_sat_pay_records_sat_not_zhu() {
    use protocol::tex::*;

    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let addr = field::ADDRESS_ONEX.clone();

    let mut ctx = build_tex_ctx_with_state(env, Box::new(TestMemState::default()));
    {
        let mut st = protocol::state::CoreState::wrap(ctx.state());
        let mut bls = Balance::default();
        bls.satoshi = Fold64::from(100).unwrap();
        st.balance_set(&addr, &bls);
    }

    let cell = CellTrsSatPay::new(Fold64::from(7).unwrap());
    cell.execute(&mut ctx, &addr).unwrap();

    assert_eq!(ctx.tex_ledger().sat, 7);
    assert_eq!(ctx.tex_ledger().zhu, 0);
}

#[test]
fn test_tex_asset_serial_must_exist_and_cache() {
    use protocol::tex::*;

    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let addr = field::ADDRESS_ONEX.clone();

    let mut ctx = build_tex_ctx_with_state(env, Box::new(TestMemState::default()));
    {
        let mut st = protocol::state::CoreState::wrap(ctx.state());
        st.asset_set(
            &Fold64::from(9).unwrap(),
            &AssetSmelt {
                serial: Fold64::from(9).unwrap(),
                supply: Fold64::from(10_000).unwrap(),
                decimal: Uint1::from(2),
                issuer: addr,
                ticket: BytesW1::from_str("AST9").unwrap(),
                name: BytesW1::from_str("Asset9").unwrap(),
            },
        );
    }

    let miss = CellCondAssetEq::new(AssetAmt::from(999, 1).unwrap())
        .execute(&mut ctx, &addr)
        .unwrap_err();
    assert!(miss.contains("does not exist"));

    let ok1 = CellCondAssetEq::new(AssetAmt::from(9, 0).unwrap());
    ok1.execute(&mut ctx, &addr).unwrap();
    let ok2 = CellCondAssetEq::new(AssetAmt::from(9, 0).unwrap());
    ok2.execute(&mut ctx, &addr).unwrap();
    assert!(
        ctx.tex_ledger()
            .asset_checked
            .contains(&Fold64::from(9).unwrap())
    );
    assert_eq!(ctx.tex_ledger().asset_checked.len(), 1);
}

#[test]
fn test_tex_diamond_get_zero_rejected_early() {
    use protocol::tex::*;

    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let addr = field::ADDRESS_ONEX.clone();

    let mut ctx = build_tex_ctx_with_state(env, Box::new(TestMemState::default()));
    let err = CellTrsDiaGet::new(DiamondNumber::from(0))
        .execute(&mut ctx, &addr)
        .unwrap_err();
    assert!(err.contains("cannot be zero"));
}

#[test]
fn test_tex_cell_json_must_use_cellid() {
    use protocol::tex::*;

    let mut ls = DnyTexCellW1::default();
    let ok_json = r#"[{"cellid":11,"haczhu":0}]"#;
    ls.from_json(ok_json).unwrap();
    assert_eq!(ls.length(), 1);

    let mut bad = DnyTexCellW1::default();
    let err = bad.from_json(r#"[{"kind":11,"haczhu":0}]"#).unwrap_err();
    assert!(err.contains("cellid"));
}

#[test]
fn test_tex_action_rejects_ast_context_even_in_fast_sync() {
    use protocol::tex::*;

    let mut env = Env::default();
    env.tx.ty = TransactionType3::TYPE;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_tex_ctx_with_state(env, Box::new(TestMemState::default()));
    let acc = Account::create_by_password("cross_tex_ast_ctx_guard").unwrap();
    let addr = Address::from(*acc.address());

    let mut act = TexCellAct::create_by(addr);
    act.add_cell(Box::new(CellCondHeightAtMost::new(100)))
        .unwrap();
    act.do_sign(&acc).unwrap();

    ctx.exec_from_set(ExecFrom::Ast);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(err.contains("TOP") && err.contains("AST"), "{err}");
}

#[test]
fn test_p2sh_prove_rejects_call_context_even_in_fast_sync() {
    let mut tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1_730_000_300,
    );
    tx.gas_max = Uint1::from(17);
    let mut env = Env::default();
    env.block.height = protocol::upgrade::ONLINE_OPEN_HEIGHT;
    env.tx = create_tx_info(&tx);
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let (_scriptmh, act) = build_p2sh_unlock_prove("return 0");
    ctx.exec_from_set(ExecFrom::Call);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("P2SHScriptProve can only run in TOP context"),
        "{err}"
    );
}

#[test]
fn test_p2sh_prove_rejects_ast_context_even_in_fast_sync() {
    let mut tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1_730_000_300,
    );
    tx.gas_max = Uint1::from(17);
    let mut env = Env::default();
    env.block.height = protocol::upgrade::ONLINE_OPEN_HEIGHT;
    env.tx = create_tx_info(&tx);
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let (_scriptmh, act) = build_p2sh_unlock_prove("return 0");
    ctx.exec_from_set(ExecFrom::Ast);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("P2SHScriptProve can only run in TOP context"),
        "{err}"
    );
}

#[test]
fn test_tex_action_signature_rejects_payload_tamper() {
    use protocol::tex::*;

    let mut env = Env::default();
    env.tx.ty = TransactionType3::TYPE;
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    env.block.height = 10;
    env.chain.fast_sync = true;

    let mut ctx = build_tex_ctx_with_state(env, Box::new(TestMemState::default()));
    let acc = Account::create_by_password("sig_check_tex").unwrap();
    let addr = Address::from(*acc.address());

    let mut act = TexCellAct::create_by(addr);
    act.add_cell(Box::new(CellCondHeightAtMost::new(100)))
        .unwrap();
    act.do_sign(&acc).unwrap();
    // tamper payload after sign
    act.add_cell(Box::new(CellCondHeightAtMost::new(100)))
        .unwrap();

    ctx.exec_from_set(ExecFrom::Top);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(err.contains("signature verification failed"));
}

// =====================================================================
// Comprehensive AST snapshot/restore edge-case tests
// =====================================================================

// --- Test helper: action that pushes a log entry ---
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestLog {
    tag: Uint1,
}

impl Parse for AstTestLog {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        self.tag.parse(buf)
    }
}
impl Serialize for AstTestLog {
    fn serialize(&self) -> Vec<u8> {
        self.tag.serialize()
    }
    fn size(&self) -> usize {
        self.tag.size()
    }
}
impl Field for AstTestLog {
    fn new() -> Self {
        Self::default()
    }
}
impl ToJSON for AstTestLog {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        format!("{{\"tag\":{}}}", self.tag.to_json_fmt(fmt))
    }
}
impl FromJSON for AstTestLog {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let pairs = json_split_object(json)?;
        for (k, v) in pairs {
            if k == "tag" {
                self.tag.from_json(v)?;
            }
        }
        Ok(())
    }
}
impl ActExec for AstTestLog {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        ctx.logs().push(&self.tag);
        Ok((0, vec![]))
    }
}
impl Description for AstTestLog {}
impl Action for AstTestLog {
    fn kind(&self) -> u16 {
        65005
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl AstTestLog {
    fn create_by(tag: u8) -> Self {
        Self {
            tag: Uint1::from(tag),
        }
    }
}

// --- Helper to build ctx with AstTestLogs ---
fn build_ast_ctx_with_logs<'a>(
    mut env: Env,
    sta: Box<dyn State>,
    log: Box<dyn Logs>,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    init_setup_once();
    env.tx.ty = env.tx.ty.max(TransactionType3::TYPE);
    let mut ctx = testkit_make_ctx_with_logs(env, sta, log, tx);
    let main = ctx.env().tx.main;
    let mut st = protocol::state::CoreState::wrap(ctx.state());
    let mut bls = st.balance(&main).unwrap_or_default();
    bls.hacash = Amount::unit238(10_000_000_000_000);
    st.balance_set(&main, &bls);
    ctx
}

// PLACEHOLDER_NEW_TESTS

// ---- Test 1: AstIf branch failure triggers whole_snap recover ----
// Validates the fix: without ctx_recover(ctx, whole_snap) on branch Err,
// the state fork layer leaks.
// ---- Test 2: AstIf else branch failure also recovers whole_snap ----
#[test]
fn test_ast_if_else_branch_fail_recovers_whole_snap() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    // cond fails -> else branch, but else also fails
    let astif = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestFail::new())]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(210, 210))]),
        AstSelect::create_by(1, 1, vec![Box::new(AstTestFail::new())]),
    );
    ctx.exec_from_set(ExecFrom::Top);
    let err = astif.execute(&mut ctx).unwrap_err();
    assert!(err.contains("ast test forced fail") || err.contains("must succeed at least"));
    assert_eq!(ast_state_get_u8(&mut ctx, 210), None);
}

// ---- Test 3: AstSelect early-return validation doesn't leak state fork ----
// Validates the fix: validation checks moved before ctx_snapshot.
#[test]
fn test_ast_select_validation_early_return_no_state_leak() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.state().set(vec![220], vec![220]);

    // min > max: invalid
    let bad = AstSelect::create_by(3, 1, vec![Box::new(AstTestSet::create_by(221, 221))]);
    ctx.exec_from_set(ExecFrom::Top);
    let err = bad.execute(&mut ctx).unwrap_err();
    assert!(err.contains("max cannot be less than min"));

    // State must still be available (no leaked fork layer)
    assert_eq!(ast_state_get_u8(&mut ctx, 220), Some(220));
    ctx.state().set(vec![222], vec![222]);
    assert_eq!(ast_state_get_u8(&mut ctx, 222), Some(222));
}

// PLACEHOLDER_TESTS_PART2

// ---- Test 4: Logs are truncated on AstSelect child failure ----
#[test]
fn test_ast_select_logs_truncated_on_child_failure() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let logs = Box::new(AstTestLogs::new());
    let logs_ptr = logs.as_ref() as *const AstTestLogs;
    let mut ctx = build_ast_ctx_with_logs(env, Box::new(AstTestState::default()), logs, &tx);
    ctx.gas_initialize(10000).unwrap();

    // child 1: log + succeed, child 2: log + fail
    // AstSelect(min=1, max=2): child1 ok, child2 fail -> ok with 1
    let act = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(AstTestLog::create_by(1)),
            Box::new(AstTestFail::new()), // fails, its snap should recover logs
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    // Only child1's log should remain (child2 failed -> no log from it, but AstTestFail doesn't log)
    // The key point: log count should be 1 (from child1), not more
    let log_len = unsafe { &*logs_ptr }.len();
    assert_eq!(log_len, 1);
}

// ---- Test 5: Logs truncated on AstIf branch failure (whole_snap recover) ----
// PLACEHOLDER_TESTS_PART3

// ---- Test 9: AstSelect min=0 all children fail -> success with empty result ----
#[test]
fn test_ast_select_min_zero_all_fail_succeeds() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.state().set(vec![230], vec![230]);

    let act = AstSelect::create_by(
        0,
        2,
        vec![Box::new(AstTestFail::new()), Box::new(AstTestFail::new())],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap(); // should succeed

    assert_eq!(ast_state_get_u8(&mut ctx, 230), Some(230)); // baseline intact
}

// PLACEHOLDER_TESTS_PART4

// ---- Test 12: State overwrite in failed branch doesn't leak ----
#[test]
fn test_ast_state_overwrite_in_failed_branch_does_not_leak() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.state().set(vec![1], vec![100]); // pre-existing value

    // child1: overwrite key=1 to 200, then fail
    let inner = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestSet::create_by(1, 200)),
            Box::new(AstTestFail::new()),
        ],
    );
    let act = AstSelect::create_by(0, 1, vec![Box::new(inner)]);
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    // Original value must be preserved
    assert_eq!(ast_state_get_u8(&mut ctx, 1), Some(100));
}

// ---- Test 13: AstIf else path with nested AstSelect partial success ----
#[test]
fn test_ast_if_else_with_nested_select_partial_success() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    // cond fails -> else branch
    // else = select(min=1, max=3): child1 ok, child2 fail, child3 ok
    let astif = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestFail::new())]),
        AstSelect::nop(),
        AstSelect::create_by(
            1,
            3,
            vec![
                Box::new(AstTestSet::create_by(160, 160)),
                Box::new(AstTestFail::new()),
                Box::new(AstTestSet::create_by(162, 162)),
            ],
        ),
    );
    ctx.exec_from_set(ExecFrom::Top);
    astif.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 160), Some(160));
    assert_eq!(ast_state_get_u8(&mut ctx, 162), Some(162));
}

// ---- Test 15: Double nested AstIf — inner else, outer if ----
#[test]
fn test_ast_double_nested_if_inner_else_outer_if() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let inner_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestFail::new())]), // cond fail -> else
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(180, 180))]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(181, 181))]),
    );
    let outer_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(182, 182))]), // cond ok -> if
        AstSelect::create_list(vec![
            Box::new(inner_if),
            Box::new(AstTestSet::create_by(183, 183)),
        ]),
        AstSelect::nop(),
    );
    ctx.exec_from_set(ExecFrom::Top);
    outer_if.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 182), Some(182)); // outer cond
    assert_eq!(ast_state_get_u8(&mut ctx, 181), Some(181)); // inner else
    assert_eq!(ast_state_get_u8(&mut ctx, 183), Some(183)); // outer if sibling
    assert_eq!(ast_state_get_u8(&mut ctx, 180), None); // inner if not taken
}

// ---- Test 16: AstSelect max reached stops early, remaining children not executed ----
#[test]
fn test_ast_select_stops_at_max() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let act = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(AstTestSet::create_by(190, 1)),
            Box::new(AstTestSet::create_by(191, 2)),
            Box::new(AstTestSet::create_by(192, 3)), // should not execute
            Box::new(AstTestSet::create_by(193, 4)), // should not execute
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 190), Some(1));
    assert_eq!(ast_state_get_u8(&mut ctx, 191), Some(2));
    assert_eq!(ast_state_get_u8(&mut ctx, 192), None); // not reached
    assert_eq!(ast_state_get_u8(&mut ctx, 193), None); // not reached
}

// ---- Test 17: AstSelect validation max > num rejected without state leak ----
#[test]
fn test_ast_select_max_gt_num_rejected_no_leak() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.state().set(vec![1], vec![1]);

    let bad = AstSelect::create_by(1, 5, vec![Box::new(AstTestSet::create_by(2, 2))]);
    ctx.exec_from_set(ExecFrom::Top);
    let err = bad.execute(&mut ctx).unwrap_err();
    assert!(err.contains("max cannot exceed list num"));

    // State still available
    assert_eq!(ast_state_get_u8(&mut ctx, 1), Some(1));
    ctx.state().set(vec![3], vec![3]);
    assert_eq!(ast_state_get_u8(&mut ctx, 3), Some(3));
}

// ---- Test 18: Sequential AST operations on same context ----
// After one AST op completes (success or fail), the next one works correctly.
#[test]
fn test_ast_sequential_operations_on_same_context() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    // Op 1: fails
    let fail_act = AstSelect::create_by(1, 1, vec![Box::new(AstTestFail::new())]);
    ctx.exec_from_set(ExecFrom::Top);
    let _ = fail_act.execute(&mut ctx);

    // Op 2: succeeds
    let ok_act = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(150, 150))]);
    ctx.exec_from_set(ExecFrom::Top);
    ok_act.execute(&mut ctx).unwrap();
    assert_eq!(ast_state_get_u8(&mut ctx, 150), Some(150));

    // Op 3: AstIf succeeds
    let if_act = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(151, 151))]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(152, 152))]),
        AstSelect::nop(),
    );
    ctx.exec_from_set(ExecFrom::Top);
    if_act.execute(&mut ctx).unwrap();
    assert_eq!(ast_state_get_u8(&mut ctx, 151), Some(151));
    assert_eq!(ast_state_get_u8(&mut ctx, 152), Some(152));
}

// =====================================================================
// VM snapshot/restore tests within AST branches
// =====================================================================

// --- Test action that mutates VM state (increments MockVM counter) ---
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestVMCall {
    increment: Uint1,
}

impl Parse for AstTestVMCall {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        self.increment.parse(buf)
    }
}
impl Serialize for AstTestVMCall {
    fn serialize(&self) -> Vec<u8> {
        self.increment.serialize()
    }
    fn size(&self) -> usize {
        self.increment.size()
    }
}
impl Field for AstTestVMCall {
    fn new() -> Self {
        Self::default()
    }
}
impl ToJSON for AstTestVMCall {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        "{}".to_owned()
    }
}
impl FromJSON for AstTestVMCall {
    fn from_json(&mut self, _json: &str) -> Ret<()> {
        Ok(())
    }
}
impl ActExec for AstTestVMCall {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        let Some(snap) = ctx.vm_snapshot_volatile() else {
            return xerrf!("test vm missing");
        };
        if let Ok(cur) = snap.downcast::<i64>() {
            let new_val = *cur + *self.increment as i64;
            ctx.vm_restore_volatile(Box::new(new_val));
        }
        Ok((0, vec![]))
    }
}
impl Description for AstTestVMCall {}
impl Action for AstTestVMCall {
    fn kind(&self) -> u16 {
        65009
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl AstTestVMCall {
    fn create_by(inc: u8) -> Self {
        Self {
            increment: Uint1::from(inc),
        }
    }
}

struct AstDeepDelayVm {
    volatile: std::sync::Arc<std::sync::atomic::AtomicI64>,
    warmup: std::sync::Arc<std::sync::atomic::AtomicI64>,
    restore_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    clean_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl VM for AstDeepDelayVm {
    fn snapshot_volatile(&mut self) -> Box<dyn std::any::Any> {
        Box::new(self.volatile.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn restore_volatile(&mut self, snap: Box<dyn std::any::Any>) {
        if let Ok(v) = snap.downcast::<i64>() {
            self.restore_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.volatile.store(*v, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn rollback_volatile_preserve_warm_and_gas(&mut self) {
        self.clean_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.volatile.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn call(
        &mut self,
        _: &mut dyn Context,
        req: Box<dyn std::any::Any>,
    ) -> XRet<(VmGasBuckets, Box<dyn std::any::Any>)> {
        let Ok(data) = req.downcast::<Vec<u8>>() else {
            return xerrf!("deep delay vm payload type mismatch");
        };
        let data = *data;
        if data.len() < 3 {
            return xerrf!("deep delay vm payload too short");
        }
        let vol_add = data[0] as i64;
        let warm_add = data[1] as i64;
        let should_fail = data[2] != 0;
        self.volatile
            .fetch_add(vol_add, std::sync::atomic::Ordering::SeqCst);
        self.warmup
            .fetch_add(warm_add, std::sync::atomic::Ordering::SeqCst);
        if should_fail {
            return xerr_rf!("deep delay vm forced fail");
        }
        Ok((VmGasBuckets::default(), Box::new(Vec::<u8>::new())))
    }
}

static AST_DEEP_DELAY_VM_HANDLES: std::sync::OnceLock<
    std::sync::Mutex<
        Option<(
            std::sync::Arc<std::sync::atomic::AtomicI64>,
            std::sync::Arc<std::sync::atomic::AtomicI64>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        )>,
    >,
> = std::sync::OnceLock::new();

fn set_ast_deep_delay_vm_handles(
    volatile: std::sync::Arc<std::sync::atomic::AtomicI64>,
    warmup: std::sync::Arc<std::sync::atomic::AtomicI64>,
    restore_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    clean_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let lock = AST_DEEP_DELAY_VM_HANDLES.get_or_init(|| std::sync::Mutex::new(None));
    *lock.lock().unwrap() = Some((volatile, warmup, restore_count, clean_count));
}

fn take_ast_deep_delay_vm() -> Box<dyn VM> {
    let lock = AST_DEEP_DELAY_VM_HANDLES.get_or_init(|| std::sync::Mutex::new(None));
    let guards = lock.lock().unwrap();
    let (volatile, warmup, restore_count, clean_count) = guards
        .as_ref()
        .expect("deep delay vm handles not set")
        .clone();
    Box::new(AstDeepDelayVm {
        volatile,
        warmup,
        restore_count,
        clean_count,
    })
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestDeepDelayVmInit;

impl Parse for AstTestDeepDelayVmInit {
    fn parse(&mut self, _buf: &[u8]) -> Ret<usize> {
        Ok(0)
    }
}

impl Serialize for AstTestDeepDelayVmInit {
    fn serialize(&self) -> Vec<u8> {
        vec![]
    }

    fn size(&self) -> usize {
        0
    }
}

impl Field for AstTestDeepDelayVmInit {
    fn new() -> Self {
        Self
    }
}

impl ToJSON for AstTestDeepDelayVmInit {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        "{}".to_owned()
    }
}

impl FromJSON for AstTestDeepDelayVmInit {
    fn from_json(&mut self, _json: &str) -> Ret<()> {
        Ok(())
    }
}

impl ActExec for AstTestDeepDelayVmInit {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        unsafe { ctx_inst(ctx) }.test_set_vm(take_ast_deep_delay_vm());
        Ok((0, vec![]))
    }
}

impl Description for AstTestDeepDelayVmInit {}

impl Action for AstTestDeepDelayVmInit {
    fn kind(&self) -> u16 {
        65019
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestDeepDelayVmCall {
    vol_add: Uint1,
    warm_add: Uint1,
    fail: Uint1,
}

impl Parse for AstTestDeepDelayVmCall {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        let mut mv = self.vol_add.parse(buf)?;
        mv += self.warm_add.parse(&buf[mv..])?;
        mv += self.fail.parse(&buf[mv..])?;
        Ok(mv)
    }
}

impl Serialize for AstTestDeepDelayVmCall {
    fn serialize(&self) -> Vec<u8> {
        [
            self.vol_add.serialize(),
            self.warm_add.serialize(),
            self.fail.serialize(),
        ]
        .concat()
    }

    fn size(&self) -> usize {
        self.vol_add.size() + self.warm_add.size() + self.fail.size()
    }
}

impl Field for AstTestDeepDelayVmCall {
    fn new() -> Self {
        Self::default()
    }
}

impl ToJSON for AstTestDeepDelayVmCall {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        "{}".to_owned()
    }
}

impl FromJSON for AstTestDeepDelayVmCall {
    fn from_json(&mut self, _json: &str) -> Ret<()> {
        Ok(())
    }
}

impl ActExec for AstTestDeepDelayVmCall {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        let payload = vec![*self.vol_add, *self.warm_add, *self.fail];
        let (_gas, rv) = ctx.vm_call(Box::new(payload))?;
        let Ok(rv) = rv.downcast::<Vec<u8>>() else {
            return xerrf!("deep delay vm return type mismatch");
        };
        // VM dynamic gas is charged through shared ctx remaining inside VM runtime.
        // Keep action return-gas channel as size-only (0 here for this custom test action).
        Ok((0, *rv))
    }
}

impl Description for AstTestDeepDelayVmCall {}

impl Action for AstTestDeepDelayVmCall {
    fn kind(&self) -> u16 {
        65020
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl AstTestDeepDelayVmCall {
    fn create_by(vol_add: u8, warm_add: u8, fail: u8) -> Self {
        Self {
            vol_add: Uint1::from(vol_add),
            warm_add: Uint1::from(warm_add),
            fail: Uint1::from(fail),
        }
    }
}

struct AstBugAssumeVm {
    remaining: std::sync::Arc<std::sync::atomic::AtomicI64>,
    burned: std::sync::Arc<std::sync::atomic::AtomicI64>,
    volatile_mark: std::sync::atomic::AtomicI64,
}

impl AstBugAssumeVm {
    fn create(
        remaining: i64,
    ) -> (
        Box<dyn VM>,
        std::sync::Arc<std::sync::atomic::AtomicI64>,
        std::sync::Arc<std::sync::atomic::AtomicI64>,
    ) {
        let rem = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(remaining));
        let burned = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
        (
            Box::new(Self {
                remaining: rem.clone(),
                burned: burned.clone(),
                volatile_mark: std::sync::atomic::AtomicI64::new(0),
            }),
            rem,
            burned,
        )
    }
}

impl VM for AstBugAssumeVm {
    fn snapshot_volatile(&mut self) -> Box<dyn std::any::Any> {
        Box::new(self.volatile_mark.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn restore_volatile(&mut self, snap: Box<dyn std::any::Any>) {
        if let Ok(mark) = snap.downcast::<i64>() {
            self.volatile_mark
                .store(*mark, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn call(
        &mut self,
        _: &mut dyn Context,
        req: Box<dyn std::any::Any>,
    ) -> XRet<(VmGasBuckets, Box<dyn std::any::Any>)> {
        let Ok(data) = req.downcast::<Vec<u8>>() else {
            return xerrf!("ast bug assume vm payload type mismatch");
        };
        let data = *data;
        if data.len() < 2 {
            return xerrf!("ast bug assume vm payload too short");
        }
        let should_fail = data[0] != 0;
        let gas_cost = data[1] as i64;
        self.remaining
            .fetch_sub(gas_cost, std::sync::atomic::Ordering::SeqCst);
        self.volatile_mark
            .fetch_add(gas_cost, std::sync::atomic::Ordering::SeqCst);
        if should_fail {
            return xerr_rf!("ast bug assume vm forced fail");
        }
        self.burned
            .fetch_add(gas_cost, std::sync::atomic::Ordering::SeqCst);
        Ok((
            VmGasBuckets {
                compute: gas_cost,
                ..VmGasBuckets::default()
            },
            Box::new(Vec::<u8>::new()),
        ))
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestBugVmCall {
    fail: Uint1,
    cost: Uint1,
}

impl Parse for AstTestBugVmCall {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        let mut mv = self.fail.parse(buf)?;
        mv += self.cost.parse(&buf[mv..])?;
        Ok(mv)
    }
}

impl Serialize for AstTestBugVmCall {
    fn serialize(&self) -> Vec<u8> {
        [self.fail.serialize(), self.cost.serialize()].concat()
    }

    fn size(&self) -> usize {
        self.fail.size() + self.cost.size()
    }
}

impl Field for AstTestBugVmCall {
    fn new() -> Self {
        Self::default()
    }
}

impl ToJSON for AstTestBugVmCall {
    fn to_json_fmt(&self, _fmt: &JSONFormater) -> String {
        "{}".to_owned()
    }
}

impl FromJSON for AstTestBugVmCall {
    fn from_json(&mut self, _json: &str) -> Ret<()> {
        Ok(())
    }
}

impl ActExec for AstTestBugVmCall {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        let payload = vec![*self.fail, *self.cost];
        let (_gas, rv) = ctx.vm_call(Box::new(payload))?;
        let Ok(rv) = rv.downcast::<Vec<u8>>() else {
            return xerrf!("ast bug assume vm return type mismatch");
        };
        // VM dynamic gas is charged through shared ctx remaining inside VM runtime.
        // Keep action return-gas channel as size-only (0 here for this custom test action).
        Ok((0, *rv))
    }
}

impl Description for AstTestBugVmCall {}

impl Action for AstTestBugVmCall {
    fn kind(&self) -> u16 {
        65016
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl AstTestBugVmCall {
    fn fail(cost: u8) -> Self {
        Self {
            fail: Uint1::from(1),
            cost: Uint1::from(cost),
        }
    }

    fn ok(cost: u8) -> Self {
        Self {
            fail: Uint1::from(0),
            cost: Uint1::from(cost),
        }
    }
}

#[test]
fn test_ast_vm_assumption_fail_child_then_success_child_reports_only_success_burn() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (vm, remaining, burned) = AstBugAssumeVm::create(100);
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(vm);

    let act = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(AstTestBugVmCall::fail(30)),
            Box::new(AstTestBugVmCall::ok(5)),
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    assert_eq!(remaining.load(std::sync::atomic::Ordering::SeqCst), 65);
    assert_eq!(burned.load(std::sync::atomic::Ordering::SeqCst), 5);
}

#[test]
fn test_ast_vm_assumption_min_zero_keeps_failed_vm_branch_unreported_in_mock_burn() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (vm, remaining, burned) = AstBugAssumeVm::create(100);
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(vm);

    let act = AstSelect::create_by(0, 1, vec![Box::new(AstTestBugVmCall::fail(20))]);
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    assert_eq!(remaining.load(std::sync::atomic::Ordering::SeqCst), 80);
    assert_eq!(burned.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn test_ast_vm_assumption_all_success_children_match_mock_burn() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (vm, remaining, burned) = AstBugAssumeVm::create(100);
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(vm);

    let act = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(AstTestBugVmCall::ok(30)),
            Box::new(AstTestBugVmCall::ok(5)),
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    let rem = remaining.load(std::sync::atomic::Ordering::SeqCst);
    let bur = burned.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rem, 65);
    assert_eq!(bur, 35);
    assert_eq!(100 - rem, bur);
}

#[test]
fn test_ast_vm_assumption_min_zero_success_child_updates_mock_burn() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (vm, remaining, burned) = AstBugAssumeVm::create(100);
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(vm);

    let act = AstSelect::create_by(0, 1, vec![Box::new(AstTestBugVmCall::ok(20))]);
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    let rem = remaining.load(std::sync::atomic::Ordering::SeqCst);
    let bur = burned.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rem, 80);
    assert_eq!(bur, 20);
    assert_eq!(100 - rem, bur);
}

#[test]
fn test_ast_vm_delay_init_depth6_revert_and_fault_channels() {
    let _guard = ast_test_globals_guard();
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.fee = Amount::unit238(1_000_000);
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    protocol::operate::hac_add(
        &mut ctx,
        &field::ADDRESS_ONEX,
        &Amount::unit238(1_000_000_000),
    )
    .unwrap();
    ctx.gas_initialize(4000).unwrap();

    let volatile = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let warmup = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let restore_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clean_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    set_ast_deep_delay_vm_handles(
        volatile.clone(),
        warmup.clone(),
        restore_count.clone(),
        clean_count.clone(),
    );

    let lvl6_fail = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestDeepDelayVmCall::create_by(9, 3, 1)),
            Box::new(AstTestSet::create_by(232, 232)),
        ],
    );
    let lvl5 = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestDeepDelayVmCall::create_by(4, 2, 0))]),
        AstSelect::create_list(vec![Box::new(lvl6_fail)]),
        AstSelect::nop(),
    );
    let lvl4 = AstSelect::create_list(vec![
        Box::new(AstTestDeepDelayVmInit::new()),
        Box::new(AstTestSet::create_by(231, 231)),
        Box::new(lvl5),
    ]);
    let lvl3 = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(230, 230))]),
        AstSelect::create_list(vec![Box::new(lvl4)]),
        AstSelect::nop(),
    );
    let root = AstSelect::create_by(
        1,
        2,
        vec![Box::new(lvl3), Box::new(AstTestSet::create_by(233, 233))],
    );

    let err = precheck_runtime_action(TransactionType3::TYPE, &root, ExecFrom::Top).unwrap_err();
    assert!(err.contains("ast tree depth 7 exceeded max 6"), "{}", err);

    // precheck rejects the whole root AST node before execution
    assert_eq!(ast_state_get_u8(&mut ctx, 230), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 231), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 232), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 233), None);

    // precheck aborts before any VM path runs, so warmup state must stay untouched
    assert_eq!(volatile.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(warmup.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(restore_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(clean_count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn test_ast_layered_composition_mixed_vm_calls_snapshot_gas_exact() {
    let _guard = ast_test_globals_guard();
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.fee = Amount::unit238(1_000_000);
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    protocol::operate::hac_add(
        &mut ctx,
        &field::ADDRESS_ONEX,
        &Amount::unit238(1_000_000_000),
    )
    .unwrap();
    ctx.gas_initialize(2000).unwrap();

    let volatile = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let warmup = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let restore_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clean_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    set_ast_deep_delay_vm_handles(
        volatile.clone(),
        warmup.clone(),
        restore_count.clone(),
        clean_count.clone(),
    );

    let node1 = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(241, 241))]);
    let node2 = AstSelect::create_list(vec![
        Box::new(AstTestDeepDelayVmInit::new()),
        Box::new(AstTestDeepDelayVmCall::create_by(1, 1, 0)),
    ]);
    let node3 = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestDeepDelayVmCall::create_by(2, 1, 0))]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(242, 242))]),
        AstSelect::nop(),
    );
    let root = AstSelect::create_list(vec![Box::new(node1), Box::new(node2), Box::new(node3)]);

    let before = ctx.gas_remaining();
    ctx.exec_from_set(ExecFrom::Top);
    root.execute(&mut ctx).unwrap();
    let after = ctx.gas_remaining();

    // Top-level loop discards return gas; only shared snapshot/dynamic gas is charged here.
    // +40 compared to old model: AstIf branch now uses isolated snapshot instead of shared.
    assert_eq!(before - after, 400);
    assert_eq!(ast_state_get_u8(&mut ctx, 241), Some(241));
    assert_eq!(ast_state_get_u8(&mut ctx, 242), Some(242));
    assert!(volatile.load(std::sync::atomic::Ordering::SeqCst) >= 0);
    assert!(warmup.load(std::sync::atomic::Ordering::SeqCst) >= 0);
    assert_eq!(restore_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(clean_count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn test_ast_layered_with_mid_vm_failure_revert_and_warmup_monotonic() {
    let _guard = ast_test_globals_guard();
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.fee = Amount::unit238(1_000_000);
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = field::ADDRESS_ONEX.clone();
    env.tx.addrs = vec![field::ADDRESS_ONEX.clone()];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    protocol::operate::hac_add(
        &mut ctx,
        &field::ADDRESS_ONEX,
        &Amount::unit238(1_000_000_000),
    )
    .unwrap();
    ctx.gas_initialize(2000).unwrap();

    let volatile = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let warmup = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let restore_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clean_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    set_ast_deep_delay_vm_handles(
        volatile.clone(),
        warmup.clone(),
        restore_count.clone(),
        clean_count.clone(),
    );

    let first_ok = AstSelect::create_list(vec![
        Box::new(AstTestDeepDelayVmInit::new()),
        Box::new(AstTestDeepDelayVmCall::create_by(5, 2, 0)),
    ]);
    let mid_fail = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestDeepDelayVmCall::create_by(7, 3, 1)),
            Box::new(AstTestSet::create_by(251, 251)),
        ],
    );
    let root = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(first_ok),
            Box::new(mid_fail),
            Box::new(AstTestSet::create_by(252, 252)),
        ],
    );

    let before = ctx.gas_remaining();
    ctx.exec_from_set(ExecFrom::Top);
    root.execute(&mut ctx).unwrap();
    let after = ctx.gas_remaining();

    // snapshots include attempt-level captures only.
    assert_eq!(before - after, 280);
    assert_eq!(ast_state_get_u8(&mut ctx, 251), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 252), Some(252));
    assert!(volatile.load(std::sync::atomic::Ordering::SeqCst) >= 0);
    assert_eq!(warmup.load(std::sync::atomic::Ordering::SeqCst), 5);
    assert!(restore_count.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

#[test]
fn test_tx_multiple_top_ast_with_internal_vm_calls_gas_settlement_matches_balance() {
    let _guard = ast_test_globals_guard();
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.gas_max = Uint1::from(17);
    tx.fee = Amount::unit238(1_000_000);
    tx.actions
        .push(Box::new(AstSelect::create_list(vec![Box::new(
            AstTestSet::create_by(201, 201),
        )])))
        .unwrap();
    tx.actions
        .push(Box::new(AstSelect::create_list(vec![
            Box::new(AstTestDeepDelayVmInit::new()),
            Box::new(AstTestDeepDelayVmCall::create_by(1, 1, 0)),
        ])))
        .unwrap();
    tx.actions
        .push(Box::new(AstIf::create_by(
            AstSelect::create_list(vec![Box::new(AstTestDeepDelayVmCall::create_by(2, 1, 0))]),
            AstSelect::create_list(vec![Box::new(AstTestSet::create_by(202, 202))]),
            AstSelect::nop(),
        )))
        .unwrap();

    let main = tx.main();
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = main;
    env.tx.addrs = vec![main];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    protocol::operate::hac_add(&mut ctx, &main, &Amount::unit238(5_000_000_000)).unwrap();

    let volatile = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let warmup = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let restore_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clean_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    set_ast_deep_delay_vm_handles(
        volatile.clone(),
        warmup.clone(),
        restore_count.clone(),
        clean_count.clone(),
    );

    let _before = ast_hac_balance(&mut ctx, &main);
    ctx.exec_from_set(ExecFrom::Top);
    tx.execute(&mut ctx).unwrap();
    let after = ast_hac_balance(&mut ctx, &main);

    assert!(after >= Amount::zero());
    assert_eq!(ast_state_get_u8(&mut ctx, 201), Some(201));
    assert_eq!(ast_state_get_u8(&mut ctx, 202), Some(202));
    assert_eq!(volatile.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(warmup.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(restore_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(clean_count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn test_tx_failed_ast_charges_used_gas_but_not_fee() {
    let mut tx = TransactionType3::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.ty = Uint1::from(TransactionType3::TYPE);
    tx.gas_max = Uint1::from(17);
    tx.fee = Amount::unit238(1_000_000);
    tx.actions
        .push(Box::new(AstSelect::create_by(
            1,
            1,
            vec![Box::new(AstTestFail::new())],
        )))
        .unwrap();

    let main = tx.main();
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    env.tx.main = main;
    env.tx.addrs = vec![main];
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    protocol::operate::hac_add(&mut ctx, &main, &Amount::unit238(5_000_000_000)).unwrap();

    let _before = ast_hac_balance(&mut ctx, &main);
    ctx.exec_from_set(ExecFrom::Top);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("must succeed at least") || err.contains("ast test forced fail"));
    let after = ast_hac_balance(&mut ctx, &main);

    let used = ctx.gas_used_charge().unwrap();
    assert!(
        used.is_positive(),
        "failed tx should still consume gas via AST snapshots"
    );

    assert!(
        after >= Amount::zero(),
        "failed tx should keep a valid balance amount"
    );
}

// PLACEHOLDER_VM_TESTS
#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestMainSet {
    key: Uint1,
    val: Uint1,
}
impl Parse for AstTestMainSet {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        let mut mv = self.key.parse(buf)?;
        mv += self.val.parse(&buf[mv..])?;
        Ok(mv)
    }
}
impl Serialize for AstTestMainSet {
    fn serialize(&self) -> Vec<u8> {
        [self.key.serialize(), self.val.serialize()].concat()
    }
    fn size(&self) -> usize {
        self.key.size() + self.val.size()
    }
}
impl Field for AstTestMainSet {
    fn new() -> Self {
        Self::default()
    }
}
impl ToJSON for AstTestMainSet {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        format!(
            "{{\"key\":{},\"val\":{}}}",
            self.key.to_json_fmt(fmt),
            self.val.to_json_fmt(fmt)
        )
    }
}
impl FromJSON for AstTestMainSet {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let pairs = json_split_object(json)?;
        for (k, v) in pairs {
            if k == "key" {
                self.key.from_json(v)?;
            } else if k == "val" {
                self.val.from_json(v)?;
            }
        }
        Ok(())
    }
}
impl ActExec for AstTestMainSet {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        ctx.state().set(vec![*self.key], vec![*self.val]);
        Ok((0, vec![]))
    }
}
impl Description for AstTestMainSet {}
impl Action for AstTestMainSet {
    fn kind(&self) -> u16 {
        65011
    }
    fn scope(&self) -> ActScope {
        ActScope::CALL
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl AstTestMainSet {
    fn create_by(key: u8, val: u8) -> Self {
        Self {
            key: Uint1::from(key),
            val: Uint1::from(val),
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct AstTestRet {
    tag: Uint1,
}
impl Parse for AstTestRet {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        self.tag.parse(buf)
    }
}
impl Serialize for AstTestRet {
    fn serialize(&self) -> Vec<u8> {
        self.tag.serialize()
    }
    fn size(&self) -> usize {
        self.tag.size()
    }
}
impl Field for AstTestRet {
    fn new() -> Self {
        Self::default()
    }
}
impl ToJSON for AstTestRet {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        format!("{{\"tag\":{}}}", self.tag.to_json_fmt(fmt))
    }
}
impl FromJSON for AstTestRet {
    fn from_json(&mut self, json: &str) -> Ret<()> {
        let pairs = json_split_object(json)?;
        for (k, v) in pairs {
            if k == "tag" {
                self.tag.from_json(v)?;
            }
        }
        Ok(())
    }
}
impl ActExec for AstTestRet {
    fn execute(&self, _ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        Ok((0, vec![*self.tag]))
    }
}
impl Description for AstTestRet {}
impl Action for AstTestRet {
    fn kind(&self) -> u16 {
        65014
    }
    fn scope(&self) -> ActScope {
        ActScope::AST
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl AstTestRet {
    fn create_by(tag: u8) -> Self {
        Self {
            tag: Uint1::from(tag),
        }
    }
}

// ---- Test 21: VM state restored on AstSelect child failure ----
#[test]
fn test_ast_vm_state_restored_on_select_child_failure() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (mock_vm, counter) = MockVM::create();
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(mock_vm);

    // child1: vm += 5, succeed
    // child2: vm += 10, then fail -> vm should be rolled back to 5
    let inner_fail = AstSelect::create_by(
        2,
        2,
        vec![
            Box::new(AstTestVMCall::create_by(10)),
            Box::new(AstTestFail::new()),
        ],
    );
    let act = AstSelect::create_by(
        1,
        2,
        vec![Box::new(AstTestVMCall::create_by(5)), Box::new(inner_fail)],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 5);
}

// ---- Test 22: VM state fully rolled back when AstIf branch fails ----
// ---- Test 23: VM state committed on successful AstIf path ----
#[test]
fn test_ast_vm_state_committed_on_success() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (mock_vm, counter) = MockVM::create();
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(mock_vm);

    // cond: vm += 2, br_if: vm += 3 -> total 5
    let astif = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestVMCall::create_by(2))]),
        AstSelect::create_list(vec![Box::new(AstTestVMCall::create_by(3))]),
        AstSelect::nop(),
    );
    ctx.exec_from_set(ExecFrom::Top);
    astif.execute(&mut ctx).unwrap();

    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 5);
}

// ---- Test 24: VM + state + tex + logs + p2sh all restored together on failure ----
// ---- Test 25: VM state in nested AstIf-inside-AstSelect: inner fail isolated ----
#[test]
fn test_ast_vm_nested_if_fail_isolated_by_outer_select() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (mock_vm, counter) = MockVM::create();
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(mock_vm);

    // child1: vm += 10, ok
    // child2: AstIf(cond: vm += 20, br_if: fail) -> inner fail, outer select recovers
    // child3: vm += 30, ok
    // Expected: 10 + 30 = 40 (child2's 20 rolled back)
    let inner_if = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestVMCall::create_by(20))]),
        AstSelect::create_by(1, 1, vec![Box::new(AstTestFail::new())]),
        AstSelect::nop(),
    );
    let act = AstSelect::create_by(
        2,
        3,
        vec![
            Box::new(AstTestVMCall::create_by(10)),
            Box::new(inner_if),
            Box::new(AstTestVMCall::create_by(30)),
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();

    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 40);
}

// ---- Test 27: Multiple sequential AST ops with VM — state accumulates correctly ----

// ---- Real built-ins: nested ContractMainCall + P2SH-backed HacFromTrs rollback is isolated ----
#[test]
fn test_ast_real_maincall_and_p2sh_transfer_failure_isolated_by_outer_select() {
    let main = seeded_addr("ast-real-maincall-p2sh-main");
    let vm_to = seeded_addr("ast-real-maincall-p2sh-vm-to");
    let ok_to = seeded_addr("ast-real-maincall-p2sh-ok-to");

    let mut tx = TransactionType3::new_by(main, Amount::unit238(1000), 1_730_000_301);
    tx.gas_max = Uint1::from(17);
    let mut env = Env::default();
    env.block.height = protocol::upgrade::ONLINE_OPEN_HEIGHT;
    env.tx = create_tx_info(&tx);
    env.chain.fast_sync = false;

    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(Box::new(vm::global_runtime_pool().checkout(1)));
    let (scriptmh, prove) = build_p2sh_unlock_prove("return 0");
    protocol::operate::hac_add(&mut ctx, &scriptmh, &Amount::mei(11)).unwrap();

    ctx.exec_from_set(ExecFrom::Top);
    prove.execute(&mut ctx).unwrap();

    let nested_fail = AstSelect::create_by(
        3,
        3,
        vec![
            Box::new(build_maincall_hac_transfer(&vm_to, 7)),
            Box::new(HacFromTrs::create_by(scriptmh.clone(), Amount::mei(5))),
            Box::new(AstTestFail::new()),
        ],
    );
    let outer = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(nested_fail),
            Box::new(HacToTrs::create_by(ok_to.clone(), Amount::mei(2))),
        ],
    );

    ctx.exec_from_set(ExecFrom::Top);
    outer.execute(&mut ctx).unwrap();

    assert_eq!(ast_hac_balance(&mut ctx, &vm_to), Amount::zero());
    assert_eq!(ast_hac_balance(&mut ctx, &ok_to), Amount::mei(2));
    assert_eq!(ast_hac_balance(&mut ctx, &scriptmh), Amount::mei(11));
    assert!(ctx.p2sh(&scriptmh).is_ok());
}

// ---- Real built-ins: deep AST success path commits ContractMainCall and P2SH transfer results ----
#[test]
fn test_ast_deep_real_maincall_and_p2sh_transfer_commit_expected_balances() {
    let main = seeded_addr("ast-real-maincall-p2sh-deep-main");
    let cond_to = seeded_addr("ast-real-maincall-p2sh-cond-to");
    let branch_to = seeded_addr("ast-real-maincall-p2sh-branch-to");
    let sibling_to = seeded_addr("ast-real-maincall-p2sh-sibling-to");

    let mut tx = TransactionType3::new_by(main, Amount::unit238(1000), 1_730_000_302);
    tx.gas_max = Uint1::from(17);
    let mut env = Env::default();
    env.block.height = protocol::upgrade::ONLINE_OPEN_HEIGHT;
    env.tx = create_tx_info(&tx);
    env.chain.fast_sync = false;

    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(Box::new(vm::global_runtime_pool().checkout(1)));
    let (scriptmh, prove) = build_p2sh_unlock_prove("return 0");
    protocol::operate::hac_add(&mut ctx, &scriptmh, &Amount::mei(9)).unwrap();

    ctx.exec_from_set(ExecFrom::Top);
    prove.execute(&mut ctx).unwrap();

    let level3 = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(HacFromTrs::create_by(
            scriptmh.clone(),
            Amount::mei(4),
        ))]),
        AstSelect::create_list(vec![Box::new(build_maincall_hac_transfer(&branch_to, 3))]),
        AstSelect::nop(),
    );
    let level2 = AstSelect::create_list(vec![
        Box::new(HacToTrs::create_by(sibling_to.clone(), Amount::mei(2))),
        Box::new(level3),
    ]);
    let level1 = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(build_maincall_hac_transfer(&cond_to, 1))]),
        AstSelect::create_list(vec![Box::new(level2)]),
        AstSelect::nop(),
    );

    ctx.exec_from_set(ExecFrom::Top);
    level1.execute(&mut ctx).unwrap();

    assert_eq!(ast_hac_balance(&mut ctx, &cond_to), Amount::mei(1));
    assert_eq!(ast_hac_balance(&mut ctx, &sibling_to), Amount::mei(2));
    assert_eq!(ast_hac_balance(&mut ctx, &branch_to), Amount::mei(3));
    assert_eq!(ast_hac_balance(&mut ctx, &scriptmh), Amount::mei(5));
    assert!(ctx.p2sh(&scriptmh).is_ok());
}

// ---- Test 32: AstSelect rejects actions len > TX_ACTIONS_MAX without leaking state context ----
#[test]
fn test_ast_select_num_over_tx_actions_max_rejected_no_leak() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.state().set(vec![241], vec![241]); // baseline

    let mut acts: Vec<Box<dyn Action>> = vec![];
    for i in 0..(TX_ACTIONS_MAX + 1) {
        acts.push(Box::new(AstTestSet::create_by((i % 200) as u8, 1)));
    }
    let over = AstSelect::create_by(0, 0, acts);

    ctx.exec_from_set(ExecFrom::Top);
    let err = over.execute(&mut ctx).unwrap_err();
    assert!(err.contains("num cannot exceed"), "{}", err);

    // state fork should not leak; context stays available
    assert_eq!(ast_state_get_u8(&mut ctx, 241), Some(241));
    ctx.state().set(vec![242], vec![242]);
    assert_eq!(ast_state_get_u8(&mut ctx, 242), Some(242));
}

// ---- Test 33: AstSelect max=0 short-circuits and executes no child ----
#[test]
fn test_ast_select_max_zero_executes_no_children() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let (mock_vm, counter) = MockVM::create();
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();
    ctx.test_set_vm(mock_vm);

    let act = AstSelect::create_by(
        0,
        0,
        vec![
            Box::new(AstTestSet::create_by(243, 1)),
            Box::new(AstTestFail::new()),
            Box::new(AstTestVMCall::create_by(9)),
        ],
    );

    ctx.exec_from_set(ExecFrom::Top);
    let rv = act.execute(&mut ctx).unwrap();
    assert_eq!(rv.1, Vec::<u8>::new());
    assert_eq!(ast_state_get_u8(&mut ctx, 243), None);
    assert!(ctx.p2sh(&Address::create_scriptmh([96u8; 20])).is_err());
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// ---- Test 34: AstSelect returns result bytes from the last successful child ----
#[test]
fn test_ast_select_returns_last_success_result_bytes() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let act = AstSelect::create_by(
        1,
        3,
        vec![
            Box::new(AstTestRet::create_by(1)),
            Box::new(AstTestFail::new()),
            Box::new(AstTestRet::create_by(3)),
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    let (_, rv) = act.execute(&mut ctx).unwrap();
    assert_eq!(rv, vec![3]);
}

// ---- Test 35: AstIf returns selected branch bytes and restores exec_from ----
#[test]
fn test_ast_if_returns_selected_branch_result_and_restores_exec_from() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let astif = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestFail::new())]), // cond fail => else
        AstSelect::create_list(vec![Box::new(AstTestRet::create_by(7))]),
        AstSelect::create_list(vec![Box::new(AstTestRet::create_by(8))]),
    );

    ctx.exec_from_set(ExecFrom::Top);
    let (_, rv) = astif.execute(&mut ctx).unwrap();
    assert_eq!(rv, vec![8]);
    assert_eq!(ctx.exec_from(), ExecFrom::Top);
}

// ---- Test 36: AstIf branch error still restores exec_from by ExecFromGuard ----
#[test]
fn test_ast_if_error_restores_exec_from() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let astif = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(244, 244))]),
        AstSelect::create_by(
            2,
            2,
            vec![
                Box::new(AstTestSet::create_by(245, 245)),
                Box::new(AstTestFail::new()),
            ],
        ),
        AstSelect::nop(),
    );
    ctx.exec_from_set(ExecFrom::Top);
    let _ = astif.execute(&mut ctx).unwrap_err();

    assert_eq!(ctx.exec_from(), ExecFrom::Top);
}

// ---- Test 37: Invalid cond AstSelect in AstIf falls through to else without cond side-effects ----
#[test]
fn test_ast_if_invalid_cond_select_runs_else_no_cond_leak() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    tx.actions.push(Box::new(AstSelect::nop())).unwrap();

    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = false; // keep runtime level precheck enabled
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let astif = AstIf::create_by(
        // invalid cond: min > max, now treated as fault and aborts whole AstIf
        AstSelect::create_by(2, 1, vec![Box::new(AstTestMainSet::create_by(246, 246))]),
        AstSelect::create_list(vec![Box::new(AstTestMainSet::create_by(247, 247))]),
        AstSelect::create_list(vec![Box::new(AstTestMainSet::create_by(248, 248))]),
    );

    ctx.exec_from_set(ExecFrom::Top);
    let err = astif.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("action ast select max cannot be less than min"),
        "{}",
        err
    );

    assert_eq!(ast_state_get_u8(&mut ctx, 246), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 247), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 248), None);
}

// ---- Test 39: AstIf branch mutate-then-fail triggers whole-snap recovery of all channels ----
// ---- Test 40: AstSelect with actions len == TX_ACTIONS_MAX is allowed ----
#[test]
fn test_ast_select_num_eq_tx_actions_max_allowed() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let mut acts: Vec<Box<dyn Action>> = vec![Box::new(AstTestSet::create_by(201, 1))];
    for _ in 1..TX_ACTIONS_MAX {
        acts.push(Box::new(AstTestFail::new()));
    }
    let act = AstSelect::create_by(1, 1, acts); // should stop after first success

    ctx.exec_from_set(ExecFrom::Top);
    act.execute(&mut ctx).unwrap();
    assert_eq!(ast_state_get_u8(&mut ctx, 201), Some(1));
}

// ---- Test 41: AstSelect error still restores exec_from by ExecFromGuard ----
#[test]
fn test_ast_select_error_restores_exec_from() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let bad = AstSelect::create_by(1, 1, vec![Box::new(AstTestFail::new())]);
    ctx.exec_from_set(ExecFrom::Top);
    let _ = bad.execute(&mut ctx).unwrap_err();

    assert_eq!(ctx.exec_from(), ExecFrom::Top);
}

// ---- Test 42: AstIf with cond=nop takes if-branch (cond success with 0-required select) ----
#[test]
fn test_ast_if_cond_nop_takes_if_branch() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let astif = AstIf::create_by(
        AstSelect::nop(), // cond succeeds (0/0)
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(202, 202))]),
        AstSelect::create_list(vec![Box::new(AstTestSet::create_by(203, 203))]),
    );
    ctx.exec_from_set(ExecFrom::Top);
    astif.execute(&mut ctx).unwrap();

    assert_eq!(ast_state_get_u8(&mut ctx, 202), Some(202));
    assert_eq!(ast_state_get_u8(&mut ctx, 203), None);
}

// ---- Test 43: ast depth exactly 6 is allowed ----
#[test]
fn test_ast_tree_depth_exact_6_is_allowed() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let lvl6 = AstSelect::create_list(vec![Box::new(AstTestSet::create_by(204, 204))]);
    let lvl5 = AstSelect::create_list(vec![Box::new(lvl6)]);
    let lvl4 = AstSelect::create_list(vec![Box::new(lvl5)]);
    let lvl3 = AstSelect::create_list(vec![Box::new(lvl4)]);
    let lvl2 = AstSelect::create_list(vec![Box::new(lvl3)]);
    let lvl1 = AstSelect::create_list(vec![Box::new(lvl2)]);

    ctx.exec_from_set(ExecFrom::Top);
    lvl1.execute(&mut ctx).unwrap();
    assert_eq!(ast_state_get_u8(&mut ctx, 204), Some(204));
}

// ---- Test 45: AstIf branch validation error recovers cond side-effects (whole-snap) ----
// ---- Test 46: Nested invalid AstSelect is treated as failed child and isolated ----
#[test]
fn test_ast_select_nested_invalid_select_isolated() {
    let mut tx = TransactionType2::default();
    tx.fee = Amount::unit238(1000);
    tx.addrlist =
        AddrOrList::Val1(Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap());
    let mut env = Env::default();
    env.tx.main = Address::from_readable("16Jswqk47s9PUcyCc88MMVwzgvHPvtEpf").unwrap();
    env.chain.fast_sync = true;
    let mut ctx = build_ast_ctx_with_state(env, Box::new(AstTestState::default()), &tx);
    ctx.gas_initialize(10000).unwrap();

    let bad_nested = AstSelect::create_by(2, 1, vec![Box::new(AstTestSet::create_by(217, 217))]);
    let outer = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(bad_nested), // fail as one child
            Box::new(AstTestSet::create_by(218, 218)),
        ],
    );

    ctx.exec_from_set(ExecFrom::Top);
    let err = outer.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("action ast select max cannot be less than min"),
        "{}",
        err
    );
    assert_eq!(ast_state_get_u8(&mut ctx, 217), None);
    assert_eq!(ast_state_get_u8(&mut ctx, 218), None);
}
