fn query_metrics(_ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let format = q_string(&req, "format", "json");
    let lines = _ctx.hnoder.pqc_metrics_prometheus();
    if format == "prometheus" {
        let body = if lines.is_empty() {
            "# hacash pqc metrics unavailable\n".to_owned()
        } else {
            let mut out = String::new();
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
            out
        };
        return ApiResponse {
            status: 200,
            headers: vec![(
                "content-type".to_owned(),
                "text/plain; version=0.0.4; charset=utf-8".to_owned(),
            )],
            body: body.into_bytes(),
        };
    }

    let mut data = serde_json::Map::new();
    data.insert("ret".to_owned(), json!(0));
    if lines.is_empty() {
        data.insert("available".to_owned(), json!(false));
        return ApiResponse::json(Value::Object(data).to_string());
    }
    data.insert("available".to_owned(), json!(true));
    for line in lines {
        if let Some((name, value)) = line.split_once(' ') {
            if let Ok(v) = value.parse::<f64>() {
                data.insert(name.to_owned(), json!(v));
            } else {
                data.insert(name.to_owned(), json!(value));
            }
        }
    }
    ApiResponse::json(Value::Object(data).to_string())
}