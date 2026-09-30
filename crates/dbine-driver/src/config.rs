use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How to reach a server. The typed fields cover what most engines share;
/// anything engine-specific (region, project, auth mode, API key…) goes in
/// `options` under the key its [`crate::Field`] declares. Secret values
/// (password, secret options) are never persisted with the rest: they live
/// in the OS keychain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// [`crate::DriverInfo::id`].
    pub driver: String,
    /// Server host, a URL, or a file path for embedded engines.
    #[serde(default)]
    pub host: String,
    /// 0 = the driver's default.
    #[serde(default)]
    pub port: u16,
    /// Database to connect to by default; empty = the server's default.
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub encrypt: bool,
    #[serde(default)]
    pub trust_server_certificate: bool,
    /// Block every statement that isn't a read (see [`crate::read_only`]).
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl ConnectionConfig {
    pub fn port_or(&self, default: u16) -> u16 {
        if self.port == 0 {
            default
        } else {
            self.port
        }
    }

    /// A non-empty option value.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn username_or_empty(&self) -> &str {
        self.username.as_deref().unwrap_or("")
    }

    pub fn password_or_empty(&self) -> &str {
        self.password.as_deref().unwrap_or("")
    }
}
