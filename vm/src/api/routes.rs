
fn routes() -> Vec<ApiRoute> {
    vec![
        ApiRoute::get("/query/contract/sandboxcall", contract_sandbox_call),
        ApiRoute::debug_get("contract/storage", debug_contract_storage),
        ApiRoute::get("/query/contract/logs", vm_logs_read),
        ApiRoute::post("/operate/contract/logs/delete", vm_logs_del),
    ]
}
