use std::sync::Arc;
use std::thread;
use std::time::*;

use basis::component::*;
use basis::config::*;
use basis::interface::*;
use field::*;
use mint::action::*;
use mint::*;
use sys::*;

pub fn start_diamond_auto_bidding(mut worker: Worker, hnode: Arc<dyn HNoder>) {
    // check config
    let eng = hnode.engine();
    let cnf = eng.config();
    let bidmin = cnf.dmer_bid_min.clone();
    let bidmax = cnf.dmer_bid_max.clone();
    let bidstep = cnf.dmer_bid_step.clone();
    let minstep = Amount::coin(1, 244);

    if !cnf.dmer_enable || !cnf.is_mainnet() {
        return; // not enable or not mainnet
    }

    macro_rules! printerr {
        ( $f: expr, $( $v: expr ),+ ) => {
            println!("\n\n{} {}\n\n",
                "[Diamond Auto Bid Config Warning]",
                format!($f, $( $v ),+)
            );
        }
    }

    if bidstep < minstep {
        printerr!("bid step amount cannot be less than {} HAC", &minstep);
    }

    if bidmax < bidmin {
        printerr!(
            "max bid fee {} cannot be less than min fee {}",
            &bidmax,
            &bidmin
        );
        return;
    }

    println!(
        "[Diamond Auto Bidding] start with account {} min fee {} and max fee {}.",
        &cnf.dmer_bid_account.readable(),
        &bidmin,
        &bidmax
    );

    // thread loop
    let engcnf = cnf.clone();
    thread::spawn(move || {
        if worker.sleep_or_quit(Duration::from_secs(15)) {
            return;
        }
        let mut current_number: u32 = 0;
        loop {
            let pending_height = eng.latest_block().height().uint() + 1;
            check_bidding_step(hnode.clone(), &engcnf, pending_height, &mut current_number);
            thread::sleep(Duration::from_millis(77));
            if worker.quit() {
                break;
            }
        }
    });
}

fn check_bidding_step(
    hnode: Arc<dyn HNoder>,
    engcnf: &EngineConf,
    pending_height: u64,
    bidding_number: &mut u32,
) {
    if pending_height % 5 == 0 {
        return; // not need bid in mining block tail 5 and 10
    }

    let txpool = hnode.txpool();
    let txplptr = txpool.as_ref();
    let my_acc = &engcnf.dmer_bid_account;
    let mut bid_step = engcnf.dmer_bid_step.clone();
    let min_step = Amount::coin(1, 244);
    let my_addr = Address::from(*my_acc.address());
    if bid_step < min_step {
        bid_step = min_step;
    }

    macro_rules! retry {
        ($ms: expr) => {
            thread::sleep(Duration::from_millis($ms));
            return
        };
    }

    macro_rules! printerr {
        ( $f: expr, $( $v: expr ),+ ) => {
            println!("\n\n{} {}\n\n",
                "[Diamond Auto Build Error]",
                format!($f, $( $v ),+)
            );
        }
    }

    let (first_bid_txp, my_bid_txp) = pick_first_and_my_bid_tx(txplptr, &my_addr);
    let Some(first_bid_txp) = first_bid_txp else {
        retry!(3); // tx pool empty
    };

    let first_bid_addr = first_bid_txp.tx().main();
    if my_addr == first_bid_addr {
        retry!(1); // im the first
    }

    let first_bid_fee = first_bid_txp.tx().fee();
    if *first_bid_fee > engcnf.dmer_bid_max {
        retry!(10); // my max too low
    }
    let Ok(first_bid_fee) = first_bid_fee.compress(2, AmtCpr::Grow) else {
        printerr!("cannot compress fee {} to 4 length", &first_bid_fee);
        retry!(10); // move step fail
    };

    let Some(my_bid_txp) = my_bid_txp else {
        retry!(3); // have no my tx
    };

    let my_bid_addr = my_bid_txp.tx().main();
    if my_bid_addr == first_bid_addr {
        retry!(1); // im the first
    }

    let my_bid_fee = my_bid_txp.tx().fee();
    if my_bid_fee >= &engcnf.dmer_bid_max {
        retry!(5); // my fee up max
    }

    let Ok(new_bid_fee) = first_bid_fee.add_mode_u64(&bid_step) else {
        printerr!("cannot add fee {} with {}", &first_bid_fee, bid_step);
        retry!(10); // move step fail
    };
    let Ok(mut new_bid_fee) = new_bid_fee.compress(2, AmtCpr::Grow) else {
        printerr!("cannot compress fee {} to 4 length", &new_bid_fee);
        retry!(10); // move step fail
    };
    if new_bid_fee > engcnf.dmer_bid_max {
        new_bid_fee = engcnf.dmer_bid_max.clone()
    }
    if new_bid_fee <= first_bid_fee {
        retry!(10); // my max too low
    }
    // ok
    if let Some(mint) = pickout_diamond_mint_action(my_bid_txp.tx_read()) {
        let act = mint.d;
        let dia = act.diamond.to_readable();
        let dnum = *act.number;
        let dfee = new_bid_fee.to_fin_string();
        if *bidding_number != dnum {
            *bidding_number = dnum;
            flush!(
                "✵✵✵✵ Diamond Auto Bid {}({}) by {} raise fee to ⇨ {}",
                dia,
                dnum,
                my_addr,
                dfee
            );
        } else {
            flush!(" ⇨ {}", dfee);
        }
    }

    // raise fee
    let mut my_tx = my_bid_txp.tx_clone();
    my_tx.set_fee(new_bid_fee.clone());
    // A failed sign must NOT be submitted: an unsigned / mis-signed raised-bid tx
    // is rejected by the node, wasting the bid window and the fee bump.
    if let Err(e) = my_tx.fill_sign(&engcnf.dmer_bid_account) {
        printerr!("raise-bid tx sign error: {}", e);
        retry!(3);
    }
    let txp = TxPkg::create(my_tx);

    // submit tx
    if let Err(e) = hnode.submit_transaction(&txp, false, false) {
        printerr!("submit tx error: {}", e);
        retry!(3); // submit error
    }

    // next check step
}

///////////////////////////////////////////////

fn pick_first_and_my_bid_tx(
    tx_pool: &dyn TxPool,
    my_addr: &Address,
) -> (Option<TxPkg>, Option<TxPkg>) {
    let mut first: Option<TxPkg> = None;
    let mut mine: Option<TxPkg> = None;
    let mut pick_dmint = |a: &TxPkg| {
        if first.is_none() {
            first = Some(a.clone());
        }
        if mine.is_none() && *my_addr == a.tx().main() {
            mine = Some(a.clone());
        }
        if first.is_some() && mine.is_some() {
            return false; // end once both found
        }
        true // next
    };
    let _ = tx_pool.iter_at(TXGID_DIAMINT, &mut pick_dmint);
    // ok
    (first, mine)
}
