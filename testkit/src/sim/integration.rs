use basis::interface::{Logs, State, TransactionRead};
use field::{Address, Amount};
use protocol::context::ContextInst;
use protocol::transaction::create_tx_info;
use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard, Once, OnceLock};

use crate::sim::context::make_ctx_with_logs;
use crate::sim::tx::{StubTx, StubTxBuilder};

pub fn test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn vm_main_addr() -> Address {
    Address::from_readable("1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9").unwrap()
}

pub fn vm_alt_addr() -> Address {
    Address::from_readable("1EuGe2GU8tDKnHLNfBsgyffx66buK7PP6g").unwrap()
}

pub fn make_stub_tx(ty: u8, main: Address, addrs: Vec<Address>, gas_max: u8) -> StubTx {
    StubTxBuilder::new()
        .ty(ty)
        .main(main)
        .addrs(addrs)
        .fee(Amount::unit238(10_000_000))
        .gas_max(gas_max)
        .tx_size(128)
        .fee_purity(3200)
        .build()
}

pub fn make_ctx_from_tx<'a>(
    height: u64,
    tx: &'a dyn TransactionRead,
    state: Box<dyn State>,
    logs: Box<dyn Logs>,
) -> ContextInst<'a> {
    let mut env = basis::component::Env::default();
    env.block.height = height;
    env.tx = create_tx_info(tx);
    make_ctx_with_logs(env, state, logs, tx)
}

thread_local! {
    static TEST_SETUP_SCOPE: RefCell<Option<protocol::setup::TestSetupScopeGuard>> = const { RefCell::new(None) };
}

fn set_scoped_setup_guard(guard: protocol::setup::TestSetupScopeGuard) {
    TEST_SETUP_SCOPE.with(|cell| {
        *cell.borrow_mut() = Some(guard);
    });
}

pub fn scoped_setup(
    vm_assigner: Option<protocol::setup::FnVmAssignFunc>,
) -> protocol::setup::TestSetupScopeGuard {
    let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
    mint::setup::register_protocol_extensions(&mut setup);
    if let Some(assigner) = vm_assigner {
        vm::action::register(&mut setup);
        setup.action_hook(vm::hook::try_action_hook);
        setup.set_vm_assigner(assigner);
    }
    protocol::setup::install_test_scope(setup)
}

pub fn ensure_standard_protocol_setup_for_tests(
    block_hasher: protocol::setup::FnBlockHasherFunc,
    include_vm_extensions: bool,
) {
    static GLOBAL_SETUP_ONCE: Once = Once::new();
    GLOBAL_SETUP_ONCE.call_once(|| {
        let setup = if include_vm_extensions {
            vm::setup::new_full_protocol_setup(block_hasher)
        } else {
            let mut setup = protocol::setup::new_standard_protocol_setup(block_hasher);
            mint::setup::register_protocol_extensions(&mut setup);
            setup
        };
        protocol::setup::install_once(setup);
    });

    let scoped = if include_vm_extensions {
        vm::setup::new_full_protocol_setup(block_hasher)
    } else {
        let mut scoped = protocol::setup::new_standard_protocol_setup(block_hasher);
        mint::setup::register_protocol_extensions(&mut scoped);
        scoped
    };
    debug_assert!(scoped.has_tx_type(protocol::transaction::TransactionType3::TYPE));
    debug_assert_eq!(scoped.has_vm_assigner(), include_vm_extensions);
    set_scoped_setup_guard(protocol::setup::install_test_scope(scoped));
}

pub fn enable_mint_setup() {
    set_scoped_setup_guard(scoped_setup(None));
}

pub fn set_vm_assigner(assigner: Option<protocol::setup::FnVmAssignFunc>) {
    set_scoped_setup_guard(scoped_setup(assigner));
}

pub fn disable_vm_setup() {
    enable_mint_setup()
}

pub fn enable_default_vm_setup() {
    set_vm_assigner(Some(|height| {
        Box::new(vm::global_runtime_pool().checkout(height))
    }))
}
