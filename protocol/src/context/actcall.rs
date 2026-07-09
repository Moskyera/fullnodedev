use crate::action;

fn ctx_action_call(this: &mut dyn Context, k: u16, b: Vec<u8>) -> XRet<(u32, Vec<u8>)> {
    // create
    let mut body = Vec::with_capacity(2 + b.len());
    body.extend_from_slice(&k.to_be_bytes());
    body.extend_from_slice(&b);
    let (action, used) = action::action_create(&body)?;
    if used != body.len() {
        return xerrf!(
            "action parse length mismatch: consumed {} but body length is {}",
            used,
            body.len()
        );
    }
    action::precheck_runtime_action(this.env().tx.ty, action.as_ref(), ExecFrom::Call)?;
    // ACTION payload actions are runtime-created and not part of tx.actions.
    // Keep runtime req_sign checks here; tx.main signature is already verified in tx.execute().
    let mut seen = HashSet::new();
    for ptr in action.req_sign() {
        let adr = this.addr(&ptr)?;
        if !seen.insert(adr) {
            continue;
        }
        if adr.is_user_signing_address() {
            this.check_sign(&adr)?;
        }
    }
    // Runtime-created actions always execute in CALL context.
    let (gas, res) = with_exec_from(this, ExecFrom::Call, |ctx| action.execute(ctx))?;
    // Runtime ACTION returned-gas uses the same delta-only extra9 surcharge rule as other
    // Type3 composition charge sites; plain actions add no returned-gas charge in this path.
    let gas = extra9_surcharge(action.extra9(), gas);
    Ok((gas, res))
}
