fn vm_log_delete_header_auth(req: &ApiRequest) -> Option<&str> {
    if let Some(value) = req.headers.get("x-vm-log-delete-auth") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value);
        }
    }
    req.headers.get("authorization").and_then(|value| {
        let (scheme, token) = value.split_once(' ')?;
        let token = token.trim();
        if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
            Some(token)
        } else {
            None
        }
    })
}

fn vm_log_delete_auth_eq(provided: &str, expected: &str) -> bool {
    let provided = provided.as_bytes();
    let expected = expected.as_bytes();
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn authorize_vm_log_delete(
    can_delete: bool,
    configured_auth: &str,
    req: &ApiRequest,
) -> Result<(), &'static str> {
    if !can_delete {
        return Err("vm log deletion is disabled");
    }
    let configured_auth = configured_auth.trim();
    if configured_auth.is_empty() {
        return Err("vm log deletion auth is not configured");
    }
    let provided = vm_log_delete_header_auth(req).ok_or("auth failed")?;
    if !vm_log_delete_auth_eq(provided, configured_auth) {
        return Err("auth failed");
    }
    Ok(())
}

fn vm_logs_del(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let conf = ctx.engine.config();
    if let Err(error) = authorize_vm_log_delete(
        conf.vm_log_can_delete,
        &conf.vm_log_delete_auth_hash,
        &req,
    ) {
        return api_error(error);
    }
    let hei = req.query_u64("height", 0);
    ctx.engine.logs().remove(hei);
    api_data_raw(r#""ok":true"#.to_owned())
}

#[cfg(test)]
mod vm_log_delete_tests {
    use super::*;

    fn request_with_header(name: &str, value: &str) -> ApiRequest {
        let mut req = ApiRequest::default();
        req.headers.insert(name.to_owned(), value.to_owned());
        req
    }

    #[test]
    fn delete_route_is_post_only() {
        let route = routes()
            .into_iter()
            .find(|route| route.path == "/operate/contract/logs/delete")
            .unwrap();
        assert_eq!(route.method, ApiMethod::Post);
    }

    #[test]
    fn delete_requires_both_enable_flag_and_configured_auth() {
        let req = request_with_header("x-vm-log-delete-auth", "secret");
        assert_eq!(
            authorize_vm_log_delete(false, "secret", &req),
            Err("vm log deletion is disabled"),
        );
        assert_eq!(
            authorize_vm_log_delete(true, "", &req),
            Err("vm log deletion auth is not configured"),
        );
    }

    #[test]
    fn delete_rejects_query_string_auth() {
        let mut req = ApiRequest::default();
        req.query.insert("auth".to_owned(), "secret".to_owned());
        assert_eq!(
            authorize_vm_log_delete(true, "secret", &req),
            Err("auth failed"),
        );
    }

    #[test]
    fn delete_accepts_dedicated_header_or_bearer() {
        let header = request_with_header("x-vm-log-delete-auth", "secret");
        assert_eq!(authorize_vm_log_delete(true, "secret", &header), Ok(()));

        let bearer = request_with_header("authorization", "Bearer secret");
        assert_eq!(authorize_vm_log_delete(true, "secret", &bearer), Ok(()));
    }

    #[test]
    fn delete_rejects_wrong_or_empty_auth() {
        let wrong = request_with_header("x-vm-log-delete-auth", "wrong");
        assert_eq!(
            authorize_vm_log_delete(true, "secret", &wrong),
            Err("auth failed"),
        );
        let empty = request_with_header("x-vm-log-delete-auth", "   ");
        assert_eq!(
            authorize_vm_log_delete(true, "secret", &empty),
            Err("auth failed"),
        );
    }
}
