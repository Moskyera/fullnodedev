pub fn new_full_protocol_setup(
    block_hasher: protocol::setup::FnBlockHasherFunc,
) -> protocol::setup::ProtocolSetup {
    let mut setup = protocol::setup::new_standard_protocol_setup(block_hasher);
    mint::setup::register_protocol_extensions(&mut setup);
    register_protocol_extensions(&mut setup);
    setup
}

pub fn install_full_test_scope(
    block_hasher: protocol::setup::FnBlockHasherFunc,
) -> protocol::setup::TestSetupScopeGuard {
    protocol::setup::install_test_scope(new_full_protocol_setup(block_hasher))
}

pub fn register_protocol_extensions(setup: &mut protocol::setup::ProtocolSetup) {
    crate::action::register(setup);
    setup.action_hook(crate::hook::try_action_hook);
    setup.set_vm_assigner(|height| Box::new(crate::global_runtime_pool().checkout(height)));
}
