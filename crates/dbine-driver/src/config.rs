use crate::info::{Field, TlsLevel, TlsRole};
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
        self.options
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn username_or_empty(&self) -> &str {
        self.username.as_deref().unwrap_or("")
    }

    pub fn password_or_empty(&self) -> &str {
        self.password.as_deref().unwrap_or("")
    }

    /// Where this connection's legacy `(encrypt, trust_server_certificate)`
    /// falls in a TLS field's legacy table: 0..=3 for `[ff, ft, tf, tt]`.
    pub fn legacy_tls_index(&self) -> usize {
        usize::from(self.encrypt) * 2 + usize::from(self.trust_server_certificate)
    }

    /// The value of a TLS option (a field with a [`crate::TlsRole`]): the
    /// saved one when the connection has it, else what a connection saved
    /// before the field existed behaves as (its legacy table, by
    /// [`Self::legacy_tls_index`]). `None`: neither, the driver keeps its
    /// legacy code path.
    pub fn tls_value<'a>(&'a self, f: &'a Field) -> Option<&'a str> {
        if let Some(v) = self.option(f.key) {
            return Some(v);
        }
        f.legacy
            .as_ref()
            .map(|l| l.0[self.legacy_tls_index()])
            .filter(|v| !v.is_empty())
    }

    /// The name the server's certificate must carry: the real server's when
    /// the connection goes through an SSH tunnel (the driver dials
    /// 127.0.0.1, see `src-tauri/src/tunnels.rs`), else the host as typed.
    pub fn tls_server_name(&self) -> &str {
        self.option(TUNNEL_ORIGIN_HOST)
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| self.host.trim())
    }

    /// The connection's TLS level by the driver's fields: its
    /// [`crate::TlsRole::Mode`] value, lowered by a skip-verify or
    /// skip-hostname option that is on. `None` when the driver has no mode
    /// field or the connection runs the driver's legacy path.
    pub fn tls_level(&self, fields: &[Field]) -> Option<TlsLevel> {
        let mut level = fields
            .iter()
            .find(|f| matches!(f.tls, Some(TlsRole::Mode { .. })))
            .and_then(|f| f.tls_level_of(self.tls_value(f)?))?;
        if level == TlsLevel::Off || level == TlsLevel::Optional {
            return Some(level);
        }
        for f in fields {
            let skips = self.tls_value(f).is_some_and(|v| f.tls_skips(v));
            match f.tls {
                Some(TlsRole::SkipVerify { .. }) if skips => {
                    level = level.min(TlsLevel::Unverified)
                }
                Some(TlsRole::SkipHostname) if skips => level = level.min(TlsLevel::VerifyCa),
                _ => {}
            }
        }
        Some(level)
    }
}

/// The option the app sets on a connection routed through an SSH tunnel:
/// the real server's host (the driver gets 127.0.0.1 as `host`).
pub const TUNNEL_ORIGIN_HOST: &str = "tunnel.origin_host";
/// The real server's port, next to [`TUNNEL_ORIGIN_HOST`].
pub const TUNNEL_ORIGIN_PORT: &str = "tunnel.origin_port";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldKind;

    fn cfg(encrypt: bool, trust: bool, options: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            driver: "x".into(),
            host: " db.example.com ".into(),
            encrypt,
            trust_server_certificate: trust,
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn sslmode() -> Field {
        Field::tls_mode(
            "sslmode",
            "sslmode",
            &[
                ("disable", "disable", TlsLevel::Off),
                ("prefer", "prefer", TlsLevel::Optional),
                ("require", "require", TlsLevel::Unverified),
                ("verify-ca", "verify-ca", TlsLevel::VerifyCa),
                ("verify-full", "verify-full", TlsLevel::VerifyFull),
            ],
            "verify-full",
            ["prefer", "require", "verify-full", "require"],
        )
    }

    #[test]
    fn legacy_index_follows_the_two_bools() {
        assert_eq!(cfg(false, false, &[]).legacy_tls_index(), 0);
        assert_eq!(cfg(false, true, &[]).legacy_tls_index(), 1);
        assert_eq!(cfg(true, false, &[]).legacy_tls_index(), 2);
        assert_eq!(cfg(true, true, &[]).legacy_tls_index(), 3);
    }

    #[test]
    fn tls_value_is_the_saved_option_else_the_legacy_table() {
        let f = sslmode();
        // A saved value wins whatever the bools say.
        assert_eq!(
            cfg(true, true, &[("sslmode", "disable")]).tls_value(&f),
            Some("disable")
        );
        // Absent: the legacy value for the bools.
        assert_eq!(cfg(false, false, &[]).tls_value(&f), Some("prefer"));
        assert_eq!(cfg(false, true, &[]).tls_value(&f), Some("require"));
        assert_eq!(cfg(true, false, &[]).tls_value(&f), Some("verify-full"));
        assert_eq!(cfg(true, true, &[]).tls_value(&f), Some("require"));
        // An empty saved value counts as absent.
        assert_eq!(
            cfg(true, false, &[("sslmode", "")]).tls_value(&f),
            Some("verify-full")
        );
        // An empty legacy entry: the driver's legacy path.
        let g = Field::tls_switch(
            "tls",
            "tls",
            TlsLevel::VerifyFull,
            TlsLevel::Off,
            true,
            ["", "", "true", "true"],
        );
        assert_eq!(cfg(false, false, &[]).tls_value(&g), None);
        assert_eq!(cfg(true, true, &[]).tls_value(&g), Some("true"));
        // A field without a legacy table: only the saved value.
        let ca = Field::tls_ca_file("sslrootcert", "sslrootcert");
        assert_eq!(cfg(true, false, &[]).tls_value(&ca), None);
        assert_eq!(
            cfg(true, false, &[("sslrootcert", "/ca.pem")]).tls_value(&ca),
            Some("/ca.pem")
        );
    }

    #[test]
    fn server_name_is_the_tunnel_origin_when_tunnelled() {
        assert_eq!(cfg(true, false, &[]).tls_server_name(), "db.example.com");
        let mut c = cfg(true, false, &[(TUNNEL_ORIGIN_HOST, " real.example.com ")]);
        c.host = "127.0.0.1".into();
        assert_eq!(c.tls_server_name(), "real.example.com");
        c.options.insert(TUNNEL_ORIGIN_HOST.into(), " ".into());
        assert_eq!(c.tls_server_name(), "127.0.0.1");
    }

    #[test]
    fn level_comes_from_the_mode_lowered_by_skip_options() {
        let fields = vec![
            Field::host(),
            sslmode(),
            Field::tls_skip_verify(
                "TrustServerCertificate",
                "TrustServerCertificate",
                false,
                ["false", "true", "false", "true"],
            ),
            Field::new("tlsAllowInvalidHostnames", "x", FieldKind::Bool).tls(TlsRole::SkipHostname),
        ];
        let level = |c: ConnectionConfig| c.tls_level(&fields);
        assert_eq!(
            level(cfg(false, false, &[("sslmode", "verify-full")])),
            Some(TlsLevel::VerifyFull)
        );
        // Legacy tf: verify-full, and TrustServerCertificate legacy false.
        assert_eq!(level(cfg(true, false, &[])), Some(TlsLevel::VerifyFull));
        // Legacy tt: require and trust → unverified.
        assert_eq!(level(cfg(true, true, &[])), Some(TlsLevel::Unverified));
        assert_eq!(level(cfg(false, false, &[])), Some(TlsLevel::Optional));
        assert_eq!(
            level(cfg(
                false,
                false,
                &[
                    ("sslmode", "verify-full"),
                    ("TrustServerCertificate", "true")
                ]
            )),
            Some(TlsLevel::Unverified)
        );
        assert_eq!(
            level(cfg(
                false,
                false,
                &[
                    ("sslmode", "verify-full"),
                    ("tlsAllowInvalidHostnames", "true")
                ]
            )),
            Some(TlsLevel::VerifyCa)
        );
        // Skipping doesn't raise a weaker mode.
        assert_eq!(
            level(cfg(
                false,
                false,
                &[("sslmode", "disable"), ("TrustServerCertificate", "true")]
            )),
            Some(TlsLevel::Off)
        );
        // An unknown value, or no mode field: no level.
        assert_eq!(level(cfg(false, false, &[("sslmode", "bogus")])), None);
        assert_eq!(cfg(true, false, &[]).tls_level(&Field::server_set()), None);
        // An inverted skip (validateCertificate=false skips).
        let inv = vec![
            Field::tls_switch(
                "ssl",
                "ssl",
                TlsLevel::VerifyFull,
                TlsLevel::Off,
                true,
                ["false", "false", "true", "true"],
            ),
            Field::tls_skip_verify(
                "validateCertificate",
                "validateCertificate",
                true,
                ["true", "false", "true", "false"],
            ),
        ];
        assert_eq!(
            cfg(true, true, &[]).tls_level(&inv),
            Some(TlsLevel::Unverified)
        );
        assert_eq!(
            cfg(true, false, &[]).tls_level(&inv),
            Some(TlsLevel::VerifyFull)
        );
    }

    #[test]
    fn levels_project_to_the_legacy_pair() {
        assert_eq!(TlsLevel::Off.legacy_pair(), (false, false));
        assert_eq!(TlsLevel::Optional.legacy_pair(), (false, false));
        assert_eq!(TlsLevel::Unverified.legacy_pair(), (true, true));
        assert_eq!(TlsLevel::VerifyCa.legacy_pair(), (true, false));
        assert_eq!(TlsLevel::VerifyFull.legacy_pair(), (true, false));
        assert!(TlsLevel::VerifyCa.verified() && !TlsLevel::Unverified.verified());
    }
}
