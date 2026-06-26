fn console(ctx: &ApiExecCtx, _req: ApiRequest) -> ApiResponse {
    let store = ctx.engine.store();
    let mtcnf = ctx.engine.minter().config().downcast::<MintConf>().unwrap();

    let latest = ctx.engine.latest_block();
    let lathei = latest.height().uint() as i64;
    let latts = latest.timestamp().uint();

    let cyln = mtcnf.difficulty_adjust_blocks as i64;
    let secnp = ["day", "week", "month", "quarter", "year", "all"];
    let secn = [cyln, cyln * 7, cyln * 30, cyln * 90, cyln * 365, lathei - 1];
    let mut target_time = Vec::with_capacity(secn.len());

    for i in 0..secn.len() {
        let sb = secn[i];
        let hei = lathei - sb;
        if hei <= 0 {
            break;
        }
        let Some((_, blkdts)) = store.block_data_by_height(&BlockHeight::from(hei as u64)) else {
            break;
        };
        let mut bhd = BlockIntro::default();
        if bhd.parse(&blkdts).is_err() {
            break;
        }
        let blkt = bhd.timestamp().uint();
        target_time.push(format!("{}: {}s", secnp[i], (latts - blkt) / (sb as u64)));
    }

    let poworkers = {
        let n = ctx.miner_worker_notice_count.lock().unwrap();
        *n
    };

    let pqc_lines = ctx.hnoder.pqc_metrics_prometheus();
    let pqc_summary = if pqc_lines.is_empty() {
        "PQC metrics: n/a".to_owned()
    } else {
        pqc_lines.join(" | ")
    };

    api_html(format!(
        r#"<html><head><title>Hacash node console</title></head><body>
        <h3>Hacash console</h3>
        <p>Latest height {} time {}</p>
        <p>Block span times: {}</p>
        <p>P2P peers: {}</p>
        <p>{}</p>
        <p>Miner worker notice connected: {}</p>
        <p>{}</p>
        <p><a href="/query/metrics">/query/metrics</a> | <a href="/query/metrics?format=prometheus">prometheus</a></p>
    </body></html>"#,
        latest.height().uint(),
        timeshow(latest.timestamp().uint()),
        target_time.join(", "),
        ctx.hnoder.all_peer_prints().join(", "),
        ctx.hnoder.txpool().print(),
        poworkers,
        pqc_summary,
    ))
}
