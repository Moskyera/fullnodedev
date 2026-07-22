fn debug_contract_storage(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let height = ctx.engine.latest_block().height().uint() + 1;
    let contract = req.query("contract").unwrap_or("");
    let key = req.query("key").unwrap_or("");
    let kind = req.query("kind").unwrap_or("storage");

    let Ok(addr) = Address::from_readable(contract) else {
        return api_error("contract address format invalid");
    };
    if ContractAddress::from_addr(addr).is_err() {
        return api_error("contract address version error");
    }
    if key.is_empty() {
        return api_error("key cannot be empty");
    }

    let args = match machine::parse_sandbox_params(key) {
        Ok(v) => v,
        Err(e) => return api_error(&e),
    };
    if args.len() != 1 {
        return api_error("key must decode to exactly one value");
    }
    let Some(key) = args.into_iter().next() else {
        return api_error("key cannot be empty");
    };

    let gst = GasExtra::new(height);
    let cap = SpaceCap::new(height);
    let staptr = ctx.engine.state();
    let state = VMStateRead::wrap(staptr.as_ref().as_ref());

    match kind {
        "status" => match state.debug_status_get(&cap, &addr, &key) {
            Ok(v) => api_data_raw(format!(
                r#""height":{},"kind":"status","exists":{},"value":{}"#,
                height,
                !matches!(v, Value::Nil),
                v.to_debug_json()
            )),
            Err(e) => api_error(&e.to_string()),
        },
        "storage" | "" => match state.debug_storage_get(&gst, &cap, height, &addr, &key) {
            Ok(Some(info)) => api_data_raw(format!(
                r#""height":{},"kind":"storage","exists":true,"active":{},"recoverable":{},"live_rest":{},"recover_rest":{},"value":{}"#,
                height,
                info.active,
                info.recoverable,
                info.live_blocks,
                info.recover_blocks,
                info.value.to_debug_json()
            )),
            Ok(None) => api_data_raw(format!(
                r#""height":{},"kind":"storage","exists":false,"active":false,"recoverable":false,"live_rest":0,"recover_rest":0,"value":{}"#,
                height,
                Value::Nil.to_debug_json()
            )),
            Err(e) => api_error(&e.to_string()),
        },
        _ => api_error("kind must be storage or status"),
    }
}
