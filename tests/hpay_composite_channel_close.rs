use basis::interface::{Transaction, TransactionRead};
use field::{
    AddrHac, AddrOrPtr, Address, Amount, CHANNEL_STATUS_AGREEMENT_CLOSED, CHANNEL_STATUS_OPENING,
    ChannelId, Field, Serialize,
};
use mint::action::{ChannelClose, ChannelOpen};
use protocol::action::HacFromToTrs;
use protocol::transaction::TransactionType2;
use sys::Account;
use testkit::sim::memchain::{MemChain, TxOutput};

const FEE_ZHU: u64 = 100_000;
const USER_START_ZHU: u64 = 5_000_000;
const HUB_START_ZHU: u64 = 1_000_000;
const USER_DEPOSIT_ZHU: u64 = 1_000_000;
const HUB_DEPOSIT_ZHU: u64 = 1_000_000;
const DELTA_ZHU: u64 = 100_000;

struct Fixture {
    chain: MemChain,
    user: Account,
    hub: Account,
    miner: Address,
    channel_id: ChannelId,
}

fn address(account: &Account) -> Address {
    Address::from(account.address().clone())
}

fn amount_zhu(amount: Amount) -> u64 {
    amount.to_238_u64().expect("test amount must fit zhu")
}

fn action14(from: Address, to: Address, amount_zhu: u64) -> HacFromToTrs {
    let mut action = HacFromToTrs::new();
    action.from = AddrOrPtr::from_addr(from);
    action.to = AddrOrPtr::from_addr(to);
    action.hacash = Amount::unit238(amount_zhu);
    action
}

fn signed_type2(
    main: &Account,
    other: &Account,
    timestamp: u64,
    actions: Vec<Box<dyn basis::interface::Action>>,
) -> TransactionType2 {
    let mut tx = TransactionType2::new_by(address(main), Amount::unit238(FEE_ZHU), timestamp);
    for action in actions {
        tx.push_action(action).expect("push action");
    }
    tx.fill_sign(main).expect("main signature");
    tx.fill_sign(other).expect("counterparty signature");
    tx.verify_signature()
        .expect("complete bilateral signatures");
    tx
}

fn open_fixture(seed: u8, hub_start_zhu: u64) -> Fixture {
    open_fixture_with_balances(seed, USER_START_ZHU, hub_start_zhu)
}

fn open_fixture_with_balances(seed: u8, user_start_zhu: u64, hub_start_zhu: u64) -> Fixture {
    let mut chain = MemChain::new();
    let user = Account::create_by(&format!("hpay-composite-user-{seed}")).expect("user account");
    let hub = Account::create_by(&format!("hpay-composite-hub-{seed}")).expect("hub account");
    let miner = address(
        &Account::create_by(&format!("hpay-composite-miner-{seed}")).expect("miner account"),
    );
    let user_address = address(&user);
    let hub_address = address(&hub);
    let channel_id = ChannelId::from([seed; 16]);

    chain.mint_hac(&user_address, user_start_zhu);
    chain.mint_hac(&hub_address, hub_start_zhu);

    let mut open = ChannelOpen::new();
    open.channel_id = channel_id;
    open.left_bill = AddrHac {
        address: user_address,
        amount: Amount::unit238(USER_DEPOSIT_ZHU),
    };
    open.right_bill = AddrHac {
        address: hub_address,
        amount: Amount::unit238(HUB_DEPOSIT_ZHU),
    };
    let open_tx = signed_type2(&user, &hub, 1_730_100_001, vec![Box::new(open)]);
    let open_hash = chain
        .submit_signed_transaction_raw(&open_tx.serialize(), TxOutput::None)
        .expect("submit exact Type2 open bytes");
    chain
        .confirm_formal_block(miner)
        .expect("execute open in a real formal block")
        .expect_success(&open_hash);

    let channel = chain.channel(&channel_id).expect("opened channel state");
    assert_eq!(channel.status, CHANNEL_STATUS_OPENING);
    assert_eq!(*channel.reuse_version, 1);

    Fixture {
        chain,
        user,
        hub,
        miner,
        channel_id,
    }
}

fn close_action(channel_id: ChannelId) -> ChannelClose {
    let mut close = ChannelClose::new();
    close.channel_id = channel_id;
    close
}

#[test]
fn composite_close_user_to_hub_executes_exact_final_balances() {
    let mut fixture = open_fixture(11, HUB_START_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_002,
        vec![
            Box::new(close_action(fixture.channel_id)),
            Box::new(action14(user_address, hub_address, DELTA_ZHU)),
        ],
    );
    assert_eq!(
        close_tx
            .actions()
            .iter()
            .map(|a| a.kind())
            .collect::<Vec<_>>(),
        vec![3, 14]
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .expect("submit exact composite close bytes");
    fixture
        .chain
        .confirm_formal_block(fixture.miner)
        .expect("execute composite close in a real formal block")
        .expect_success(&hash);

    assert_eq!(
        amount_zhu(fixture.chain.balance(&user_address)),
        USER_START_ZHU - (2 * FEE_ZHU) - DELTA_ZHU
    );
    assert_eq!(
        amount_zhu(fixture.chain.balance(&hub_address)),
        HUB_START_ZHU + DELTA_ZHU
    );
    assert_eq!(
        fixture.chain.channel(&fixture.channel_id).unwrap().status,
        CHANNEL_STATUS_AGREEMENT_CLOSED
    );
}

#[test]
fn composite_close_hub_to_user_executes_exact_final_balances() {
    let mut fixture = open_fixture(12, HUB_START_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_003,
        vec![
            Box::new(close_action(fixture.channel_id)),
            Box::new(action14(hub_address, user_address, DELTA_ZHU)),
        ],
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .expect("submit exact reverse composite close bytes");
    fixture
        .chain
        .confirm_formal_block(fixture.miner)
        .expect("execute reverse composite close in a real formal block")
        .expect_success(&hash);

    assert_eq!(
        amount_zhu(fixture.chain.balance(&user_address)),
        USER_START_ZHU - (2 * FEE_ZHU) + DELTA_ZHU
    );
    assert_eq!(
        amount_zhu(fixture.chain.balance(&hub_address)),
        HUB_START_ZHU - DELTA_ZHU
    );
}

#[test]
fn unchanged_close_executes_action3_only() {
    let mut fixture = open_fixture(13, HUB_START_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_004,
        vec![Box::new(close_action(fixture.channel_id))],
    );
    assert_eq!(
        close_tx
            .actions()
            .iter()
            .map(|a| a.kind())
            .collect::<Vec<_>>(),
        vec![3]
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .expect("submit unchanged close bytes");
    fixture
        .chain
        .confirm_formal_block(fixture.miner)
        .expect("execute unchanged close in a real formal block")
        .expect_success(&hash);

    assert_eq!(
        amount_zhu(fixture.chain.balance(&user_address)),
        USER_START_ZHU - (2 * FEE_ZHU)
    );
    assert_eq!(
        amount_zhu(fixture.chain.balance(&hub_address)),
        HUB_START_ZHU
    );
}

#[test]
fn failed_action14_rolls_back_action3_fee_and_balances() {
    let mut fixture = open_fixture(14, HUB_DEPOSIT_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let before_state = fixture.chain.state_entries();
    let before_user = fixture.chain.balance(&user_address);
    let before_hub = fixture.chain.balance(&hub_address);

    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_005,
        vec![
            Box::new(close_action(fixture.channel_id)),
            Box::new(action14(hub_address, user_address, HUB_DEPOSIT_ZHU + 1)),
        ],
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .expect("submit failing composite close bytes");
    let block = fixture
        .chain
        .confirm_formal_block_observing_failures(fixture.miner)
        .expect("observe failed production transaction execution");
    block
        .receipt(&hash)
        .expect("failed transaction receipt")
        .expect_error_contains("insufficient");

    assert_eq!(fixture.chain.state_entries(), before_state);
    assert_eq!(fixture.chain.balance(&user_address), before_user);
    assert_eq!(fixture.chain.balance(&hub_address), before_hub);
    assert_eq!(
        fixture.chain.channel(&fixture.channel_id).unwrap().status,
        CHANNEL_STATUS_OPENING
    );
}

#[test]
fn close_rolls_back_when_final_user_principal_cannot_pay_fee() {
    let mut fixture = open_fixture_with_balances(15, USER_DEPOSIT_ZHU + FEE_ZHU, HUB_DEPOSIT_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let before_state = fixture.chain.state_entries();
    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_006,
        vec![
            Box::new(close_action(fixture.channel_id)),
            Box::new(action14(user_address, hub_address, USER_DEPOSIT_ZHU)),
        ],
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .unwrap();
    let block = fixture
        .chain
        .confirm_formal_block_observing_failures(fixture.miner)
        .unwrap();
    block
        .receipt(&hash)
        .unwrap()
        .expect_error_contains("insufficient");
    assert_eq!(fixture.chain.state_entries(), before_state);
    assert_eq!(
        fixture.chain.channel(&fixture.channel_id).unwrap().status,
        CHANNEL_STATUS_OPENING
    );
}

#[test]
fn close_succeeds_when_final_user_principal_exactly_covers_fee() {
    let mut fixture = open_fixture_with_balances(16, USER_DEPOSIT_ZHU + FEE_ZHU, HUB_DEPOSIT_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_007,
        vec![
            Box::new(close_action(fixture.channel_id)),
            Box::new(action14(
                user_address,
                hub_address,
                USER_DEPOSIT_ZHU - FEE_ZHU,
            )),
        ],
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .unwrap();
    fixture
        .chain
        .confirm_formal_block(fixture.miner)
        .unwrap()
        .expect_success(&hash);
    assert_eq!(amount_zhu(fixture.chain.balance(&user_address)), 0);
}

#[test]
fn close_succeeds_when_final_user_principal_exceeds_fee() {
    let mut fixture = open_fixture_with_balances(17, USER_DEPOSIT_ZHU + FEE_ZHU, HUB_DEPOSIT_ZHU);
    let user_address = address(&fixture.user);
    let hub_address = address(&fixture.hub);
    let close_tx = signed_type2(
        &fixture.user,
        &fixture.hub,
        1_730_100_008,
        vec![
            Box::new(close_action(fixture.channel_id)),
            Box::new(action14(
                user_address,
                hub_address,
                USER_DEPOSIT_ZHU - (2 * FEE_ZHU),
            )),
        ],
    );
    let hash = fixture
        .chain
        .submit_signed_transaction_raw(&close_tx.serialize(), TxOutput::None)
        .unwrap();
    fixture
        .chain
        .confirm_formal_block(fixture.miner)
        .unwrap()
        .expect_success(&hash);
    assert_eq!(amount_zhu(fixture.chain.balance(&user_address)), FEE_ZHU);
}
