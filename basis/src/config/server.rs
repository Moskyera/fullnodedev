#[derive(Clone)]
pub struct ServerConf {
    pub enable: bool,
    pub listen: u16,
    /// Interface to bind. Default `127.0.0.1` (not world-reachable).
    pub bind: String,
    /// When non-empty, HTTP API requires X-Api-Token or Authorization: Bearer.
    pub api_token: String,
    pub multi_thread: bool,
    pub debug_open: bool,
}

#[cfg(test)]
mod server_conf_tests {
    use super::*;

    #[test]
    fn debug_open_defaults_to_false() {
        let ini = IniObj::new();
        let cnf = ServerConf::new(&ini);
        assert!(!cnf.debug_open);
    }

    #[test]
    fn debug_open_reads_server_section() {
        let mut ini = IniObj::new();
        let mut server = std::collections::HashMap::new();
        server.insert("debug_open".to_owned(), Some("true".to_owned()));
        ini.insert("server".to_owned(), server);

        let cnf = ServerConf::new(&ini);
        assert!(cnf.debug_open);
    }

    #[test]
    fn bind_defaults_to_loopback() {
        let ini = IniObj::new();
        let cnf = ServerConf::new(&ini);
        assert_eq!(cnf.bind, "127.0.0.1");
        assert!(cnf.api_token.is_empty());
    }

    #[test]
    fn bind_and_token_from_ini() {
        let mut ini = IniObj::new();
        let mut server = std::collections::HashMap::new();
        server.insert("bind".to_owned(), Some("0.0.0.0".to_owned()));
        server.insert("api_token".to_owned(), Some("secret".to_owned()));
        ini.insert("server".to_owned(), server);
        let cnf = ServerConf::new(&ini);
        assert_eq!(cnf.bind, "0.0.0.0");
        assert_eq!(cnf.api_token, "secret");
    }

    #[test]
    fn invalid_bind_fails_closed() {
        let mut ini = IniObj::new();
        let mut server = std::collections::HashMap::new();
        server.insert("bind".to_owned(), Some("not-an-ip-address".to_owned()));
        ini.insert("server".to_owned(), server);
        let cnf = ServerConf::new(&ini);
        assert!(cnf.socket_addr().is_err());
    }
}

impl ServerConf {
    pub fn new(ini: &IniObj) -> ServerConf {
        let sec = ini_section(ini, "server");
        ServerConf {
            enable: ini_must_bool(&sec, "enable", false),
            listen: ini_must_u64(&sec, "listen", 8083) as u16,
            bind: {
                let raw = ini_must(&sec, "bind", "127.0.0.1");
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    "127.0.0.1".to_string()
                } else {
                    trimmed.to_string()
                }
            },
            api_token: ini_must(&sec, "api_token", "").trim().to_string(),
            multi_thread: ini_must_bool(&sec, "multi_thread", false),
            debug_open: ini_must_bool(&sec, "debug_open", false),
        }
    }

    /// Parse bind + port into a socket address.
    pub fn socket_addr(&self) -> Result<std::net::SocketAddr, String> {
        let host = self.bind.trim();
        if host.is_empty() {
            return Ok(std::net::SocketAddr::from(([127, 0, 0, 1], self.listen)));
        }
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            return Ok(std::net::SocketAddr::new(ip, self.listen));
        }
        // host:port form (rare); prefer explicit bind + listen keys
        if let Ok(addr) = format!("{host}:{}", self.listen).parse::<std::net::SocketAddr>() {
            return Ok(addr);
        }
        Err(format!("invalid [server] bind address: {host}"))
    }

    pub fn is_loopback_bind(&self) -> bool {
        self.socket_addr()
            .map(|a| a.ip().is_loopback())
            .unwrap_or(false)
    }
}
