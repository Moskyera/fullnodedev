use crate::action::*;
use crate::block::*;
use crate::context::ContextInst;
use crate::state::EmptyLogs;
use crate::transaction::*;
use basis::component::*;
use basis::interface::*;
use field::*;
use sys::*;

fn install_test_registry() -> crate::setup::TestSetupScopeGuard {
    let mut setup =
        crate::setup::new_standard_protocol_setup(|_, stuff| sys::calculate_hash(stuff));
    setup.action_codec(
        &[TestExtEnvReadOnly::KIND],
        action_env_try_create,
        action_env_try_json_decode,
    );
    crate::setup::install_test_scope(setup)
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct TestExtEnvReadOnly {
    kind: Uint2,
}

impl TestExtEnvReadOnly {
    const KIND: u16 = 0x07f0;
}

impl Parse for TestExtEnvReadOnly {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        self.kind.parse(buf)
    }
}

impl Serialize for TestExtEnvReadOnly {
    fn serialize(&self) -> Vec<u8> {
        self.kind.serialize()
    }
    fn size(&self) -> usize {
        self.kind.size()
    }
}

impl Field for TestExtEnvReadOnly {
    fn new() -> Self {
        Self {
            kind: Uint2::from(Self::KIND),
        }
    }
}

impl ToJSON for TestExtEnvReadOnly {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        format!(r#"{{"kind":{}}}"#, self.kind.to_json_fmt(fmt))
    }
}

impl FromJSON for TestExtEnvReadOnly {
    fn from_json(&mut self, json_str: &str) -> Ret<()> {
        let pairs = json_split_object(json_str)?;
        for (k, v) in pairs {
            if k == "kind" {
                self.kind.from_json(v)?;
            }
        }
        Ok(())
    }
}

impl Description for TestExtEnvReadOnly {
    fn to_description(&self) -> String {
        "Test ext env read only action".to_owned()
    }
}

impl ActExec for TestExtEnvReadOnly {
    fn execute(&self, _ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        Ok((0, vec![1u8]))
    }
}

impl Action for TestExtEnvReadOnly {
    fn kind(&self) -> u16 {
        *self.kind
    }
    fn scope(&self) -> ActScope {
        ActScope::CALL_ONLY
    }
    fn req_sign(&self) -> Vec<AddrOrPtr> {
        vec![]
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn action_env_try_create(kind: u16, buf: &[u8]) -> Ret<Option<(Box<dyn Action>, usize)>> {
    if kind != TestExtEnvReadOnly::KIND {
        return Ok(None);
    }
    let (act, sk) = TestExtEnvReadOnly::create(buf)?;
    Ok(Some((Box::new(act), sk)))
}

fn action_env_try_json_decode(kind: u16, json: &str) -> Ret<Option<Box<dyn Action>>> {
    if kind != TestExtEnvReadOnly::KIND {
        return Ok(None);
    }
    let mut act = TestExtEnvReadOnly::default();
    act.from_json(json)?;
    Ok(Some(Box::new(act)))
}

fn init_action_env_test_registry() {
    let _guard = install_test_registry();
}

struct DummyP2sh;
impl P2sh for DummyP2sh {
    fn code_stuff(&self) -> &[u8] {
        b""
    }
    fn witness(&self) -> &[u8] {
        b""
    }
}

#[test]
fn test_setup_default_uses_sha3_block_hasher() {
    let registry = crate::setup::ProtocolSetup::default();
    let _guard = crate::setup::install_test_scope(registry);
    assert_eq!(
        crate::setup::do_block_hash(1, b"abc"),
        sys::calculate_hash(b"abc")
    );
}

#[derive(Default, Clone)]
struct AstForkableState {
    parent: std::sync::Weak<Box<dyn State>>,
    mem: MemMap,
}

impl State for AstForkableState {
    fn fork_sub(&self, parent: std::sync::Weak<Box<dyn State>>) -> Box<dyn State> {
        Box::new(Self {
            parent,
            mem: MemMap::default(),
        })
    }

    fn merge_sub(&mut self, sta: Box<dyn State>) {
        self.mem.extend(sta.as_mem().clone());
    }

    fn detach(&mut self) {
        self.parent = std::sync::Weak::<Box<dyn State>>::new();
    }

    fn clone_state(&self) -> Box<dyn State> {
        Box::new(self.clone())
    }

    fn as_mem(&self) -> &MemMap {
        &self.mem
    }

    fn get(&self, k: Vec<u8>) -> Option<Vec<u8>> {
        if let Some(v) = self.mem.get(&k) {
            return v.clone();
        }
        if let Some(parent) = self.parent.upgrade() {
            return parent.get(k);
        }
        None
    }

    fn set(&mut self, k: Vec<u8>, v: Vec<u8>) {
        self.mem.insert(k, Some(v));
    }

    fn del(&mut self, k: Vec<u8>) {
        self.mem.insert(k, None);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestLevelNoopAction {
    kind: Uint2,
    scope: ActScope,
    desc: &'static str,
}

impl TestLevelNoopAction {
    const TOP_ONLY_CAN_WITH_GUARD_KIND: u16 = 0x07f2;
    const TOP_UNIQUE_KIND: u16 = 0x07f3;
    const CALL_ONLY_KIND: u16 = 0x07f4;
    const AST_KIND: u16 = 0x07f5;

    fn top_only_can_with_guard() -> Self {
        Self::from_kind(Self::TOP_ONLY_CAN_WITH_GUARD_KIND).unwrap()
    }

    fn top_unique() -> Self {
        Self::from_kind(Self::TOP_UNIQUE_KIND).unwrap()
    }

    fn call_only() -> Self {
        Self::from_kind(Self::CALL_ONLY_KIND).unwrap()
    }

    fn ast() -> Self {
        Self::from_kind(Self::AST_KIND).unwrap()
    }

    fn meta(kind: u16) -> Ret<(ActScope, &'static str)> {
        match kind {
            Self::TOP_ONLY_CAN_WITH_GUARD_KIND => Ok((
                ActScope::TOP_ONLY_CAN_WITH_GUARD,
                "Test top-only-can-with-guard",
            )),
            Self::TOP_UNIQUE_KIND => Ok((ActScope::TOP_UNIQUE, "Test top-unique")),
            Self::CALL_ONLY_KIND => Ok((ActScope::CALL_ONLY, "Test call-only")),
            Self::AST_KIND => Ok((ActScope::AST, "Test ast leaf")),
            _ => errf!("unknown test level noop action kind {}", kind),
        }
    }

    fn from_kind(kind: u16) -> Ret<Self> {
        let (level, desc) = Self::meta(kind)?;
        Ok(Self {
            kind: Uint2::from(kind),
            scope: level,
            desc,
        })
    }

    fn refresh_meta(&mut self) -> Rerr {
        let (level, desc) = Self::meta(*self.kind)?;
        self.scope = level;
        self.desc = desc;
        Ok(())
    }
}

impl Default for TestLevelNoopAction {
    fn default() -> Self {
        Self {
            kind: Uint2::default(),
            scope: ActScope::CALL,
            desc: "",
        }
    }
}

impl Parse for TestLevelNoopAction {
    fn parse(&mut self, buf: &[u8]) -> Ret<usize> {
        let used = self.kind.parse(buf)?;
        self.refresh_meta()?;
        Ok(used)
    }
}

impl Serialize for TestLevelNoopAction {
    fn serialize(&self) -> Vec<u8> {
        self.kind.serialize()
    }

    fn size(&self) -> usize {
        self.kind.size()
    }
}

impl Field for TestLevelNoopAction {
    fn new() -> Self {
        Self::default()
    }
}

impl ToJSON for TestLevelNoopAction {
    fn to_json_fmt(&self, fmt: &JSONFormater) -> String {
        format!(r#"{{"kind":{}}}"#, self.kind.to_json_fmt(fmt))
    }
}

impl FromJSON for TestLevelNoopAction {
    fn from_json(&mut self, json_str: &str) -> Ret<()> {
        let pairs = json_split_object(json_str)?;
        for (k, v) in pairs {
            if k == "kind" {
                self.kind.from_json(v)?;
            }
        }
        self.refresh_meta()
    }
}

impl Description for TestLevelNoopAction {
    fn to_description(&self) -> String {
        self.desc.to_owned()
    }
}

impl ActExec for TestLevelNoopAction {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        if !ctx.env().chain.fast_sync {
            precheck_runtime_action(ctx.env().tx.ty, self, ctx.exec_from())?;
        }
        Ok((0, vec![]))
    }
}

impl Action for TestLevelNoopAction {
    fn kind(&self) -> u16 {
        *self.kind
    }

    fn scope(&self) -> ActScope {
        self.scope
    }

    fn min_tx_type(&self) -> u8 {
        match *self.kind {
            Self::AST_KIND => 3,
            _ => 1,
        }
    }

    fn req_sign(&self) -> Vec<AddrOrPtr> {
        vec![]
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

field::combi_struct! { TestType3GasAction,
    kind     : Uint2
    gas_used : Uint1
    burn     : Bool
}

impl TestType3GasAction {
    const KIND: u16 = 0x07f6;
    fn create_by(gas_used: u8, burn: bool) -> Self {
        Self {
            kind: Uint2::from(Self::KIND),
            gas_used: Uint1::from(gas_used),
            burn: Bool::new(burn),
        }
    }
}

impl Description for TestType3GasAction {
    fn to_description(&self) -> String {
        format!("Test type3 gas action {}", *self.gas_used)
    }
}

impl ActExec for TestType3GasAction {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        if !ctx.env().chain.fast_sync {
            precheck_runtime_action(ctx.env().tx.ty, self, ctx.exec_from())?;
        }
        Ok((*self.gas_used as u32, vec![]))
    }
}

impl Action for TestType3GasAction {
    fn kind(&self) -> u16 {
        *self.kind
    }

    fn scope(&self) -> ActScope {
        ActScope::TOP
    }

    fn min_tx_type(&self) -> u8 {
        3
    }

    fn extra9(&self) -> bool {
        self.burn.check()
    }

    fn req_sign(&self) -> Vec<AddrOrPtr> {
        vec![]
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

field::combi_struct! { TestStateSetAction,
    kind: Uint2
    key: Uint1
    val: Uint1
    mode: Uint1
}

impl TestStateSetAction {
    const KIND: u16 = 0x07f7;

    fn create_by(key: u8, val: u8, mode: u8) -> Self {
        Self {
            kind: Uint2::from(Self::KIND),
            key: Uint1::from(key),
            val: Uint1::from(val),
            mode: Uint1::from(mode),
        }
    }
}

impl Description for TestStateSetAction {
    fn to_description(&self) -> String {
        format!("Test state set {}={}", *self.key, *self.val)
    }
}

impl ActExec for TestStateSetAction {
    fn execute(&self, ctx: &mut dyn Context) -> XRet<(u32, Vec<u8>)> {
        if !ctx.env().chain.fast_sync {
            precheck_runtime_action(ctx.env().tx.ty, self, ctx.exec_from())?;
        }
        ctx.state().set(vec![*self.key], vec![*self.val]);
        match *self.mode {
            0 => Ok((0, vec![])),
            1 => xerr_r!("test state set revert"),
            2 => xerr!("test state set fault"),
            _ => xerr!("test state set invalid mode"),
        }
    }
}

impl Action for TestStateSetAction {
    fn kind(&self) -> u16 {
        *self.kind
    }

    fn scope(&self) -> ActScope {
        ActScope::CALL
    }

    fn min_tx_type(&self) -> u8 {
        1
    }

    fn req_sign(&self) -> Vec<AddrOrPtr> {
        vec![]
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn build_depth_7_ast_select() -> AstSelect {
    let lvl7 = AstSelect::create_list(vec![Box::new(HacToTrs::new())]);
    let lvl6 = AstSelect::create_list(vec![Box::new(lvl7)]);
    let lvl5 = AstSelect::create_list(vec![Box::new(lvl6)]);
    let lvl4 = AstSelect::create_list(vec![Box::new(lvl5)]);
    let lvl3 = AstSelect::create_list(vec![Box::new(lvl4)]);
    let lvl2 = AstSelect::create_list(vec![Box::new(lvl3)]);
    AstSelect::create_list(vec![Box::new(lvl2)])
}

#[test]
fn test_transaction_json_full_cycle() {
    let _guard = install_test_registry();

    // 1. Create a TransactionType2 with some actions
    let mut tx = TransactionType2::default();
    tx.ty = Uint1::from(TransactionType2::TYPE);
    tx.timestamp = Timestamp::from(1730000000);
    tx.fee = Amount::small(1, 244); // 1.0 HAC

    // Add an action: HacToTrs
    let mut act1 = HacToTrs::new();
    act1.to = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    act1.hacash = Amount::small(5, 244); // 5.0 HAC
    tx.actions.push(Box::new(act1)).unwrap();

    // Add another action: DiaToTrs
    let mut act2 = DiaToTrs::new();
    act2.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    act2.diamonds = DiamondNameListMax200::from_readable("WTYUIA").unwrap();
    tx.actions.push(Box::new(act2)).unwrap();

    // Add another action: AssetToTrs
    let mut act3 = AssetToTrs::new();
    act3.to = AddrOrPtr::from_ptr(1);
    act3.asset = AssetAmt::from(100, 500).unwrap();
    tx.actions.push(Box::new(act3)).unwrap();

    // 2. Serialize to Binary
    let bin1 = tx.serialize();

    // 3. Serialize to JSON
    let json = tx.to_json();
    println!("Transaction JSON: {}", json);

    // 4. Deserialize from JSON
    let mut tx2 = TransactionType2::default();
    tx2.from_json(&json).expect("JSON Deserialization failed");

    // 5. Serialize reconstructed to Binary
    let bin2 = tx2.serialize();

    // 6. Compare
    assert_eq!(
        bin1, bin2,
        "Binary mismatch after Transaction JSON round-trip"
    );
    assert_eq!(*tx2.timestamp, 1730000000);
    assert_eq!(tx2.actions.length(), 3);
}

#[test]
fn test_block_json_full_cycle() {
    let _guard = install_test_registry();

    // 1. Create a BlockV1
    let mut block = BlockV1::default();
    block.intro.head.height = BlockHeight::from(100);
    block.intro.head.timestamp = Timestamp::from(1730000000);
    block.intro.head.prevhash = Hash::from([1u8; 32]);

    // Create and add Transaction 1
    let mut tx1 = TransactionType2::default();
    tx1.ty = Uint1::from(TransactionType2::TYPE);
    tx1.timestamp = Timestamp::from(1730000001);
    tx1.fee = Amount::mei(100);
    let mut act1 = HacToTrs::new();
    act1.to = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    act1.hacash = Amount::small(1, 244);
    tx1.actions.push(Box::new(act1)).unwrap();

    block.transactions.push(Box::new(tx1)).unwrap();

    // Create and add Transaction 2 (Prelude)
    let mut tx2 = DefaultPreludeTx::default();
    tx2.address = field::ADDRESS_TWOX.clone();
    tx2.message = Fixed16::from_readable(b"hello prelude   ").unwrap();

    block.transactions.push(Box::new(tx2)).unwrap();

    // Update transaction count in header
    block.intro.head.transaction_count = Uint4::from(block.transactions.length() as u32);

    // 2. Serialize to Binary
    let bin1 = block.serialize();

    // 3. Serialize to JSON
    let json = block.to_json();
    println!("Block JSON: {}", json);

    // 4. Deserialize from JSON
    let mut block2 = BlockV1::default();
    block2
        .from_json(&json)
        .expect("Block JSON Deserialization failed");

    // 5. Serialize reconstructed to Binary
    let bin2 = block2.serialize();

    // 6. Compare
    assert_eq!(bin1, bin2, "Binary mismatch after Block JSON round-trip");
    assert_eq!(block2.transactions.length(), 2);
    assert_eq!(*block2.intro.head.height, 100);
}

#[test]
fn test_block_prelude_transaction_must_return_tx0() {
    let _guard = install_test_registry();

    let mut block = BlockV1::default();

    let mut prelude = DefaultPreludeTx::default();
    prelude.address = field::ADDRESS_TWOX.clone();
    block.transactions.push(Box::new(prelude)).unwrap();

    let mut tx = TransactionType2::default();
    tx.ty = Uint1::from(TransactionType2::TYPE);
    tx.timestamp = Timestamp::from(1730000001);
    tx.fee = Amount::mei(1);
    block.transactions.push(Box::new(tx)).unwrap();

    block.intro.head.transaction_count = Uint4::from(block.transactions.length() as u32);

    let ptx = block.prelude_transaction().unwrap();
    assert_eq!(ptx.ty(), DefaultPreludeTx::TYPE);
    assert_eq!(ptx.author(), Some(field::ADDRESS_TWOX.clone()));
    assert_eq!(ptx.block_reward().cloned(), Some(Amount::small_mei(1)));
}

#[test]
fn test_block_execute_must_credit_reward_and_fees_to_default_prelude() {
    let _guard = install_test_registry();

    let miner_acc = Account::create_by("protocol-default-prelude-main").unwrap();
    let miner = Address::from(*miner_acc.address());
    let payee_acc = Account::create_by("protocol-default-prelude-payee").unwrap();
    let payee = Address::from(*payee_acc.address());

    let mut block = BlockV1::default();
    block.intro.head.height = BlockHeight::from(1);
    block.intro.head.transaction_count = Uint4::from(2u32);
    let mut prelude = DefaultPreludeTx::default();
    prelude.address = miner.clone();
    block.transactions.push(Box::new(prelude)).unwrap();
    let mut paytx = TransactionType2::new_by(miner.clone(), Amount::mei(1), 1730000000);
    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(payee);
    act.hacash = Amount::zhu(1);
    paytx.actions.push(Box::new(act)).unwrap();
    paytx.fill_sign(&miner_acc).unwrap();
    block.transactions.push(Box::new(paytx)).unwrap();

    let chain = ChainInfo {
        id: 0,
        diamond_form: false,
        fast_sync: false,
    };
    let mut state_in: Box<dyn State> = Box::new(AstForkableState::default());
    {
        let mut st = crate::state::CoreState::wrap(state_in.as_mut());
        let mut bls = st.balance(&miner).unwrap_or_default();
        bls.hacash = Amount::mei(10);
        st.balance_set(&miner, &bls);
    }
    let (mut state, _) = block
        .execute(chain, state_in, Box::new(EmptyLogs {}))
        .unwrap();
    let miner_bal = crate::state::CoreState::wrap(state.as_mut())
        .balance(&miner)
        .unwrap_or_default()
        .hacash;
    let expected = Amount::mei(10)
        .sub_mode_u64(&Amount::mei(1))
        .unwrap()
        .sub_mode_u64(&Amount::zhu(1))
        .unwrap()
        .add_mode_u64(&Amount::small_mei(1))
        .unwrap()
        .add_mode_u64(&Amount::mei(1))
        .unwrap();

    assert_eq!(miner_bal, expected);
}

#[test]
fn test_ctx_action_call_must_check_req_sign() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    // tx without any signatures
    let alice = Address::create_privakey([3u8; 20]);
    let tx = TransactionType2::new_by(alice.clone(), Amount::mei(1), 1730000000);

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);

    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    // HacFromTrs requires signature of `from`, but since this action is executed via ctx.action_call,
    // it would bypass tx.req_sign unless ctx_action_call enforces it.
    let mut act = HacFromTrs::new();
    act.from = AddrOrPtr::from_addr(alice.clone());
    act.hacash = Amount::mei(1);
    let bytes = act.serialize();

    let err = ctx
        .action_call(HacFromTrs::KIND, bytes[2..].to_vec())
        .unwrap_err();
    assert!(err.contains("signature") || err.contains("failed") || err.contains("verify"));
}

#[test]
fn test_tx_execute_must_verify_signature_before_actions() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let alice = Address::create_privakey([3u8; 20]);
    let bob = Address::create_privakey([4u8; 20]);
    let mut tx = TransactionType2::new_by(alice.clone(), Amount::mei(1), 1730000000);
    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(bob);
    act.hacash = Amount::mei(1);
    tx.actions.push(Box::new(act)).unwrap();

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("signature") || err.contains("failed") || err.contains("verify"),
        "{}",
        err
    );
}

#[test]
fn test_tx_execute_must_reject_action_count_over_max() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let mut tx = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);
    for _ in 0..(TX_ACTIONS_MAX + 1) {
        let mut act = HacToTrs::new();
        act.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
        act.hacash = Amount::mei(1);
        tx.actions.push(Box::new(act)).unwrap();
    }

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("one transaction max actions"), "{}", err);
}

#[test]
fn test_tx_execute_must_reject_invalid_top_only_can_with_guard_before_any_action_runs() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let main_acc = Account::create_by("protocol-static-action-set-main").unwrap();
    let main = Address::from(*main_acc.address());
    let mut tx = TransactionType2::new_by(main, Amount::mei(1), 1730000000);

    let mut writer = HacToTrs::new();
    writer.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    writer.hacash = Amount::mei(1);
    tx.actions.push(Box::new(writer)).unwrap();
    tx.actions
        .push(Box::new(TestLevelNoopAction::top_only_can_with_guard()))
        .unwrap();
    tx.fill_sign(&main_acc).unwrap();

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::mei(5);
        state.balance_set(&main, &bls);
    }

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("TOP_ONLY_CAN_WITH_GUARD"), "{}", err);
    let bls = crate::state::CoreState::wrap(ctx.state())
        .balance(&main)
        .unwrap_or_default();
    assert_eq!(bls.hacash, Amount::mei(5));
}

#[test]
fn test_precheck_tx_actions_rejects_guard_only_ast_select() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(AstSelect::create_list(vec![Box::new(
        HeightScope::new(),
    )]))];

    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("all GUARD"), "{}", err);
}

#[test]
fn test_precheck_tx_actions_rejects_guard_only_ast_if() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(AstIf::create_by(
        AstSelect::create_list(vec![Box::new(HeightScope::new())]),
        AstSelect::nop(),
        AstSelect::nop(),
    ))];

    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("all GUARD"), "{}", err);
}

#[test]
fn test_precheck_tx_actions_allows_mixed_guard_and_non_guard_leafs_in_ast() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(AstSelect::create_list(vec![
        Box::new(HeightScope::new()),
        Box::new(HacToTrs::new()),
    ]))];

    precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap();
}

#[test]
fn test_precheck_tx_actions_rejects_duplicate_top_unique() {
    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(TestLevelNoopAction::top_unique()),
        Box::new(TestLevelNoopAction::top_unique()),
    ];

    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("TOP_UNIQUE"), "{}", err);
}

#[test]
fn test_precheck_tx_actions_rejects_nested_top_only_can_with_guard_in_ast() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(AstSelect::create_list(vec![Box::new(
        TestLevelNoopAction::top_only_can_with_guard(),
    )]))];

    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("TOP_ONLY_CAN_WITH_GUARD"), "{}", err);
}

#[test]
fn test_precheck_tx_actions_rejects_call_only_in_tx_tree() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(TestLevelNoopAction::call_only())];

    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("CALL_ONLY") && err.contains("TOP"), "{}", err);
}

#[test]
fn test_precheck_runtime_action_depth_accepts_ast_leaf_action() {
    let act = TestLevelNoopAction::ast();
    precheck_runtime_action(TransactionType3::TYPE, &act, ExecFrom::Top).unwrap();
}

#[test]
fn test_precheck_tx_actions_allows_top_level_ast_leaf_action_on_type3() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(TestLevelNoopAction::ast())];
    precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap();
}

#[test]
fn test_precheck_tx_actions_rejects_type2_top_level_ast_leaf_action() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(TestLevelNoopAction::ast())];
    let err = precheck_tx_actions(TransactionType2::TYPE, &actions).unwrap_err();
    assert!(err.contains("requires tx type >= 3"), "{}", err);
}

#[test]
fn test_precheck_tx_actions_allows_nested_ast_leaf_action_on_type3() {
    let actions: Vec<Box<dyn Action>> = vec![Box::new(AstSelect::create_list(vec![Box::new(
        TestLevelNoopAction::ast(),
    )]))];
    precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap();
}

#[test]
fn test_guard_level_error_message_reports_top_and_ast_scope() {
    let err = precheck_runtime_action(TransactionType3::TYPE, &HeightScope::new(), ExecFrom::Call)
        .unwrap_err();
    assert!(err.contains("GUARD") && err.contains("CALL"), "{}", err);
}

#[test]
fn test_precheck_runtime_action_allows_call_only_from_action_call_at_top_ctx() {
    precheck_runtime_action(
        TransactionType3::TYPE,
        &TestLevelNoopAction::call_only(),
        ExecFrom::Call,
    )
    .unwrap();
}

#[test]
fn test_precheck_runtime_action_rejects_call_only_without_action_call_origin() {
    let err = precheck_runtime_action(
        TransactionType3::TYPE,
        &TestLevelNoopAction::call_only(),
        ExecFrom::Top,
    )
    .unwrap_err();
    assert!(err.contains("CALL_ONLY") && err.contains("TOP"), "{}", err);
}

#[test]
fn test_precheck_runtime_action_rejects_guard_from_action_call() {
    let err = precheck_runtime_action(TransactionType3::TYPE, &HeightScope::new(), ExecFrom::Call)
        .unwrap_err();
    assert!(err.contains("GUARD") && err.contains("CALL"), "{}", err);
}

#[test]
fn test_tx_execute_fast_sync_ast_depth_precheck_rejects_7th_level() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType3;

    let mut tx = TransactionType3::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);
    tx.actions
        .push(Box::new(build_depth_7_ast_select()))
        .unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("ast tree depth 7 exceeded max 6"), "{}", err);
}

#[test]
fn test_ctx_action_call_rejects_ast_action_even_in_fast_sync() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let tx = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let bytes = AstSelect::nop().serialize();
    let err = ctx
        .action_call(AstSelect::KIND, bytes[2..].to_vec())
        .unwrap_err();
    assert!(err.contains("AST") && err.contains("CALL"), "{}", err);
}

#[test]
fn test_tx_req_sign_must_be_privakey_address() {
    let _guard = install_test_registry();

    use crate::transaction::TransactionType2;

    let mut tx = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);
    // scriptmh address (non-privakey, cannot sign) - should be ignored by tx.req_sign()
    let scriptmh_addr = Address::create_scriptmh([1u8; 20]);

    let mut act = HacFromTrs::new();
    act.from = AddrOrPtr::from_addr(scriptmh_addr);
    act.hacash = Amount::mei(1);
    tx.actions.push(Box::new(act)).unwrap();

    let signset = tx.req_sign().unwrap();
    assert!(signset.contains(&tx.main()));
    assert!(!signset.contains(&scriptmh_addr));
}

#[test]
fn test_tx_execute_must_reject_nonzero_gas_max_on_type1() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType1;

    let main_acc = Account::create_by("protocol-type1-gas-max-main").unwrap();
    let main = Address::from(*main_acc.address());
    let mut tx = TransactionType1::new_by(main, Amount::mei(1), 1730000000);
    tx.gas_max = Uint1::from(1);

    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    act.hacash = Amount::mei(1);
    tx.actions.push(Box::new(act)).unwrap();
    tx.push_sign(Sign::create_by(&main_acc, &tx.hash()))
        .unwrap();

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("gas_max must be zero"), "{}", err);
}

#[test]
fn test_tx_execute_must_reject_nonzero_ano_mark_on_type2() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let main_acc = Account::create_by("protocol-type2-ano-mark-main").unwrap();
    let main = Address::from(*main_acc.address());
    let mut tx = TransactionType2::new_by(main, Amount::mei(1), 1730000000);
    tx.ano_mark = Fixed1::from([1u8]);

    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    act.hacash = Amount::mei(1);
    tx.actions.push(Box::new(act)).unwrap();
    tx.fill_sign(&main_acc).unwrap();

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("ano_mark must be zero"), "{}", err);
}

#[test]
fn test_tx_execute_must_reject_nonzero_ano_mark_on_type3() {
    let _guard = install_test_registry();

    let main_acc = Account::create_by("protocol-type3-ano-mark-main").unwrap();
    let main = Address::from(*main_acc.address());
    let mut tx = TransactionType3::new_by(main, Amount::mei(1), 1730000000);
    tx.gas_max = Uint1::from(1);
    tx.ano_mark = Fixed1::from([1u8]);
    tx.actions
        .push(Box::new(TestType3GasAction::create_by(1, false)))
        .unwrap();
    tx.fill_sign(&main_acc).unwrap();

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::mei(5);
        state.balance_set(&main, &bls);
    }

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("ano_mark must be zero"), "{}", err);
}

#[test]
fn test_type3_fee_got_does_not_burn_from_action_mark() {
    let _guard = install_test_registry();

    let mut tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1_000_000),
        1730000000,
    );
    tx.gas_max = Uint1::from(1);

    tx.actions
        .push(Box::new(TestType3GasAction::create_by(7, true)))
        .unwrap();

    assert_eq!(tx.fee_got(), tx.fee().clone());
    assert!(tx.fee_purity() > 0);
}

#[test]
fn test_fee_purity_saturates_instead_of_zero_on_large_fee() {
    let _guard = install_test_registry();

    let fee = Amount::coin_u128(u64::MAX as u128 + 1, UNIT_238);
    let tx3 = TransactionType3::new_by(field::ADDRESS_ONEX.clone(), fee.clone(), 1730000000);
    assert!(tx3.fee_purity() > 0);

    let tx2 = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), fee, 1730000000);
    assert!(tx2.fee_purity() > 0);
}

fn run_type3_top_level_gas_case(burn: bool) -> i64 {
    let mut tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1_000_000),
        1730000000,
    );
    tx.gas_max = Uint1::from(1);

    tx.actions
        .push(Box::new(TestType3GasAction::create_by(7, burn)))
        .unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&tx.main()).unwrap_or_default();
        bls.hacash = Amount::unit238(5_000_000_000);
        state.balance_set(&tx.main(), &bls);
    }

    tx.execute(&mut ctx).unwrap();
    crate::context::decode_gas_budget(1) - ctx.gas_remaining()
}

fn build_type3_gas_ctx(budget: i64) -> (ContextInst<'static>, Address) {
    let main = field::ADDRESS_ONEX.clone();
    let tx = Box::new(TransactionType3::new_by(
        main,
        Amount::unit238(1_000_000),
        1730000000,
    ));
    let tx: &'static TransactionType3 = Box::leak(tx);
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::unit238(5_000_000_000);
        state.balance_set(&main, &bls);
    }
    ctx.gas_initialize(budget).unwrap();
    (ctx, main)
}

#[test]
fn test_type3_top_level_action_local_burn_factor_is_applied() {
    let _guard = install_test_registry();

    let plain_used = run_type3_top_level_gas_case(false);
    let burn_used = run_type3_top_level_gas_case(true);

    assert_eq!(plain_used, 0);
    assert_eq!(burn_used, 63);
}

#[test]
fn test_type3_gas_max_zero_allows_plain_zero_surcharge_action() {
    let _guard = install_test_registry();

    let main = field::ADDRESS_ONEX.clone();
    let to = field::ADDRESS_ZERO.clone();
    let mut tx = TransactionType3::new_by(main, Amount::unit238(1000), 1730000000);
    tx.gas_max = Uint1::from(0);
    tx.actions
        .push(Box::new(HacToTrs::create_by(to, Amount::unit238(10))))
        .unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&tx.main()).unwrap_or_default();
        bls.hacash = Amount::unit238(10_000);
        state.balance_set(&tx.main(), &bls);
    }

    tx.execute(&mut ctx).unwrap();
    assert_eq!(ctx.gas_remaining(), 0);
}

#[test]
fn test_transfer_rejects_system_recipient_but_allows_blackhole() {
    let _guard = install_test_registry();

    let main_acc = Account::create_by("protocol-system-recipient-main").unwrap();
    let main = Address::from(*main_acc.address());
    let mut tx = TransactionType2::new_by(main.clone(), Amount::mei(1), 1730000000);
    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    act.hacash = Amount::zhu(1);
    tx.actions.push(Box::new(act)).unwrap();
    tx.fill_sign(&main_acc).unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::mei(10);
        state.balance_set(&main, &bls);
    }

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("cannot transfer to system address"), "{}", err);

    let mut tx_ok = TransactionType2::new_by(main, Amount::mei(1), 1730000000);
    let mut act_ok = HacToTrs::new();
    act_ok.to = AddrOrPtr::from_addr(field::ADDRESS_ZERO.clone());
    act_ok.hacash = Amount::zhu(1);
    tx_ok.actions.push(Box::new(act_ok)).unwrap();
    tx_ok.fill_sign(&main_acc).unwrap();
    tx_ok.execute(&mut ctx).unwrap();
}

fn seed_tex_asset_state(ctx: &mut dyn Context, owner: Address, serial: Fold64, amount: u64) {
    let mut state = crate::state::CoreState::wrap(ctx.state());
    let smelt = AssetSmelt {
        serial,
        supply: Fold64::from(1_000_000).unwrap(),
        decimal: Uint1::from(0),
        issuer: owner,
        ticket: BytesW1::from_str("TEXAST").unwrap(),
        name: BytesW1::from_str("Tex Asset").unwrap(),
    };
    state.asset_set(&serial, &smelt);
    let mut bls = state.balance(&owner).unwrap_or_default();
    bls.asset_set(AssetAmt::from(serial.uint(), amount).unwrap())
        .unwrap();
    state.balance_set(&owner, &bls);
}

fn fund_tex_type3_main(ctx: &mut dyn Context, main: Address) {
    let mut state = crate::state::CoreState::wrap(ctx.state());
    let mut bls = state.balance(&main).unwrap_or_default();
    bls.hacash = Amount::unit238(1_000_000_000_000);
    state.balance_set(&main, &bls);
}

#[test]
fn test_type3_tex_asset_cell_extra9_requires_initialized_gas() {
    let _guard = install_test_registry();
    use crate::tex::*;

    let main = field::ADDRESS_ONEX.clone();
    let pay_acc = Account::create_by_password("tex_type3_asset_pay_zero_gas").unwrap();
    let pay = Address::from(*pay_acc.address());
    let serial = Fold64::from(77).unwrap();

    let mut act = TexCellAct::create_by(pay);
    act.add_cell(Box::new(CellTrsAssetPay::new(
        AssetAmt::from(serial.uint(), 100).unwrap(),
    )))
    .unwrap();
    act.do_sign(&pay_acc).unwrap();
    assert!(act.extra9());

    let mut tx = TransactionType3::new_by(main, Amount::unit238(1000), 1730000000);
    tx.gas_max = Uint1::from(0);
    tx.actions.push(Box::new(act)).unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    fund_tex_type3_main(&mut ctx, tx.main());
    seed_tex_asset_state(&mut ctx, pay, serial, 100);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("gas not initialized"), "{err}");
}

#[test]
fn test_type3_tex_non_asset_cell_still_allows_zero_gas() {
    let _guard = install_test_registry();
    use crate::tex::*;

    let main = field::ADDRESS_ONEX.clone();
    let tex_acc = Account::create_by_password("tex_type3_non_asset_zero_gas").unwrap();
    let tex_addr = Address::from(*tex_acc.address());

    let mut act = TexCellAct::create_by(tex_addr);
    act.add_cell(Box::new(CellCondHeightAtMost::new(100)))
        .unwrap();
    act.do_sign(&tex_acc).unwrap();
    assert!(!act.extra9());

    let mut tx = TransactionType3::new_by(main, Amount::unit238(1000), 1730000000);
    tx.gas_max = Uint1::from(0);
    tx.actions.push(Box::new(act)).unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    fund_tex_type3_main(&mut ctx, tx.main());

    tx.execute(&mut ctx).unwrap();
    assert_eq!(ctx.gas_remaining(), 0);
}

#[test]
fn test_type3_tex_asset_cell_succeeds_with_gas_budget() {
    let _guard = install_test_registry();
    use crate::tex::*;

    let main = field::ADDRESS_ONEX.clone();
    let pay_acc = Account::create_by_password("tex_type3_asset_pay_with_gas").unwrap();
    let get_acc = Account::create_by_password("tex_type3_asset_get_with_gas").unwrap();
    let pay = Address::from(*pay_acc.address());
    let get = Address::from(*get_acc.address());
    let serial = Fold64::from(78).unwrap();

    let mut pay_act = TexCellAct::create_by(pay);
    pay_act
        .add_cell(Box::new(CellTrsAssetPay::new(
            AssetAmt::from(serial.uint(), 100).unwrap(),
        )))
        .unwrap();
    pay_act.do_sign(&pay_acc).unwrap();

    let mut get_act = TexCellAct::create_by(get);
    get_act
        .add_cell(Box::new(CellTrsAssetGet::new(
            AssetAmt::from(serial.uint(), 100).unwrap(),
        )))
        .unwrap();
    get_act.do_sign(&get_acc).unwrap();
    assert!(pay_act.extra9());
    assert!(get_act.extra9());

    let mut tx = TransactionType3::new_by(main, Amount::unit238(1_000_000), 1730000000);
    tx.gas_max = Uint1::from(99);
    tx.actions.push(Box::new(pay_act)).unwrap();
    tx.actions.push(Box::new(get_act)).unwrap();

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    fund_tex_type3_main(&mut ctx, tx.main());
    seed_tex_asset_state(&mut ctx, pay, serial, 100);

    tx.execute(&mut ctx).unwrap();

    let state = crate::state::CoreState::wrap(ctx.state());
    assert_eq!(
        state
            .balance(&get)
            .unwrap_or_default()
            .asset(serial)
            .unwrap()
            .amount
            .uint(),
        100
    );
    assert!(
        state
            .balance(&pay)
            .unwrap_or_default()
            .asset(serial)
            .is_none()
    );
}

#[test]
fn test_ast_select_revert_restores_failed_child_only() {
    let _guard = install_test_registry();
    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut tx = tx;
    tx.gas_max = Uint1::from(17);
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let main = field::ADDRESS_ONEX.clone();
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::unit238(1_000_000_000);
        state.balance_set(&main, &bls);
    }
    ctx.gas_initialize(10_000).unwrap();

    let act = AstSelect::create_by(
        1,
        2,
        vec![
            Box::new(TestStateSetAction::create_by(1, 11, 0)),
            Box::new(TestStateSetAction::create_by(2, 22, 1)),
        ],
    );
    ctx.exec_from_set(ExecFrom::Top);
    let out = act.execute(&mut ctx).unwrap();
    assert_eq!(out.0, 0);
    assert_eq!(ctx.state().get(vec![1]), Some(vec![11]));
    assert_eq!(ctx.state().get(vec![2]), None);
}

#[test]
fn test_ast_if_fault_fast_fails_without_whole_node_recover() {
    let _guard = install_test_registry();
    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut tx = tx;
    tx.gas_max = Uint1::from(17);
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let main = field::ADDRESS_ONEX.clone();
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::unit238(1_000_000_000);
        state.balance_set(&main, &bls);
    }
    ctx.gas_initialize(10_000).unwrap();

    let act = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(TestStateSetAction::create_by(1, 11, 0))]),
        AstSelect::create_list(vec![Box::new(TestStateSetAction::create_by(2, 22, 2))]),
        AstSelect::create_list(vec![Box::new(TestStateSetAction::create_by(3, 33, 0))]),
    );
    ctx.exec_from_set(ExecFrom::Top);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(err.is_fault(), "{err}");
}

#[test]
fn test_gas_refund_enters_settled_state_and_keeps_queries() {
    let (mut ctx, _main) = build_type3_gas_ctx(1000);

    ctx.gas_charge(25).unwrap();
    let used_before = ctx.gas_used_charge().unwrap();
    let max_before = ctx.gas_max_charge().unwrap();

    ctx.gas_refund().unwrap();

    assert_eq!(ctx.gas_used_charge().unwrap(), used_before);
    assert_eq!(ctx.gas_max_charge().unwrap(), max_before);
    assert!(ctx.gas_charge(1).unwrap_err().contains("already settled"));
    assert!(ctx.gas_refund().unwrap_err().contains("already settled"));
}

#[test]
fn test_type3_gas_price_uses_protocol_vm_floor_when_fee_purity_is_low() {
    let main = field::ADDRESS_ONEX.clone();
    let mut tx = TransactionType3::new_by(main, Amount::unit238(1), 1730000000);
    tx.gas_max = Uint1::from(1);
    let tx: &'static TransactionType3 = Box::leak(Box::new(tx));

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::unit238(5_000_000_000);
        state.balance_set(&main, &bls);
    }

    ctx.gas_initialize(100).unwrap();
    assert_eq!(
        ctx.gas_max_charge().unwrap(),
        Amount::unit238(100 * crate::params::VM_LOWEST_FEE_PURITY)
    );

    ctx.gas_charge(25).unwrap();
    assert_eq!(
        ctx.gas_used_charge().unwrap(),
        Amount::unit238(25 * crate::params::VM_LOWEST_FEE_PURITY)
    );
}

#[test]
fn test_gas_charge_out_of_gas_does_not_mutate_remaining() {
    let (mut ctx, _main) = build_type3_gas_ctx(100);

    assert_eq!(ctx.gas_remaining(), 100);
    let err = ctx.gas_charge(101).unwrap_err();
    assert!(err.contains("gas has run out"), "{err}");
    assert_eq!(ctx.gas_remaining(), 100);
}

#[test]
fn test_gas_init_while_running_errors_without_double_precharge() {
    let (mut ctx, main) = build_type3_gas_ctx(1000);

    let before_retry = {
        let state = crate::state::CoreState::wrap(ctx.state());
        state.balance(&main).unwrap_or_default().hacash
    };
    let err = ctx.gas_initialize(500).unwrap_err();
    assert!(err.contains("already initialized"), "{err}");
    let after_retry = {
        let state = crate::state::CoreState::wrap(ctx.state());
        state.balance(&main).unwrap_or_default().hacash
    };
    assert_eq!(after_retry, before_retry);
}

#[test]
fn test_gas_init_after_settle_errors_without_reprecharge() {
    let (mut ctx, main) = build_type3_gas_ctx(1000);

    ctx.gas_charge(25).unwrap();
    ctx.gas_refund().unwrap();

    let after_refund = {
        let state = crate::state::CoreState::wrap(ctx.state());
        state.balance(&main).unwrap_or_default().hacash
    };
    let err = ctx.gas_initialize(500).unwrap_err();
    assert!(err.contains("already settled"), "{err}");
    let after_retry = {
        let state = crate::state::CoreState::wrap(ctx.state());
        state.balance(&main).unwrap_or_default().hacash
    };
    assert_eq!(after_retry, after_refund);
}

#[test]
fn test_ctx_action_call_actenv_does_not_require_tx_main_signature() {
    let _guard = install_test_registry();
    init_action_env_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let tx = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);

    let mut ctx = ContextInst::new(
        env,
        Box::new(crate::context::EmptyState {}),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let res = ctx.action_call(TestExtEnvReadOnly::KIND, vec![]).unwrap();
    assert_eq!(res.1, vec![1u8]);
}

#[test]
fn test_tx_req_sign_must_collect_nested_ast_child_actions() {
    let _guard = install_test_registry();

    use crate::transaction::TransactionType2;

    let mut tx = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);
    let nested_signer = field::ADDRESS_TWOX.clone();

    let mut leaf = HacFromTrs::new();
    leaf.from = AddrOrPtr::from_addr(nested_signer);
    leaf.hacash = Amount::mei(1);

    let nested_if = AstIf::create_by(
        AstSelect::nop(),
        AstSelect::create_list(vec![Box::new(AstSelect::create_list(vec![Box::new(leaf)]))]),
        AstSelect::nop(),
    );
    tx.actions
        .push(Box::new(AstSelect::create_list(vec![Box::new(nested_if)])))
        .unwrap();

    let signset = tx.req_sign().unwrap();
    assert!(signset.contains(&tx.main()));
    assert!(signset.contains(&nested_signer));
}

#[test]
fn test_precheck_runtime_action_rejects_ast_leaf_from_action_call() {
    let err = precheck_runtime_action(
        TransactionType3::TYPE,
        &TestLevelNoopAction::ast(),
        ExecFrom::Call,
    )
    .unwrap_err();
    assert!(err.contains("AST") && err.contains("CALL"), "{}", err);
}

#[test]
fn test_tx_req_sign_astif_must_collect_cond_if_else_and_filter_scriptmh() {
    let _guard = install_test_registry();

    use crate::transaction::TransactionType2;

    let mut tx = TransactionType2::new_by(field::ADDRESS_ONEX.clone(), Amount::mei(1), 1730000000);
    let scriptmh = Address::create_scriptmh([9u8; 20]);

    let mut cond = HacFromTrs::new();
    cond.from = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    cond.hacash = Amount::mei(1);

    let mut br_if = HacFromTrs::new();
    br_if.from = AddrOrPtr::from_addr(scriptmh);
    br_if.hacash = Amount::mei(1);

    let mut br_else = HacFromTrs::new();
    br_else.from = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    br_else.hacash = Amount::mei(1);

    let astif = AstIf::create_by(
        AstSelect::create_list(vec![Box::new(cond)]),
        AstSelect::create_list(vec![Box::new(br_if)]),
        AstSelect::create_list(vec![Box::new(br_else)]),
    );
    tx.actions.push(Box::new(astif)).unwrap();

    let signset = tx.req_sign().unwrap();
    assert!(signset.contains(&field::ADDRESS_ONEX.clone()));
    assert!(signset.contains(&field::ADDRESS_TWOX.clone()));
    assert!(!signset.contains(&scriptmh)); // scriptmh cannot sign
    assert_eq!(signset.len(), 2);
}

#[test]
fn test_precheck_tx_actions_rejects_top_only_can_with_guard_plus_guard_only_ast_wrapper() {
    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(TestLevelNoopAction::top_only_can_with_guard()),
        Box::new(AstSelect::create_list(vec![Box::new(HeightScope::new())])),
    ];
    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("TOP_ONLY_CAN_WITH_GUARD"), "{}", err);
}

#[test]
fn test_ast_select_min_failure_is_revert() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType3;

    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let main = field::ADDRESS_ONEX.clone();
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::unit238(1_000_000_000);
        state.balance_set(&main, &bls);
    }
    ctx.gas_initialize(10_000).unwrap();

    let mut bad_guard = HeightScope::new();
    bad_guard.start = BlockHeight::from(10);
    bad_guard.end = BlockHeight::from(20);
    let act = AstSelect::create_by(1, 1, vec![Box::new(bad_guard)]);
    ctx.exec_from_set(ExecFrom::Top);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(err.is_revert(), "{}", err);
    assert!(err.contains("must succeed at least"), "{}", err);
}

#[test]
fn test_ast_if_rethrow_preserves_revert_kind() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType3;

    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let main = field::ADDRESS_ONEX.clone();
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::unit238(1_000_000_000);
        state.balance_set(&main, &bls);
    }
    ctx.gas_initialize(10_000).unwrap();

    let mut cond_guard = HeightScope::new();
    cond_guard.start = BlockHeight::from(20);
    cond_guard.end = BlockHeight::from(30);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);

    let mut else_guard = HeightScope::new();
    else_guard.start = BlockHeight::from(30);
    else_guard.end = BlockHeight::from(40);
    let br_else = AstSelect::create_by(1, 1, vec![Box::new(else_guard)]);

    let act = AstIf::create_by(cond, AstSelect::nop(), br_else);
    ctx.exec_from_set(ExecFrom::Top);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(err.is_revert(), "{}", err);
}

#[test]
fn test_precheck_tx_actions_rejects_top_only_can_with_guard_plus_non_guard_ast_wrapper() {
    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(TestLevelNoopAction::top_only_can_with_guard()),
        Box::new(AstSelect::create_list(vec![Box::new(HacToTrs::new())])),
    ];
    let err = precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert!(err.contains("TOP_ONLY_CAN_WITH_GUARD"), "{}", err);
}

#[test]
fn test_tx_execute_rejects_type2_ast_leaf_action() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let main_acc = Account::create_by("protocol-ast-leaf-type2-main").unwrap();
    let main = Address::from(*main_acc.address());
    let mut tx = TransactionType2::new_by(main, Amount::mei(1), 1730000000);
    tx.actions
        .push(Box::new(TestLevelNoopAction::ast()))
        .unwrap();
    tx.fill_sign(&main_acc).unwrap();

    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&main).unwrap_or_default();
        bls.hacash = Amount::mei(5);
        state.balance_set(&main, &bls);
    }

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("requires tx type >= 3"), "{}", err);
}

#[test]
fn test_balance_floor_empty_and_duplicate_asset_rejected() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let mut empty = BalanceFloor::new();
    empty.addr = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    let err = empty.execute(&mut ctx).unwrap_err();
    assert!(err.contains("is empty"), "{}", err);

    let mut dup = BalanceFloor::new();
    dup.addr = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    dup.hacash = Amount::mei(1);
    dup.assets
        .push(AssetAmt::from(9527u64, 1u64).unwrap())
        .unwrap();
    dup.assets
        .push(AssetAmt::from(9527u64, 2u64).unwrap())
        .unwrap();
    let err = dup.execute(&mut ctx).unwrap_err();
    assert!(err.contains("duplicate"), "{}", err);
}

#[test]
fn test_balance_floor_success_and_insufficient() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        &tx,
    );
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&field::ADDRESS_ONEX).unwrap_or_default();
        bls.hacash = Amount::mei(1000);
        bls.satoshi = SatoshiAuto::from_satoshi(&Satoshi::from(20u64));
        bls.diamond = DiamondNumberAuto::from_diamond(&DiamondNumber::from(3u32));
        bls.asset_set(AssetAmt::from(88u64, 9u64).unwrap()).unwrap();
        state.balance_set(&field::ADDRESS_ONEX, &bls);
    }

    let mut ok = BalanceFloor::new();
    ok.addr = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    ok.hacash = Amount::mei(900);
    ok.satoshi = Satoshi::from(10u64);
    ok.diamond = DiamondNumber::from(2u32);
    ok.assets
        .push(AssetAmt::from(88u64, 7u64).unwrap())
        .unwrap();
    let _ = ok.execute(&mut ctx).unwrap();

    let mut bad = BalanceFloor::new();
    bad.addr = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    bad.assets
        .push(AssetAmt::from(88u64, 10u64).unwrap())
        .unwrap();
    let err = bad.execute(&mut ctx).unwrap_err();
    assert!(err.contains("lower than floor"), "{}", err);
}

#[test]
fn test_ctx_action_call_must_reject_trailing_bytes() {
    let _guard = install_test_registry();

    use crate::context::ContextInst;
    use crate::state::EmptyLogs;
    use crate::transaction::TransactionType2;

    let tx = TransactionType2::default();
    let mut env = Env::default();
    env.tx = crate::transaction::create_tx_info(&tx);
    let mut ctx = ContextInst::new(
        env,
        Box::new(crate::context::EmptyState {}),
        Box::new(EmptyLogs {}),
        &tx,
    );

    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    act.hacash = Amount::mei(1);
    let mut body = act.serialize()[2..].to_vec();
    body.push(0x00); // trailing garbage

    let err = ctx.action_call(HacToTrs::KIND, body).unwrap_err();
    assert!(err.contains("parse length mismatch"), "{}", err);
}

#[test]
fn test_action_json_create_must_reject_kind_mismatch() {
    let _guard = install_test_registry();

    let json = r#"{"kind":14}"#;
    let err = crate::action::action_json_create(HacToTrs::KIND, json).unwrap_err();
    assert!(err.contains("kind mismatch"), "{}", err);
}

#[test]
fn test_action_json_create_must_reject_missing_required_field_for_diamond_from_to() {
    let _guard = install_test_registry();

    let json = r#"{"kind":6,"to":"1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS","diamonds":"WWWTTT"}"#;
    let err = crate::action::action_json_create(DiaFromToTrs::KIND, json).unwrap_err();
    assert!(err.contains("missing required field"), "{}", err);
    assert!(err.contains("from"), "{}", err);
}

#[test]
fn test_action_json_create_must_reject_missing_required_field_for_diamond_from() {
    let _guard = install_test_registry();

    let json = r#"{"kind":8,"diamonds":"WWWTTT"}"#;
    let err = crate::action::action_json_create(DiaFromTrs::KIND, json).unwrap_err();
    assert!(err.contains("missing required field"), "{}", err);
    assert!(err.contains("from"), "{}", err);
}

#[test]
fn test_action_binary_create_must_reject_kind_mismatch() {
    let _guard = install_test_registry();

    let bytes = HacFromToTrs::new().serialize();
    let err = HacToTrs::create(&bytes).unwrap_err();
    assert!(err.contains("kind mismatch"), "{}", err);
}

#[test]
fn test_complex_json_structure() {
    let _guard = install_test_registry();

    // Test a very complex transaction with many actions
    let mut tx = TransactionType1::default();
    tx.ty = Uint1::from(TransactionType1::TYPE);
    tx.fee = Amount::small(1, 240);

    for i in 1..=20 {
        let mut act = HacToTrs::new();
        act.to = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
        act.hacash = Amount::mei(i as u64);
        tx.actions.push(Box::new(act)).unwrap();
    }

    let json = tx.to_json();
    let mut tx2 = TransactionType1::default();
    tx2.from_json(&json)
        .expect("Complex JSON Deserialization failed");

    assert_eq!(tx.serialize(), tx2.serialize());
    assert_eq!(tx2.actions.length(), 20);
}

#[test]
fn test_transaction_base58check_format_roundtrip() {
    let _guard = install_test_registry();

    // Round-trip with Base58Check format (Address outputs bare, no b58: prefix)
    let mut tx = TransactionType2::default();
    tx.ty = Uint1::from(TransactionType2::TYPE);
    tx.timestamp = Timestamp::from(1730000000);
    tx.fee = Amount::small(1, 244);
    tx.addrlist = AddrOrList::from_addr(
        Address::from_readable("1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS").unwrap(),
    );

    let mut act = HacToTrs::new();
    act.to =
        AddrOrPtr::from_addr(Address::from_readable("1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS").unwrap());
    act.hacash = Amount::small(5, 244);
    tx.actions.push(Box::new(act)).unwrap();

    let fmt_58 = field::JSONFormater {
        unit: "HAC".to_string(),
        binary: field::JSONBinaryFormat::Base58Check,
    };
    let json = tx.to_json_fmt(&fmt_58);

    // Verify Address is output without b58: prefix
    assert!(json.contains("1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"));
    assert!(!json.contains("b58:1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS"));

    let mut tx2 = TransactionType2::default();
    tx2.from_json(&json)
        .expect("Base58Check format JSON parse failed");
    assert_eq!(tx.serialize(), tx2.serialize());
}

fn build_tex_test_ctx<'a>(tx: &'a dyn TransactionRead) -> crate::context::ContextInst<'a> {
    use crate::context::ContextInst;
    use crate::state::EmptyLogs;

    let mut env = Env::default();
    env.chain.fast_sync = true;
    env.tx = crate::transaction::create_tx_info(tx);
    ContextInst::new(
        env,
        Box::new(AstForkableState::default()),
        Box::new(EmptyLogs {}),
        tx,
    )
}

#[test]
fn test_tex_zhu_condition_compares_fractional_hac_balance_exactly() {
    use crate::tex::*;

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&field::ADDRESS_ONEX).unwrap_or_default();
        let half = Amount::zhu(1).ratio_floor(1, 2).unwrap();
        bls.hacash = Amount::zhu(1).add_mode_u128(&half).unwrap();
        state.balance_set(&field::ADDRESS_ONEX, &bls);
    }

    CellCondZhuAtLeast::new(Fold64::from(1).unwrap())
        .execute(&mut ctx, &field::ADDRESS_ONEX)
        .unwrap();
    let err = CellCondZhuEq::new(Fold64::from(1).unwrap())
        .execute(&mut ctx, &field::ADDRESS_ONEX)
        .unwrap_err();
    assert!(err.contains("zhu check failed"), "{}", err);
}

#[test]
fn test_tex_zhu_pay_accepts_fractional_hac_balance() {
    use crate::tex::*;

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&field::ADDRESS_ONEX).unwrap_or_default();
        let half = Amount::zhu(1).ratio_floor(1, 2).unwrap();
        bls.hacash = Amount::zhu(1).add_mode_u128(&half).unwrap();
        state.balance_set(&field::ADDRESS_ONEX, &bls);
    }

    CellTrsZhuPay::new(Fold64::from(1).unwrap())
        .execute(&mut ctx, &field::ADDRESS_ONEX)
        .unwrap();

    let bls = crate::state::CoreState::wrap(ctx.state())
        .balance(&field::ADDRESS_ONEX)
        .unwrap();
    assert_eq!(bls.hacash, Amount::zhu(1).ratio_floor(1, 2).unwrap());
    assert_eq!(ctx.tex_ledger().zhu, 1);
}

#[test]
fn test_tex_zhu_get_accepts_fractional_hac_balance() {
    use crate::tex::*;

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&field::ADDRESS_ONEX).unwrap_or_default();
        bls.hacash = Amount::zhu(1).ratio_floor(1, 2).unwrap();
        state.balance_set(&field::ADDRESS_ONEX, &bls);
    }

    ctx.tex_ledger_mut_top().unwrap().zhu = 1;
    CellTrsZhuGet::new(Fold64::from(1).unwrap())
        .execute(&mut ctx, &field::ADDRESS_ONEX)
        .unwrap();

    let bls = crate::state::CoreState::wrap(ctx.state())
        .balance(&field::ADDRESS_ONEX)
        .unwrap();
    let half = Amount::zhu(1).ratio_floor(1, 2).unwrap();
    assert_eq!(bls.hacash, Amount::zhu(1).add_mode_u128(&half).unwrap());
    assert_eq!(ctx.tex_ledger().zhu, 0);
}

#[test]
fn test_tex_zhu_condition_accepts_exact_one_zhu() {
    use crate::tex::*;

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    {
        let mut state = crate::state::CoreState::wrap(ctx.state());
        let mut bls = state.balance(&field::ADDRESS_ONEX).unwrap_or_default();
        bls.hacash = Amount::zhu(1);
        state.balance_set(&field::ADDRESS_ONEX, &bls);
    }

    CellCondZhuEq::new(Fold64::from(1).unwrap())
        .execute(&mut ctx, &field::ADDRESS_ONEX)
        .unwrap();
}

#[test]
fn test_tex_ledger_direct_write_rejects_call_context() {
    let _guard = install_test_registry();

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    ctx.exec_from_set(ExecFrom::Call);
    let err = ctx.tex_ledger_mut_top().err().unwrap();
    assert!(
        err.contains("tex ledger write only allowed in TOP context"),
        "{err}"
    );
}

#[test]
fn test_tex_ledger_direct_write_rejects_ast_context() {
    let _guard = install_test_registry();

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    ctx.exec_from_set(ExecFrom::Ast);
    let err = ctx.tex_ledger_mut_top().err().unwrap();
    assert!(
        err.contains("tex ledger write only allowed in TOP context"),
        "{err}"
    );
}

#[test]
fn test_p2sh_set_rejects_call_context() {
    let _guard = install_test_registry();

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    ctx.exec_from_set(ExecFrom::Call);
    let err = ctx
        .p2sh_set(Address::create_scriptmh([7u8; 20]), Box::new(DummyP2sh))
        .unwrap_err();
    assert!(
        err.contains("p2sh_set only allowed in TOP context"),
        "{err}"
    );
}

#[test]
fn test_p2sh_set_rejects_ast_context() {
    let _guard = install_test_registry();

    let tx = TransactionType2::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    ctx.exec_from_set(ExecFrom::Ast);
    let err = ctx
        .p2sh_set(Address::create_scriptmh([8u8; 20]), Box::new(DummyP2sh))
        .unwrap_err();
    assert!(
        err.contains("p2sh_set only allowed in TOP context"),
        "{err}"
    );
}

#[test]
fn test_tex_action_rejects_call_context_even_in_fast_sync() {
    let _guard = install_test_registry();
    use crate::tex::*;

    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    let acc = Account::create_by_password("tex_call_ctx_guard").unwrap();
    let addr = Address::from(*acc.address());

    let mut act = TexCellAct::create_by(addr);
    act.add_cell(Box::new(CellCondHeightAtMost::new(100)))
        .unwrap();
    act.do_sign(&acc).unwrap();

    ctx.exec_from_set(ExecFrom::Call);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("TexCellAct can only run in TOP context"),
        "{err}"
    );
}

#[test]
fn test_tex_action_rejects_ast_context_even_in_fast_sync() {
    let _guard = install_test_registry();
    use crate::tex::*;

    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);
    let acc = Account::create_by_password("tex_ast_ctx_guard").unwrap();
    let addr = Address::from(*acc.address());

    let mut act = TexCellAct::create_by(addr);
    act.add_cell(Box::new(CellCondHeightAtMost::new(100)))
        .unwrap();
    act.do_sign(&acc).unwrap();

    ctx.exec_from_set(ExecFrom::Ast);
    let err = act.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("TexCellAct can only run in TOP context"),
        "{err}"
    );
}

#[test]
fn test_tex_diamond_settlement_assigns_fifo_across_interleaved_actions() {
    let _guard = install_test_registry();
    use crate::tex::*;

    let tx = TransactionType3::new_by(
        field::ADDRESS_ONEX.clone(),
        Amount::unit238(1000),
        1730000000,
    );
    let mut ctx = build_tex_test_ctx(&tx);

    let pay1_acc = Account::create_by_password("tex_fifo_pay_1").unwrap();
    let pay2_acc = Account::create_by_password("tex_fifo_pay_2").unwrap();
    let get1_acc = Account::create_by_password("tex_fifo_get_1").unwrap();
    let get2_acc = Account::create_by_password("tex_fifo_get_2").unwrap();
    let pay1 = Address::from(*pay1_acc.address());
    let pay2 = Address::from(*pay2_acc.address());
    let get1 = Address::from(*get1_acc.address());
    let get2 = Address::from(*get2_acc.address());
    let settlement = field::ADDRESS_ONEX.clone();

    let dia_a = DiamondName::from_readable(b"KKKKVA").unwrap();
    let dia_b = DiamondName::from_readable(b"HYXYHY").unwrap();
    let dia_c = DiamondName::from_readable(b"UETWNK").unwrap();

    {
        let mut state = crate::state::CoreState::wrap(ctx.state());

        let mut bls1 = state.balance(&pay1).unwrap_or_default();
        bls1.diamond = DiamondNumberAuto::from_diamond(&DiamondNumber::from(2u32));
        state.balance_set(&pay1, &bls1);

        let mut bls2 = state.balance(&pay2).unwrap_or_default();
        bls2.diamond = DiamondNumberAuto::from_diamond(&DiamondNumber::from(1u32));
        state.balance_set(&pay2, &bls2);

        for (name, owner) in [
            (dia_a.clone(), pay1),
            (dia_b.clone(), pay1),
            (dia_c.clone(), pay2),
        ] {
            let mut dia = DiamondSto::new();
            dia.status = DIAMOND_STATUS_NORMAL;
            dia.address = owner;
            state.diamond_set(&name, &dia);
        }
    }

    let mut act_pay1 = TexCellAct::create_by(pay1);
    act_pay1
        .add_cell(Box::new(CellTrsDiaPay::new(
            DiamondNameListMax200::from_list_checked(vec![dia_b.clone(), dia_a.clone()]).unwrap(),
        )))
        .unwrap();
    act_pay1.do_sign(&pay1_acc).unwrap();

    let mut act_get1 = TexCellAct::create_by(get1);
    act_get1
        .add_cell(Box::new(CellTrsDiaGet::new(DiamondNumber::from(1u32))))
        .unwrap();
    act_get1.do_sign(&get1_acc).unwrap();

    let mut act_pay2 = TexCellAct::create_by(pay2);
    act_pay2
        .add_cell(Box::new(CellTrsDiaPay::new(
            DiamondNameListMax200::from_list_checked(vec![dia_c.clone()]).unwrap(),
        )))
        .unwrap();
    act_pay2.do_sign(&pay2_acc).unwrap();

    let mut act_get2 = TexCellAct::create_by(get2);
    act_get2
        .add_cell(Box::new(CellTrsDiaGet::new(DiamondNumber::from(2u32))))
        .unwrap();
    act_get2.do_sign(&get2_acc).unwrap();

    for act in [&act_pay1, &act_get1, &act_pay2, &act_get2] {
        ctx.exec_from_set(ExecFrom::Top);
        act.execute(&mut ctx).unwrap();
    }

    {
        let state = crate::state::CoreState::wrap(ctx.state());
        assert_eq!(state.diamond(&dia_a).unwrap().address, settlement);
        assert_eq!(state.diamond(&dia_b).unwrap().address, settlement);
        assert_eq!(state.diamond(&dia_c).unwrap().address, settlement);
        assert_eq!(
            state
                .balance(&get1)
                .unwrap_or_default()
                .diamond
                .to_diamond()
                .unwrap(),
            DiamondNumber::from(0u32)
        );
        assert_eq!(
            state
                .balance(&get2)
                .unwrap_or_default()
                .diamond
                .to_diamond()
                .unwrap(),
            DiamondNumber::from(0u32)
        );
    }
    assert_eq!(ctx.tex_ledger().dia, 0);

    crate::tex::do_settlement(&mut ctx).unwrap();

    let state = crate::state::CoreState::wrap(ctx.state());
    assert_eq!(state.diamond(&dia_b).unwrap().address, get1);
    assert_eq!(state.diamond(&dia_a).unwrap().address, get2);
    assert_eq!(state.diamond(&dia_c).unwrap().address, get2);
    assert_eq!(
        state
            .balance(&pay1)
            .unwrap_or_default()
            .diamond
            .to_diamond()
            .unwrap(),
        DiamondNumber::from(0u32)
    );
    assert_eq!(
        state
            .balance(&pay2)
            .unwrap_or_default()
            .diamond
            .to_diamond()
            .unwrap(),
        DiamondNumber::from(0u32)
    );
    assert_eq!(
        state
            .balance(&get1)
            .unwrap_or_default()
            .diamond
            .to_diamond()
            .unwrap(),
        DiamondNumber::from(1u32)
    );
    assert_eq!(
        state
            .balance(&get2)
            .unwrap_or_default()
            .diamond
            .to_diamond()
            .unwrap(),
        DiamondNumber::from(2u32)
    );
    assert_eq!(
        state
            .balance(&settlement)
            .unwrap_or_default()
            .diamond
            .to_diamond()
            .unwrap(),
        DiamondNumber::from(0u32)
    );
}

#[test]
fn test_address_bare_base58check_in_protocol() {
    // Verify Address can parse from JSON with bare base58check (no b58: prefix)
    let addr_str = "1AVRuFXNFi3rdMrPH4hdqSgFrEBnWisWaS";
    let mut addr = Address::default();
    addr.from_json(&format!(r#""{}""#, addr_str)).unwrap();
    assert_eq!(addr.to_readable(), addr_str);

    // Verify AddrOrPtr with Address parses bare base58check
    let mut ptr = AddrOrPtr::default();
    ptr.from_json(&format!(r#""{}""#, addr_str)).unwrap();
    assert_eq!(ptr.to_readable(), addr_str);
}

#[test]
fn test_block_parse_rejects_oversized_transaction_count_without_oom() {
    // Regression test for transactions preallocation DoS
    let _guard = install_test_registry();

    // Build a block whose body holds exactly one parseable transaction.
    let mut block = BlockV1::default();
    block.intro.head.height = BlockHeight::from(1);
    let mut tx = TransactionType2::default();
    tx.ty = Uint1::from(TransactionType2::TYPE);
    tx.timestamp = Timestamp::from(1730000001);
    tx.fee = Amount::mei(1);
    let mut act = HacToTrs::new();
    act.to = AddrOrPtr::from_addr(field::ADDRESS_ONEX.clone());
    act.hacash = Amount::small(1, 244);
    tx.actions.push(Box::new(act)).unwrap();
    block.transactions.push(Box::new(tx)).unwrap();

    // Lie about the count: claim u32::MAX transactions while the body has one.
    block.intro.head.transaction_count = Uint4::from(u32::MAX);

    let bytes = block.serialize();
    let res = block_create(&bytes);
    assert!(
        res.is_err(),
        "block with an oversized transaction_count must be rejected by the parser"
    );
}
