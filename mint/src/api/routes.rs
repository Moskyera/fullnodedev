fn routes() -> Vec<ApiRoute> {
    use ApiRoute as R;
    vec![
        R::get("/", console),
        R::get("/query/block/intro", block_intro),
        R::get("/query/block/recents", block_recents),
        R::get("/query/block/views", block_views),
        R::get("/query/block/datas", block_datas),
        R::get("/query/fee/average", fee_average),
        R::get("/query/transaction", transaction_exist),
        R::get("/query/metrics", query_metrics),
        // Type 2/3/4 transaction builder (tx_type=4 requires v6/v7 main_address)
        R::post("/create/transaction", transaction_build),
        // Legacy Type 2 transfer (main_prikey); Type 4 via tx_type=4 query param
        R::post("/create/coin/transfer", create_coin_transfer),
        // Dedicated Type 4 PQC/hybrid transfer (hybrid_keystore + keystore_pass required)
        R::post("/create/coin/transfer/v4", create_coin_transfer_v4),
        R::post("/submit/transaction", submit_transaction),
        R::post("/submit/block", submit_block),
        R::debug_get("block/txs", debug_block_txs),
        R::debug_get("transaction/receipt", debug_transaction_receipt),
        R::debug_post("transaction/simulate", debug_transaction_simulate),
        R::debug_post("submit/transaction", debug_submit_transaction),
        R::post("/operate/fee/raise", fee_raise),
        R::post("/util/transaction/check", transaction_check),
        // Legacy sign (prikey) + auto Type 4 branch when body is ty=4 and hybrid_keystore set
        R::post("/util/transaction/sign", transaction_sign),
        // Type 4 only: requires hybrid_keystore + keystore_pass (no prikey path)
        R::post("/util/transaction/sign/v4", transaction_sign_v4),
        R::get("/query/hashrate", hashrate),
        R::get("/query/hashrate/logs", hashrate_logs),
        R::get("/query/balance", balance),
        R::get("/query/channel", channel),
        R::get("/query/diamond", diamond),
        R::get("/query/diamond/bidding", diamond_bidding),
        R::get("/query/diamond/views", diamond_views),
        R::get("/query/diamond/engrave", diamond_engrave),
        R::get(
            "/query/diamond/inscription_protocol_cost",
            diamond_inscription_protocol_cost,
        ),
        R::get(
            "/query/diamond/inscription_protocol_cost/append",
            diamond_inscription_protocol_cost_append,
        ),
        R::get(
            "/query/diamond/inscription_protocol_cost/move",
            diamond_inscription_protocol_cost_move,
        ),
        R::get(
            "/query/diamond/inscription_protocol_cost/edit",
            diamond_inscription_protocol_cost_edit,
        ),
        R::get(
            "/query/diamond/inscription_protocol_cost/drop",
            diamond_inscription_protocol_cost_drop,
        ),
        R::get("/query/latest", latest),
        R::get("/query/supply", supply),
        R::get_async("/query/miner/notice", miner_notice),
        R::get("/query/miner/pending", miner_pending),
        R::get("/submit/miner/success", miner_success),
        R::get("/query/diamondminer/init", diamondminer_init),
        R::post("/submit/diamondminer/success", diamondminer_success),
    ]
}