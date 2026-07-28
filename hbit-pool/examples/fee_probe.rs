//! Ask a LIVE node what one of the pool's own blocks paid it in transaction
//! fees, using the exact function the settlement path uses.
//!
//! This exists to make the fee hold-back falsifiable. `block_fees` is the one
//! step of the money path that cannot be exercised without a node: it reads
//! `/query/block/intro?tx_hash_list=true` and then one `/query/transaction` per
//! packed transaction, and every one of those field names is a promise about a
//! node this crate does not build. Run it against a height the pool really won
//! and the answer is either the fee total in units of 0.1 HAC, or the refusal
//! the settlement turns into "settle nothing this cycle".
//!
//! usage: fee_probe <node_base_url> <height> <our_block_hash_hex>

use hbit_pool::{BlockFees, block_fees, http_client};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: fee_probe <node> <height> <block_hash_hex>");
        std::process::exit(2);
    }
    let node = a[1].trim_end_matches('/').to_string();
    let height: u64 = a[2].parse().expect("height");
    let hash = a[3].to_lowercase();

    let client = http_client();
    match block_fees(&client, &node, height, &hash) {
        BlockFees::Counted(u) => {
            println!("Counted({u}) units of 0.1 HAC");
        }
        BlockFees::NotOnChain => {
            println!("NotOnChain (the chain does not hold our block at that height)");
            std::process::exit(1);
        }
        BlockFees::Unknown(why) => {
            println!("Unknown: {why}");
            // The settlement turns this into "nothing is settled this cycle",
            // so a probe that answered 0 here would be worse than useless.
            std::process::exit(1);
        }
    }
}
