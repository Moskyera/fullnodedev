#[allow(unused)]
#[derive(Clone)]
pub struct HttpServer {
    cnf: ServerConf,
    engine: Arc<dyn Engine>,
    hcshnd: Arc<dyn HNoder>,
    router: Arc<Mutex<Option<Router>>>,
}

impl Server for HttpServer {
    fn start(&self, worker: Worker) {
        self.do_start(worker)
    }
}

impl HttpServer {
    pub fn open(iniobj: &IniObj, hnd: Arc<dyn HNoder>, router: Router) -> Self {
        let cnf = ServerConf::new(iniobj);
        Self {
            cnf: cnf,
            engine: hnd.engine(),
            hcshnd: hnd,
            router: Mutex::new(Some(router)).into(),
        }
    }

    fn do_start(&self, worker: Worker) {
        if !self.cnf.enable {
            return; // disable
        }
        let rt = new_tokio_rt(self.cnf.multi_thread);
        // server listen loop
        rt.block_on(async move { server_listen(self, worker).await });
    }
}

fn request_api_token(req: &axum::extract::Request) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get("x-api-token")
        .and_then(|v| v.to_str().ok())
    {
        let token = v.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    if let Some(v) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some((scheme, token)) = v.split_once(' ') {
            let token = token.trim();
            if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod api_token_tests {
    use super::*;

    #[test]
    fn token_is_accepted_from_headers_only() {
        let custom_header = axum::http::Request::builder()
            .header("x-api-token", "secret")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(request_api_token(&custom_header).as_deref(), Some("secret"));

        let bearer_header = axum::http::Request::builder()
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(request_api_token(&bearer_header).as_deref(), Some("secret"));
    }

    #[test]
    fn token_in_query_string_is_rejected() {
        let request = axum::http::Request::builder()
            .uri("/query/latest?api_token=secret")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(request_api_token(&request).is_none());
    }
}

async fn require_api_token(
    axum::extract::State(expected): axum::extract::State<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    match request_api_token(&req) {
        Some(provided) if provided == expected => Ok(next.run(req).await),
        _ => Err(axum::http::StatusCode::UNAUTHORIZED),
    }
}

async fn server_listen(ser: &HttpServer, worker: Worker) {
    let addr = match ser.cnf.socket_addr() {
        Ok(a) => a,
        Err(e) => {
            println!("\n[Error] api server: {e}\n");
            return;
        }
    };

    // Non-loopback binds must require a token so the API is never open to the LAN unauthenticated.
    if !addr.ip().is_loopback() && ser.cnf.api_token.is_empty() {
        println!(
            "\n[Error] api server bind {} is not loopback but [server] api_token is empty.\n\
             Set api_token in hacash.config.ini, or use bind = 127.0.0.1\n",
            addr.ip()
        );
        return;
    }

    let listener = TcpListener::bind(addr).await;
    if let Err(ref e) = listener {
        println!("\n[Error] api server failed to bind {}: {}\n", addr, e);
        return;
    }
    let listener = listener.unwrap();
    if ser.cnf.api_token.is_empty() {
        println!("[Api Server] listening on http://{addr} (loopback, no token)");
    } else {
        println!("[Api Server] listening on http://{addr} (api_token required)");
    }

    let mut rtapp = ser.router.lock().unwrap().take().unwrap();
    if !ser.cnf.api_token.is_empty() {
        rtapp = rtapp.layer(axum::middleware::from_fn_with_state(
            ser.cnf.api_token.clone(),
            require_api_token,
        ));
    }
    let mut wkr = worker.clone();
    if let Err(e) = axum::serve(listener, rtapp)
        .with_graceful_shutdown(async move {
            let _ = wkr.wait().await;
        })
        .await
    {
        println!("{e}");
    }
    println!("[Server] serve exit.");
}
