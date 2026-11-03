use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::net::{SocketAddr, ToSocketAddrs};

pub const DEFAULT_API_BASE: &str = "https://index-translate.bilibili.com/v1";
pub const DEFAULT_MODEL: &str = "Index-Translate-35B-A3B";
pub const DEFAULT_PORT: u16 = 18080;
pub const USER_AGENT: &str = "Index-Translate-Client/1.0 (rust)";

const FALLBACK_MODELS: [&str; 3] = [
    "Index-Translate-35B-A3B",
    "Index-Translate-9B",
    "Index-Translate-2B",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub api_base: String,
    pub model: String,
    pub host: String,
    pub port: u16,
    pub access_log: bool,
    pub max_body_bytes: usize,
}

impl Config {
    pub fn new(
        api_base: impl Into<String>,
        model: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        access_log: bool,
        max_body_bytes: usize,
    ) -> Result<Self> {
        let api_base = api_base.into().trim().trim_end_matches('/').to_owned();
        if !(api_base.starts_with("http://") || api_base.starts_with("https://")) {
            bail!("--api-base must start with http:// or https://");
        }

        let host = host.into().trim().to_owned();
        if host.is_empty() {
            bail!("--host must not be empty");
        }
        if port == 0 {
            bail!("--port must be between 1 and 65535");
        }
        if max_body_bytes == 0 {
            bail!("--max-body-mib must be at least 1");
        }

        Ok(Self {
            api_base,
            model: model.into(),
            host,
            port,
            access_log,
            max_body_bytes,
        })
    }

    /// Build the upstream URL, keeping any query string intact.
    pub fn upstream_url(&self, path: &str, query: Option<&str>) -> String {
        let mut url = format!("{}{}", self.api_base, path);
        if let Some(query) = query.filter(|query| !query.is_empty()) {
            url.push('?');
            url.push_str(query);
        }
        url
    }

    /// Address the proxy actually listens on (wildcards are preserved).
    pub fn bind_addr(&self) -> Result<SocketAddr> {
        resolve(&self.host, self.port)
    }

    /// Address a local client (or readiness probe) should connect to.
    pub fn connect_addr(&self) -> Result<SocketAddr> {
        let host = match self.host.as_str() {
            "0.0.0.0" => "127.0.0.1",
            "::" | "[::]" => "::1",
            other => other,
        };
        resolve(host, self.port)
    }

    pub fn display_host(&self) -> String {
        match self.host.as_str() {
            "0.0.0.0" => "127.0.0.1".to_owned(),
            "::" | "[::]" => "[::1]".to_owned(),
            host if host.contains(':') && !host.starts_with('[') => format!("[{host}]"),
            host => host.to_owned(),
        }
    }

    /// Base URL to configure in Immersive Translate.
    pub fn endpoint(&self) -> String {
        format!("http://{}:{}/v1", self.display_host(), self.port)
    }

    pub fn is_loopback(&self) -> bool {
        let host = self.host.trim_matches(|c| c == '[' || c == ']');
        host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost")
    }

    /// Model catalogue used when the upstream `/models` endpoint is unavailable.
    pub fn fallback_models(&self) -> Value {
        let mut ids: Vec<&str> = FALLBACK_MODELS.to_vec();
        if !ids.contains(&self.model.as_str()) {
            ids.insert(0, self.model.as_str());
        }
        let data: Vec<Value> = ids
            .into_iter()
            .map(|id| {
                json!({
                    "id": id,
                    "object": "model",
                    "created": 1_727_712_000u64,
                    "owned_by": "index-translate",
                })
            })
            .collect();
        json!({ "object": "list", "data": data })
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve host {host:?}"))?
        .next()
        .with_context(|| format!("host {host:?} resolved to no addresses"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(api_base: &str, host: &str) -> Config {
        Config::new(
            api_base,
            DEFAULT_MODEL,
            host,
            DEFAULT_PORT,
            false,
            64 * 1024 * 1024,
        )
        .unwrap()
    }

    #[test]
    fn trims_trailing_slash_and_appends_query() {
        let cfg = config("https://example.com/v1/", "127.0.0.1");
        assert_eq!(
            cfg.upstream_url("/chat/completions", Some("a=1")),
            "https://example.com/v1/chat/completions?a=1"
        );
        assert_eq!(
            cfg.upstream_url("/models", Some("")),
            "https://example.com/v1/models"
        );
    }

    #[test]
    fn wildcard_binds_are_mapped_for_clients() {
        let cfg = config(DEFAULT_API_BASE, "0.0.0.0");
        assert_eq!(cfg.endpoint(), "http://127.0.0.1:18080/v1");
        assert_eq!(cfg.connect_addr().unwrap().ip().to_string(), "127.0.0.1");
        assert_eq!(cfg.bind_addr().unwrap().ip().to_string(), "0.0.0.0");
    }

    #[test]
    fn fallback_models_include_custom_default() {
        let mut cfg = config(DEFAULT_API_BASE, "127.0.0.1");
        cfg.model = "custom-model".to_owned();
        let models = cfg.fallback_models();
        assert_eq!(models["data"][0]["id"], "custom-model");
    }
}
