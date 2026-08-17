use field::{AddrOrPtr, Address, Amount, BytesW2, Field, Hash, Serialize, Sign, Uint4};
use protocol::action::{HacFromToTrs, HacToTrs};
use sha2::{Digest, Sha256};
use sys::Account;
use testkit::sim::memchain::{MemChain, TxOutput};
use vm::value::Value;
use vm::{ContractAddress, VMStateRead};

const DOMAIN: &[u8] = b"HPAY/HVM-CHANNEL-REGISTRY/V2";
const CHALLENGE_BLOCKS: u64 = 12;
const CONTRACT_SOURCE: &str = include_str!("../contracts/hpay_channel_registry_v2.fitsh");
const CONTRACT_MANIFEST: &str = include_str!("../contracts/hpay_channel_registry_v2.manifest.json");

fn account_address(account: &Account) -> Address {
    Address::from(account.address().clone())
}

fn channel_key(prefix: &str, left: &Address) -> Value {
    let mut key = prefix.as_bytes().to_vec();
    key.extend_from_slice(left.as_bytes());
    Value::bytes(key)
}

#[derive(Clone)]
struct Bill {
    network: [u8; 32],
    contract: Address,
    channel_id: [u8; 16],
    reuse: u32,
    left: Address,
    hub: Address,
    total: u64,
    challenge_blocks: u64,
    serial: u64,
    left_balance: u64,
    hub_balance: u64,
    left_sign: Sign,
    hub_sign: Sign,
}

impl Bill {
    fn unsigned(
        network: [u8; 32],
        contract: Address,
        channel_id: [u8; 16],
        reuse: u32,
        left: Address,
        hub: Address,
        total: u64,
        serial: u64,
        left_balance: u64,
        hub_balance: u64,
    ) -> Self {
        Self {
            network,
            contract,
            channel_id,
            reuse,
            left,
            hub,
            total,
            challenge_blocks: CHALLENGE_BLOCKS,
            serial,
            left_balance,
            hub_balance,
            left_sign: Sign::new(),
            hub_sign: Sign::new(),
        }
    }

    fn commitment(&self) -> Hash {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&self.network);
        bytes.extend_from_slice(self.contract.as_bytes());
        bytes.extend_from_slice(&self.channel_id);
        bytes.extend_from_slice(&self.reuse.to_be_bytes());
        bytes.extend_from_slice(self.left.as_bytes());
        bytes.extend_from_slice(self.hub.as_bytes());
        bytes.extend_from_slice(&self.total.to_be_bytes());
        bytes.extend_from_slice(&self.challenge_blocks.to_be_bytes());
        bytes.extend_from_slice(&self.serial.to_be_bytes());
        bytes.extend_from_slice(&self.left_balance.to_be_bytes());
        bytes.extend_from_slice(&self.hub_balance.to_be_bytes());
        Hash::from(sys::sha3(bytes))
    }

    fn sign(mut self, left: &Account, hub: &Account) -> Self {
        let commitment = self.commitment();
        self.left_sign = Sign::create_by(left, &commitment);
        self.hub_sign = Sign::create_by(hub, &commitment);
        self
    }
}

fn contract_call_source(contract: &ContractAddress, call: &str) -> String {
    format!(
        "lib Registry = 1: {}\nvar result = Registry.{}\nassert result == 0\nend",
        contract.to_readable(),
        call
    )
}

fn confirm_source(
    chain: &mut MemChain,
    payer: &Account,
    extra_signers: &[&Account],
    contract: &ContractAddress,
    source: &str,
    miner: Address,
) {
    let mut addrs = vec![account_address(payer), contract.to_addr()];
    addrs.extend(extra_signers.iter().map(|account| account_address(account)));
    let hash = chain
        .submit_formal_main_call_fitsh_with_signers(payer, extra_signers, addrs, source, u8::MAX)
        .expect("build signed registry call");
    chain
        .confirm_formal_block(miner)
        .expect("execute registry call block")
        .expect_success(&hash);
}

fn confirm_call(
    chain: &mut MemChain,
    payer: &Account,
    extra_signers: &[&Account],
    contract: &ContractAddress,
    call: &str,
    miner: Address,
) {
    confirm_source(
        chain,
        payer,
        extra_signers,
        contract,
        &contract_call_source(contract, call),
        miner,
    );
}

fn confirm_deposit(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    amount: u64,
    miner: Address,
) {
    let payer_address = account_address(payer);
    let mut action = HacToTrs::new();
    action.to = AddrOrPtr::from_addr(contract.to_addr());
    action.hacash = Amount::zhu(amount);
    let hash = chain
        .submit_formal_actions(
            payer,
            vec![payer_address, contract.to_addr()],
            vec![Box::new(action)],
            u8::MAX,
            TxOutput::None,
        )
        .expect("build registry deposit");
    chain
        .confirm_formal_block(miner)
        .expect("execute registry deposit")
        .expect_success(&hash);
}

fn submit_payout(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    recipient: Address,
    amount: u64,
) -> Hash {
    let payer_address = account_address(payer);
    let mut action = HacFromToTrs::new();
    action.from = AddrOrPtr::from_addr(contract.to_addr());
    action.to = AddrOrPtr::from_addr(recipient);
    action.hacash = Amount::zhu(amount);
    chain
        .submit_formal_actions(
            payer,
            vec![payer_address, contract.to_addr(), recipient],
            vec![Box::new(action)],
            u8::MAX,
            TxOutput::None,
        )
        .expect("build registry payout")
}

fn submit_call(
    chain: &mut MemChain,
    payer: &Account,
    extra_signers: &[&Account],
    contract: &ContractAddress,
    call: &str,
) -> Hash {
    let mut addrs = vec![account_address(payer), contract.to_addr()];
    addrs.extend(extra_signers.iter().map(|account| account_address(account)));
    chain
        .submit_formal_main_call_fitsh_with_signers(
            payer,
            extra_signers,
            addrs,
            &contract_call_source(contract, call),
            u8::MAX,
        )
        .expect("build registry call")
}

fn confirm_call_failure(
    chain: &mut MemChain,
    payer: &Account,
    extra_signers: &[&Account],
    contract: &ContractAddress,
    call: &str,
    miner: Address,
) {
    let hash = submit_call(chain, payer, extra_signers, contract, call);
    let block = chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute expected-failure registry call");
    assert!(
        block
            .receipt(&hash)
            .expect("expected-failure registry receipt")
            .is_error(),
        "registry call unexpectedly succeeded: {call}"
    );
}

fn bill_call(name: &str, bill: &Bill) -> String {
    format!(
        "{}({}, {}, {}, {}, 0x{}, 0x{})",
        name,
        bill.left.to_readable(),
        bill.serial,
        bill.left_balance,
        bill.hub_balance,
        hex::encode(bill.left_sign.serialize()),
        hex::encode(bill.hub_sign.serialize()),
    )
}

struct RegistryFixture {
    chain: MemChain,
    hub: Account,
    left: Account,
    watchtower: Account,
    miner: Address,
    network: [u8; 32],
    contract: ContractAddress,
}

impl RegistryFixture {
    fn new(seed: &str) -> Self {
        let mut chain = MemChain::new();
        chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
        let hub = Account::create_by(&format!("hpay-registry-hub-{seed}")).unwrap();
        let left = Account::create_by(&format!("hpay-registry-left-{seed}")).unwrap();
        let watchtower = Account::create_by(&format!("hpay-registry-watchtower-{seed}")).unwrap();
        let miner =
            account_address(&Account::create_by(&format!("hpay-registry-miner-{seed}")).unwrap());
        let hub_address = account_address(&hub);
        for address in [
            hub_address,
            account_address(&left),
            account_address(&watchtower),
        ] {
            chain.mint_hac(&address, 30_000_000_000_000);
        }
        let network = [0x33_u8; 32];
        let contract = ContractAddress::calculate(&hub_address, &Uint4::from(0));
        let mut deploy = vm::action::ContractDeploy::new();
        deploy.nonce = Uint4::from(0);
        deploy.construct_argv = BytesW2::from(network.to_vec()).unwrap();
        deploy.contract = vm::fitshc::compile(CONTRACT_SOURCE).unwrap().0.into_sto();
        deploy.protocol_cost = Amount::unit238(20_000_000_000_000);
        let hash = chain
            .submit_formal_actions(
                &hub,
                vec![hub_address],
                vec![Box::new(deploy)],
                u8::MAX,
                TxOutput::ContractAddress(contract.clone()),
            )
            .unwrap();
        chain
            .confirm_formal_block(miner)
            .unwrap()
            .expect_success(&hash);
        Self {
            chain,
            hub,
            left,
            watchtower,
            miner,
            network,
            contract,
        }
    }

    fn init_call(&self, channel_id: [u8; 16], reuse: u32, deposit: u64) -> String {
        format!(
            "init(0x{}, {}, {}, {}, {}, 100)",
            hex::encode(channel_id),
            reuse,
            account_address(&self.left).to_readable(),
            deposit,
            CHALLENGE_BLOCKS,
        )
    }
}

#[test]
fn shared_registry_source_compiles_and_constructor_is_present() {
    let output = vm::fitshc::compile(CONTRACT_SOURCE).expect("registry v2 must compile");
    let bytes = output.0.serialize();
    assert!(!bytes.is_empty());
    let source_hash = hex::encode(Sha256::digest(CONTRACT_SOURCE.as_bytes()));
    let bytecode_hash = hex::encode(sys::sha3(bytes));
    let manifest: serde_json::Value = serde_json::from_str(CONTRACT_MANIFEST).unwrap();
    assert_eq!(manifest["schema"], "hpay-hvm-channel-registry-manifest/2");
    assert_eq!(manifest["contract_name"], "HPAYChannelRegistryV2");
    assert_eq!(manifest["protocol_domain"], "HPAY/HVM-CHANNEL-REGISTRY/V2");
    assert_eq!(manifest["source_sha256"], source_hash);
    assert_eq!(manifest["bytecode_sha3"], bytecode_hash);
    assert_eq!(
        manifest["required_action_kinds"],
        serde_json::json!([40, 41, 44])
    );
    assert_eq!(
        manifest["deployment_model"]["per_channel_contract_deploy"],
        false
    );
    assert_eq!(
        manifest["channel_model"]["right_hub_deposit"],
        "exactly_zero"
    );
    assert_eq!(
        manifest["registry_storage_keys"].as_array().unwrap().len(),
        6
    );
    assert_eq!(
        manifest["channel_storage_prefixes"]
            .as_array()
            .unwrap()
            .len(),
        12
    );
    assert_eq!(manifest["mainnet_deployment"]["enabled"], false);
    assert_eq!(
        manifest["mainnet_deployment"]["external_audit_complete"],
        false
    );
}

#[test]
fn one_deployment_isolates_two_channels_and_aggregates_hub_claims() {
    const A_DEPOSIT: u64 = 1_000_000;
    const B_DEPOSIT: u64 = 2_000_000;
    const A_LEFT_FINAL: u64 = 700_000;
    const A_HUB_FINAL: u64 = 300_000;
    const B_LEFT_FINAL: u64 = 1_250_000;
    const B_HUB_FINAL: u64 = 750_000;

    let mut chain = MemChain::new();
    chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
    let hub = Account::create_by("hpay-registry-v2-hub").unwrap();
    let left_a = Account::create_by("hpay-registry-v2-left-a").unwrap();
    let left_b = Account::create_by("hpay-registry-v2-left-b").unwrap();
    let watchtower = Account::create_by("hpay-registry-v2-watchtower").unwrap();
    let miner = account_address(&Account::create_by("hpay-registry-v2-miner").unwrap());
    let hub_address = account_address(&hub);
    let left_a_address = account_address(&left_a);
    let left_b_address = account_address(&left_b);
    let watchtower_address = account_address(&watchtower);
    for (address, amount) in [
        (hub_address, 30_000_000_000_000_u64),
        (left_a_address, 10_000_000_000_000),
        (left_b_address, 10_000_000_000_000),
        (watchtower_address, 10_000_000_000_000),
    ] {
        chain.mint_hac(&address, amount);
    }

    let network = [0x22_u8; 32];
    let compiled = vm::fitshc::compile(CONTRACT_SOURCE).unwrap().0.into_sto();
    let contract = ContractAddress::calculate(&hub_address, &Uint4::from(0));
    let mut deploy = vm::action::ContractDeploy::new();
    deploy.nonce = Uint4::from(0);
    deploy.construct_argv = BytesW2::from(network.to_vec()).unwrap();
    deploy.contract = compiled;
    deploy.protocol_cost = Amount::unit238(20_000_000_000_000);
    let deploy_hash = chain
        .submit_formal_actions(
            &hub,
            vec![hub_address],
            vec![Box::new(deploy)],
            u8::MAX,
            TxOutput::ContractAddress(contract.clone()),
        )
        .expect("build shared registry deployment");
    chain
        .confirm_formal_block(miner)
        .expect("execute shared registry deployment")
        .expect_success(&deploy_hash);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_network".to_vec())),
        Value::bytes(network.to_vec())
    );
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_hub".to_vec())),
        Value::Address(hub_address)
    );

    let channel_a = [0xA1_u8; 16];
    let channel_b = [0xB2_u8; 16];
    let init_a = format!(
        "init(0x{}, 0, {}, {}, {}, 100)",
        hex::encode(channel_a),
        left_a_address.to_readable(),
        A_DEPOSIT,
        CHALLENGE_BLOCKS,
    );
    let init_b = format!(
        "init(0x{}, 0, {}, {}, {}, 100)",
        hex::encode(channel_b),
        left_b_address.to_readable(),
        B_DEPOSIT,
        CHALLENGE_BLOCKS,
    );
    confirm_call(&mut chain, &left_a, &[&hub], &contract, &init_a, miner);
    confirm_call(&mut chain, &left_b, &[&hub], &contract, &init_b, miner);
    confirm_deposit(&mut chain, &left_a, &contract, A_DEPOSIT, miner);
    confirm_deposit(&mut chain, &left_b, &contract, B_DEPOSIT, miner);

    assert_eq!(
        chain.storage(&contract, &channel_key("c_status_", &left_a_address)),
        Value::U8(2)
    );
    assert_eq!(
        chain.storage(&contract, &channel_key("c_status_", &left_b_address)),
        Value::U8(2)
    );
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_locked".to_vec())),
        Value::U64(A_DEPOSIT + B_DEPOSIT)
    );
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_open_count".to_vec())),
        Value::U64(2)
    );

    let a_stale = Bill::unsigned(
        network,
        contract.to_addr(),
        channel_a,
        0,
        left_a_address,
        hub_address,
        A_DEPOSIT,
        1,
        800_000,
        200_000,
    )
    .sign(&left_a, &hub);
    confirm_call(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &bill_call("challenge", &a_stale),
        miner,
    );

    let b_final = Bill::unsigned(
        network,
        contract.to_addr(),
        channel_b,
        0,
        left_b_address,
        hub_address,
        B_DEPOSIT,
        1,
        B_LEFT_FINAL,
        B_HUB_FINAL,
    )
    .sign(&left_b, &hub);
    confirm_call(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &bill_call("cooperative_close", &b_final),
        miner,
    );
    assert_eq!(
        chain.storage(&contract, &channel_key("c_status_", &left_a_address)),
        Value::U8(3),
        "closing channel B must not change channel A"
    );
    assert_eq!(
        chain.storage(&contract, &channel_key("c_status_", &left_b_address)),
        Value::U8(4)
    );

    let mut cross_channel = a_stale.clone();
    cross_channel.left = left_b_address;
    let cross_hash = submit_call(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &bill_call("respond", &cross_channel),
    );
    assert!(
        chain
            .confirm_formal_block_observing_failures(miner)
            .expect("execute cross-channel replay block")
            .receipt(&cross_hash)
            .expect("cross-channel replay receipt")
            .is_error()
    );
    assert_eq!(
        chain.storage(&contract, &channel_key("c_serial_", &left_b_address)),
        Value::U64(1),
        "cross-channel replay must not mutate channel B"
    );

    let a_latest = Bill::unsigned(
        network,
        contract.to_addr(),
        channel_a,
        0,
        left_a_address,
        hub_address,
        A_DEPOSIT,
        2,
        A_LEFT_FINAL,
        A_HUB_FINAL,
    )
    .sign(&left_a, &hub);
    confirm_call(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &bill_call("respond", &a_latest),
        miner,
    );
    let Value::U64(a_deadline) =
        chain.storage(&contract, &channel_key("c_deadline_", &left_a_address))
    else {
        panic!("channel A deadline must be u64")
    };
    chain
        .confirm_empty_formal_blocks_to_height(miner, a_deadline)
        .unwrap();
    confirm_call(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &format!("finalize({})", left_a_address.to_readable()),
        miner,
    );

    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_locked".to_vec())),
        Value::U64(0)
    );
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_open_count".to_vec())),
        Value::U64(0)
    );
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_left_claimable".to_vec())),
        Value::U64(A_LEFT_FINAL + B_LEFT_FINAL)
    );
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_hub_claimable".to_vec())),
        Value::U64(A_HUB_FINAL + B_HUB_FINAL)
    );

    let left_a_payout = submit_payout(
        &mut chain,
        &watchtower,
        &contract,
        left_a_address,
        A_LEFT_FINAL,
    );
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&left_a_payout);
    let replay = submit_payout(
        &mut chain,
        &watchtower,
        &contract,
        left_a_address,
        A_LEFT_FINAL,
    );
    chain
        .confirm_formal_block_observing_failures(miner)
        .unwrap()
        .receipt(&replay)
        .expect("left replay receipt")
        .expect_error_contains("HPAY_LEFT_ALREADY_CLAIMED");
    let left_b_payout = submit_payout(
        &mut chain,
        &watchtower,
        &contract,
        left_b_address,
        B_LEFT_FINAL,
    );
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&left_b_payout);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_left_claimable".to_vec())),
        Value::U64(0)
    );

    let first_hub_claim =
        submit_payout(&mut chain, &watchtower, &contract, hub_address, A_HUB_FINAL);
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&first_hub_claim);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_hub_claimable".to_vec())),
        Value::U64(B_HUB_FINAL)
    );
    let second_hub_claim =
        submit_payout(&mut chain, &watchtower, &contract, hub_address, B_HUB_FINAL);
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&second_hub_claim);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"g_hub_claimable".to_vec())),
        Value::U64(0)
    );
    assert_eq!(chain.balance(&contract.to_addr()).to_zhu_u64(), Ok(0));
}

#[test]
fn init_requires_both_parties_and_reuse_invalidates_old_bills() {
    const DEPOSIT: u64 = 900_000;
    let mut fixture = RegistryFixture::new("reuse");
    let left_address = account_address(&fixture.left);
    let hub_address = account_address(&fixture.hub);
    let first_id = [0x41_u8; 16];
    let first_init = fixture.init_call(first_id, 0, DEPOSIT);

    confirm_call_failure(
        &mut fixture.chain,
        &fixture.left,
        &[],
        &fixture.contract,
        &first_init,
        fixture.miner,
    );
    confirm_call_failure(
        &mut fixture.chain,
        &fixture.hub,
        &[],
        &fixture.contract,
        &first_init,
        fixture.miner,
    );
    assert_eq!(
        fixture
            .chain
            .storage(&fixture.contract, &channel_key("c_status_", &left_address)),
        Value::Nil
    );

    confirm_call(
        &mut fixture.chain,
        &fixture.left,
        &[&fixture.hub],
        &fixture.contract,
        &first_init,
        fixture.miner,
    );
    let active_reinit = fixture.init_call([0x42; 16], 1, DEPOSIT);
    confirm_call_failure(
        &mut fixture.chain,
        &fixture.left,
        &[&fixture.hub],
        &fixture.contract,
        &active_reinit,
        fixture.miner,
    );
    confirm_deposit(
        &mut fixture.chain,
        &fixture.left,
        &fixture.contract,
        DEPOSIT,
        fixture.miner,
    );

    let final_bill = Bill::unsigned(
        fixture.network,
        fixture.contract.to_addr(),
        first_id,
        0,
        left_address,
        hub_address,
        DEPOSIT,
        1,
        DEPOSIT,
        0,
    )
    .sign(&fixture.left, &fixture.hub);
    confirm_call(
        &mut fixture.chain,
        &fixture.watchtower,
        &[],
        &fixture.contract,
        &bill_call("cooperative_close", &final_bill),
        fixture.miner,
    );
    let unclaimed_reinit = fixture.init_call([0x42; 16], 1, DEPOSIT);
    confirm_call_failure(
        &mut fixture.chain,
        &fixture.left,
        &[&fixture.hub],
        &fixture.contract,
        &unclaimed_reinit,
        fixture.miner,
    );

    let payout = submit_payout(
        &mut fixture.chain,
        &fixture.watchtower,
        &fixture.contract,
        left_address,
        DEPOSIT,
    );
    fixture
        .chain
        .confirm_formal_block(fixture.miner)
        .unwrap()
        .expect_success(&payout);
    let skipped_reuse = fixture.init_call([0x42; 16], 2, DEPOSIT);
    confirm_call_failure(
        &mut fixture.chain,
        &fixture.left,
        &[&fixture.hub],
        &fixture.contract,
        &skipped_reuse,
        fixture.miner,
    );

    let second_id = [0x42_u8; 16];
    let second_init = fixture.init_call(second_id, 1, DEPOSIT);
    confirm_call(
        &mut fixture.chain,
        &fixture.left,
        &[&fixture.hub],
        &fixture.contract,
        &second_init,
        fixture.miner,
    );
    confirm_deposit(
        &mut fixture.chain,
        &fixture.left,
        &fixture.contract,
        DEPOSIT,
        fixture.miner,
    );

    let old_generation = Bill::unsigned(
        fixture.network,
        fixture.contract.to_addr(),
        first_id,
        0,
        left_address,
        hub_address,
        DEPOSIT,
        2,
        800_000,
        100_000,
    )
    .sign(&fixture.left, &fixture.hub);
    confirm_call_failure(
        &mut fixture.chain,
        &fixture.watchtower,
        &[],
        &fixture.contract,
        &bill_call("challenge", &old_generation),
        fixture.miner,
    );
    assert_eq!(
        fixture
            .chain
            .storage(&fixture.contract, &channel_key("c_serial_", &left_address)),
        Value::U64(0)
    );
    assert_eq!(
        fixture
            .chain
            .storage(&fixture.contract, &channel_key("c_status_", &left_address)),
        Value::U8(2)
    );
}

#[test]
fn permissionless_renewal_recovers_registry_and_exact_channel_keys() {
    const DEPOSIT: u64 = 750_000;
    let mut fixture = RegistryFixture::new("leases");
    let left_address = account_address(&fixture.left);
    let init = fixture.init_call([0x51; 16], 0, DEPOSIT);
    confirm_call(
        &mut fixture.chain,
        &fixture.left,
        &[&fixture.hub],
        &fixture.contract,
        &init,
        fixture.miner,
    );
    confirm_deposit(
        &mut fixture.chain,
        &fixture.left,
        &fixture.contract,
        DEPOSIT,
        fixture.miner,
    );
    confirm_call(
        &mut fixture.chain,
        &fixture.watchtower,
        &[],
        &fixture.contract,
        "renew_registry(100)",
        fixture.miner,
    );
    confirm_call(
        &mut fixture.chain,
        &fixture.watchtower,
        &[],
        &fixture.contract,
        &format!("renew_channel({}, 100)", left_address.to_readable()),
        fixture.miner,
    );

    fixture
        .chain
        .set_height(fixture.chain.height().saturating_add(25_000));
    let manifest: serde_json::Value = serde_json::from_str(CONTRACT_MANIFEST).unwrap();
    let mut recoverable = 0usize;
    for key in manifest["registry_storage_keys"].as_array().unwrap() {
        let key = Value::bytes(key.as_str().unwrap().as_bytes().to_vec());
        let debug = VMStateRead::wrap(fixture.chain.state())
            .debug_storage_get(
                &vm::rt::GasExtra::new(fixture.chain.height()),
                &vm::rt::SpaceCap::new(fixture.chain.height()),
                fixture.chain.height(),
                &fixture.contract.to_addr(),
                &key,
            )
            .unwrap()
            .expect("registry key must remain recoverable");
        recoverable += usize::from(debug.recoverable);
    }
    for prefix in manifest["channel_storage_prefixes"].as_array().unwrap() {
        let key = channel_key(prefix.as_str().unwrap(), &left_address);
        let debug = VMStateRead::wrap(fixture.chain.state())
            .debug_storage_get(
                &vm::rt::GasExtra::new(fixture.chain.height()),
                &vm::rt::SpaceCap::new(fixture.chain.height()),
                fixture.chain.height(),
                &fixture.contract.to_addr(),
                &key,
            )
            .unwrap()
            .expect("channel key must remain recoverable");
        recoverable += usize::from(debug.recoverable);
    }
    assert!(
        recoverable > 0,
        "fixture must reach recoverable lease state"
    );

    confirm_call(
        &mut fixture.chain,
        &fixture.watchtower,
        &[],
        &fixture.contract,
        "renew_registry(100)",
        fixture.miner,
    );
    confirm_call(
        &mut fixture.chain,
        &fixture.watchtower,
        &[],
        &fixture.contract,
        &format!("renew_channel({}, 100)", left_address.to_readable()),
        fixture.miner,
    );
    for key in manifest["registry_storage_keys"].as_array().unwrap() {
        assert_ne!(
            fixture.chain.storage(
                &fixture.contract,
                &Value::bytes(key.as_str().unwrap().as_bytes().to_vec())
            ),
            Value::Nil
        );
    }
    for prefix in manifest["channel_storage_prefixes"].as_array().unwrap() {
        assert_ne!(
            fixture.chain.storage(
                &fixture.contract,
                &channel_key(prefix.as_str().unwrap(), &left_address)
            ),
            Value::Nil
        );
    }
    assert_eq!(
        fixture
            .chain
            .storage(&fixture.contract, &channel_key("c_status_", &left_address)),
        Value::U8(2)
    );
}
