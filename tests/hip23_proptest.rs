//! Property-based HIP-23 tests (proptest).
//! Run: cargo test hip23_proptest_ -- --nocapture

mod common;

use basis::interface::{Action, Transaction, TxExec};
use common::hip23::*;
use field::*;
use common::hip23_errors::{classify_error, Hip23ErrorCode};
use mint::action::AssetCreate;
use protocol::action::*;
use protocol::tex::*;
use proptest::prelude::*;
use sys::Account;

const PROP_BASE: u64 = protocol::upgrade::ONLINE_OPEN_HEIGHT + 10_000;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    /// Balanced HAC TEX swaps settle for random positive zhu amounts.
    #[test]
    fn hip23_proptest_balanced_hac_tex_settles(
        zhu_mei in 1u64..50u64,
    ) {
        init_setup();
        let zhu = zhu_mei * 100_000_000;
        let main_acc = Account::create_by(&format!("hip23-prop-main-{zhu_mei}")).unwrap();
        let pay_acc = Account::create_by(&format!("hip23-prop-pay-{zhu_mei}")).unwrap();
        let get_acc = Account::create_by(&format!("hip23-prop-get-{zhu_mei}")).unwrap();
        let pay = addr_of(&pay_acc);
        let get = addr_of(&get_acc);

        let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, zhu, 0, 0);
        let tx = build_signed_type3(
            &main_acc,
            vec![Box::new(pay_tex), Box::new(get_tex)],
            0,
        );

        let mut ctx = make_ctx(PROP_BASE, tx.as_read());
        seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
        seed_hac(&mut ctx, &pay, zhu_mei + 10);

        tx.execute(&mut ctx).unwrap();
        prop_assert_eq!(hac_mei(&mut ctx, &get), zhu_mei);
    }

    /// end = 0 means unbounded upper; heights above start succeed.
    #[test]
    fn hip23_proptest_height_scope_unlimited_end_zero(
        above in 1u64..500u64,
    ) {
        init_setup();
        let start = PROP_BASE;
        let inside = start + above;
        let main_acc = Account::create_by(&format!("hip23-prop-h0-{above}")).unwrap();
        let main = addr_of(&main_acc);
        let recipient = field::ADDRESS_TWOX.clone();

        let mut guard = HeightScope::new();
        guard.start = BlockHeight::from(start);
        guard.end = BlockHeight::from(0);
        let mut transfer = HacToTrs::new();
        transfer.to = AddrOrPtr::from_addr(recipient.clone());
        transfer.hacash = Amount::mei(1);

        let tx = build_signed_type3(
            &main_acc,
            vec![Box::new(guard), Box::new(transfer)],
            0,
        );
        let mut ctx = make_ctx(inside, tx.as_read());
        seed_hac(&mut ctx, &main, 50);
        tx.execute(&mut ctx).unwrap();
        prop_assert_eq!(hac_mei(&mut ctx, &recipient), 1);
    }

    /// Height inside [start, end] succeeds; outside reverts.
    #[test]
    fn hip23_proptest_height_scope_window(
        offset in 0u64..500u64,
        span in 1u64..500u64,
        outside in 1u64..500u64,
    ) {
        init_setup();
        let start = PROP_BASE;
        let end = start + span;
        let inside = start + offset.min(span);
        let below = start.saturating_sub(outside);
        let above = end + outside;

        let main_acc = Account::create_by(&format!("hip23-prop-h-{offset}-{span}")).unwrap();
        let main = addr_of(&main_acc);
        let recipient = field::ADDRESS_TWOX.clone();

        let mut guard = HeightScope::new();
        guard.start = BlockHeight::from(start);
        guard.end = BlockHeight::from(end);
        let mut transfer = HacToTrs::new();
        transfer.to = AddrOrPtr::from_addr(recipient.clone());
        transfer.hacash = Amount::mei(1);

        let tx = build_signed_type3(
            &main_acc,
            vec![Box::new(guard), Box::new(transfer)],
            0,
        );

        let mut ok_ctx = make_ctx(inside, tx.as_read());
        seed_hac(&mut ok_ctx, &main, 50);
        tx.execute(&mut ok_ctx).unwrap();
        prop_assert_eq!(hac_mei(&mut ok_ctx, &recipient), 1);

        for bad_height in [below, above] {
            let mut bad_ctx = make_ctx(bad_height, tx.as_read());
            seed_hac(&mut bad_ctx, &main, 50);
            let err = tx.execute(&mut bad_ctx).unwrap_err();
            prop_assert!(err.contains("submitted in height between"), "{err}");
        }
    }

    /// Guard-only topologies are always rejected at precheck.
    #[test]
    fn hip23_proptest_guard_only_always_rejected(
        start in 0u64..1_000_000u64,
        end in 0u64..1_000_000u64,
    ) {
        init_setup();
        let mut guard = HeightScope::new();
        guard.start = BlockHeight::from(start);
        guard.end = BlockHeight::from(end);
        let actions: Vec<Box<dyn Action>> = vec![Box::new(guard)];
        let err = protocol::action::precheck_tx_actions(
            protocol::transaction::TransactionType3::TYPE,
            &actions,
        )
        .unwrap_err();
        prop_assert!(err.contains("all GUARD"), "{err}");
    }

    /// Imbalanced TEX always fails settlement.
    #[test]
    fn hip23_proptest_imbalanced_tex_always_fails(
        pay_mei in 2u64..30u64,
        get_mei in 1u64..30u64,
    ) {
        prop_assume!(pay_mei != get_mei);
        init_setup();
        let main_acc = Account::create_by(&format!("hip23-prop-imb-main-{pay_mei}-{get_mei}")).unwrap();
        let pay_acc = Account::create_by(&format!("hip23-prop-imb-pay-{pay_mei}")).unwrap();
        let get_acc = Account::create_by(&format!("hip23-prop-imb-get-{get_mei}")).unwrap();
        let pay = addr_of(&pay_acc);

        let (pay_tex, _) = build_balanced_tex_swap(
            &pay_acc,
            &get_acc,
            pay_mei * 100_000_000,
            0,
            0,
        );
        let mut get_tex = TexCellAct::create_by(addr_of(&get_acc));
        get_tex
            .add_cell(Box::new(CellTrsZhuGet::new(
                Fold64::from(get_mei * 100_000_000).unwrap(),
            )))
            .unwrap();
        get_tex.do_sign(&get_acc).unwrap();

        let tx = build_signed_type3(
            &main_acc,
            vec![Box::new(pay_tex), Box::new(get_tex)],
            0,
        );
        let mut ctx = make_ctx(PROP_BASE, tx.as_read());
        seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
        seed_hac(&mut ctx, &pay, pay_mei + 5);

        let err = tx.execute(&mut ctx).unwrap_err();
        prop_assert!(err.contains("settlement check failed"), "{err}");
    }

    /// Production path: balanced TEX also settles under fast_sync=false.
    #[test]
    fn hip23_proptest_strict_balanced_tex_settles(
        zhu_mei in 1u64..20u64,
    ) {
        init_setup();
        let zhu = zhu_mei * 100_000_000;
        let main_acc = Account::create_by(&format!("hip23-prop-strict-main-{zhu_mei}")).unwrap();
        let pay_acc = Account::create_by(&format!("hip23-prop-strict-pay-{zhu_mei}")).unwrap();
        let get_acc = Account::create_by(&format!("hip23-prop-strict-get-{zhu_mei}")).unwrap();
        let pay = addr_of(&pay_acc);
        let get = addr_of(&get_acc);

        let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, zhu, 0, 0);
        let tx = build_signed_type3(
            &main_acc,
            vec![Box::new(pay_tex), Box::new(get_tex)],
            0,
        );

        let mut ctx = make_ctx_strict(PROP_BASE, tx.as_read());
        seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
        seed_hac(&mut ctx, &pay, zhu_mei + 5);

        tx.execute(&mut ctx).unwrap();
        prop_assert_eq!(hac_mei(&mut ctx, &get), zhu_mei);
    }

    /// Wrong protocol_cost always faults (P4).
    #[test]
    fn hip23_proptest_wrong_protocol_cost_always_fails(
        bad_mei in 1u64..100u64,
        serial in 9000u64..9999u64,
    ) {
        init_setup();
        let main_acc = Account::create_by(&format!("hip23-prop-p4-main-{serial}")).unwrap();
        let issuer = addr_of(&Account::create_by(&format!("hip23-prop-p4-iss-{serial}")).unwrap());

        let mut create = AssetCreate::new();
        create.metadata = AssetSmelt {
            serial: Fold64::from(serial).unwrap(),
            supply: Fold64::from(100).unwrap(),
            decimal: Uint1::from(0),
            issuer,
            ticket: BytesW1::from_str("P4").unwrap(),
            name: BytesW1::from_str("P4").unwrap(),
        };
        create.protocol_cost = Amount::mei(bad_mei);

        let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
        let mut ctx = make_ctx(PROP_BASE, tx.as_read());
        seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
        let err = tx.execute(&mut ctx).unwrap_err();
        prop_assert_eq!(classify_error(&err), Hip23ErrorCode::ProtocolFeeMismatch);
    }

    /// P5: height cond revert selects else branch and succeeds.
    #[test]
    fn hip23_proptest_p5_else_on_height_revert(
        outside in 2u64..200u64,
    ) {
        init_setup();
        let main_acc = Account::create_by(&format!("hip23-prop-p5-{outside}")).unwrap();
        let main = addr_of(&main_acc);
        let recipient = field::ADDRESS_TWOX.clone();
        let start = PROP_BASE;
        let end = start + 50;
        let height = end + outside;

        let mut cond_guard = HeightScope::new();
        cond_guard.start = BlockHeight::from(start);
        cond_guard.end = BlockHeight::from(end);
        let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);
        let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
            recipient.clone(),
            Amount::mei(9),
        ))]);
        let br_else = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
            recipient.clone(),
            Amount::mei(2),
        ))]);
        let act = AstIf::create_by(cond, br_if, br_else);

        let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 17);
        let mut ctx = make_ctx(height, tx.as_read());
        seed_hac(&mut ctx, &main, 200);
        tx.execute(&mut ctx).unwrap();
        prop_assert_eq!(hac_mei(&mut ctx, &recipient), 2);
    }

    /// Random bytes on TEX wire must not panic (fuzz-adjacent).
    #[test]
    fn hip23_proptest_tex_wire_parse_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        init_setup();
        let mut tex = TexCellAct::new();
        let _ = tex.parse(&data);
    }
}