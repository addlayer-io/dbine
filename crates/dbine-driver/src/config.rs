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
    /// [`crate::TlsRole::Mode`] value, capped by a verification select
    /// ([`crate::TlsRole::Verify`]: the level its value allows) and by a
    /// skip-verify or skip-hostname option that is on. A cap never raises
    /// the mode (`SSL=false` with `SSLVerification=FULL` is off). `None` when
    /// the driver has no mode field, the connection runs the driver's legacy
    /// path, or the mode or the verification select holds a value that isn't
    /// one of its own.
    pub fn tls_level(&self, fields: &[Field]) -> Option<TlsLevel> {
        let mut level = fields
            .iter()
            .find(|f| matches!(f.tls, Some(TlsRole::Mode { .. })))
            .and_then(|f| f.tls_level_of(self.tls_value(f)?))?;
        if level == TlsLevel::Off || level == TlsLevel::Optional {
            return Some(level);
        }
        for f in fields {
            let Some(v) = self.tls_value(f) else {
                continue;
            };
            match f.tls {
                Some(TlsRole::SkipVerify { .. }) if f.tls_skips(v) => {
                    level = level.min(TlsLevel::Unverified)
                }
                Some(TlsRole::SkipHostname { .. }) if f.tls_skips(v) => {
                    level = level.min(TlsLevel::VerifyCa)
                }
                Some(TlsRole::Verify { .. }) => level = level.min(f.tls_level_of(v)?),
                _ => {}
            }
        }
        Some(level)
    }

    /// The CA file ([`crate::TlsRole::CaFile`]) that this connection's level
    /// can't verify without (its `required_at`) and that is empty.
    pub fn tls_ca_missing<'f>(&self, fields: &'f [Field]) -> Option<&'f Field> {
        let level = self.tls_level(fields)?;
        fields
            .iter()
            .find(|f| f.tls_ca_required_at(level) && self.tls_value(f).is_none())
    }

    /// Refuses a verified mode that needs a CA file it doesn't have
    /// ([`Self::tls_ca_missing`]), with a message that names the option that
    /// set the mode and the CA option, instead of connecting unverified.
    /// Drivers whose CA file has a `required_at` call it before connecting.
    pub fn require_tls_ca(&self, fields: &[Field]) -> crate::Result<()> {
        let Some(ca) = self.tls_ca_missing(fields) else {
            return Ok(());
        };
        let level = self.tls_level(fields);
        // The option that asked for this level: the verification select when
        // it's the one that set it, else the mode.
        let by = fields
            .iter()
            .filter(|f| matches!(f.tls, Some(TlsRole::Verify { .. })))
            .chain(
                fields
                    .iter()
                    .filter(|f| matches!(f.tls, Some(TlsRole::Mode { .. }))),
            )
            .find_map(|f| {
                let v = self.tls_value(f)?;
                (f.tls_level_of(v) == level).then_some((f.key, v))
            });
        let message = match by {
            Some((key, value)) => format!(
                "{key}={value} necesita el archivo de la CA: completá {} o elegí otro valor de {key}.",
                ca.label
            ),
            None => format!(
                "Este modo de TLS necesita el archivo de la CA: completá {}.",
                ca.label
            ),
        };
        Err(crate::Error::connect_failed(crate::ConnectFailure::new(
            crate::ConnectFailureKind::TlsUntrusted,
            message,
        )))
    }
}

/// The option the app sets on a connection routed through an SSH tunnel:
/// the real server's host (the driver gets 127.0.0.1 as `host`).
pub const TUNNEL_ORIGIN_HOST: &str = "tunnel.origin_host";
/// The real server's port, next to [`TUNNEL_ORIGIN_HOST`].
pub const TUNNEL_ORIGIN_PORT: &str = "tunnel.origin_port";

/// The SHA-256 of the server certificate (DER, in hex) that a mode which
/// doesn't verify the chain (`TlsLevel::Unverified`) trusts: saved by the
/// app from the first connection (`Session::tls_pin_seen`), checked on every
/// later one. Not a secret; it is stored with the connection's options.
pub const TLS_PIN: &str = "tls_pin";

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
            Field::new("tlsAllowInvalidHostnames", "x", FieldKind::Bool)
                .tls(TlsRole::SkipHostname { inverted: false }),
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

    /// Trino: the `SSL` switch, `SSLVerification` and the trust store.
    fn trino() -> Vec<Field> {
        vec![
            Field::tls_switch(
                "SSL",
                "SSL",
                TlsLevel::VerifyFull,
                TlsLevel::Off,
                true,
                ["false", "false", "true", "true"],
            ),
            Field::tls_verify(
                "SSLVerification",
                "SSLVerification",
                &[
                    ("FULL", "FULL", TlsLevel::VerifyFull),
                    ("CA", "CA", TlsLevel::VerifyCa),
                    ("NONE", "NONE", TlsLevel::Unverified),
                ],
                "FULL",
                ["FULL", "NONE", "FULL", "NONE"],
            ),
            Field::tls_ca_file("SSLTrustStorePath", "SSLTrustStorePath"),
        ]
    }

    #[test]
    fn a_verification_select_caps_the_mode() {
        let f = trino();
        let level = |opts: &[(&str, &str)]| cfg(false, false, opts).tls_level(&f);
        assert_eq!(
            level(&[("SSL", "true"), ("SSLVerification", "FULL")]),
            Some(TlsLevel::VerifyFull)
        );
        assert_eq!(
            level(&[("SSL", "true"), ("SSLVerification", "CA")]),
            Some(TlsLevel::VerifyCa)
        );
        assert_eq!(
            level(&[("SSL", "true"), ("SSLVerification", "NONE")]),
            Some(TlsLevel::Unverified)
        );
        // It never raises the mode.
        assert_eq!(
            level(&[("SSL", "false"), ("SSLVerification", "FULL")]),
            Some(TlsLevel::Off)
        );
        // Not set and no legacy value: no cap.
        assert_eq!(level(&[("SSL", "true")]), Some(TlsLevel::VerifyFull));
        // A value that isn't one of its own: no level, like the mode's.
        assert_eq!(
            level(&[("SSL", "true"), ("SSLVerification", "bogus")]),
            None
        );
        // The legacy pairs: ff, ft off; tf verified; tt unverified.
        let legacy = |e, t| cfg(e, t, &[]).tls_level(&f);
        assert_eq!(legacy(false, false), Some(TlsLevel::Off));
        assert_eq!(legacy(false, true), Some(TlsLevel::Off));
        assert_eq!(legacy(true, false), Some(TlsLevel::VerifyFull));
        assert_eq!(legacy(true, true), Some(TlsLevel::Unverified));
        // With a skip option too: the lowest cap wins.
        let mut g = f.clone();
        g.push(Field::tls_insecure(["false"; 4]));
        assert_eq!(
            cfg(
                false,
                false,
                &[
                    ("SSL", "true"),
                    ("SSLVerification", "CA"),
                    ("insecure", "true")
                ]
            )
            .tls_level(&g),
            Some(TlsLevel::Unverified)
        );
    }

    #[test]
    fn an_inverted_skip_hostname_skips_when_false() {
        let fields = vec![
            Field::tls_switch(
                "use_ssl",
                "use_ssl",
                TlsLevel::VerifyFull,
                TlsLevel::Off,
                true,
                ["false", "false", "true", "true"],
            ),
            Field::tls_skip_hostname(
                "ssl_assert_hostname",
                "ssl_assert_hostname",
                true,
                ["true"; 4],
            ),
        ];
        let level = |opts: &[(&str, &str)]| cfg(false, false, opts).tls_level(&fields);
        assert_eq!(
            level(&[("use_ssl", "true"), ("ssl_assert_hostname", "false")]),
            Some(TlsLevel::VerifyCa)
        );
        assert_eq!(
            level(&[("use_ssl", "true"), ("ssl_assert_hostname", "true")]),
            Some(TlsLevel::VerifyFull)
        );
        // The legacy table keeps the host-name check.
        assert_eq!(
            cfg(true, false, &[]).tls_level(&fields),
            Some(TlsLevel::VerifyFull)
        );
    }

    /// MySQL-like: `VERIFY_CA` can't verify without `ssl-ca`,
    /// `VERIFY_IDENTITY` uses the system roots.
    fn ca_required() -> Vec<Field> {
        vec![
            Field::tls_mode(
                "ssl-mode",
                "ssl-mode",
                &[
                    ("DISABLED", "DISABLED", TlsLevel::Off),
                    ("REQUIRED", "REQUIRED", TlsLevel::Unverified),
                    ("VERIFY_CA", "VERIFY_CA", TlsLevel::VerifyCa),
                    ("VERIFY_IDENTITY", "VERIFY_IDENTITY", TlsLevel::VerifyFull),
                ],
                "VERIFY_IDENTITY",
                ["DISABLED", "DISABLED", "VERIFY_IDENTITY", "REQUIRED"],
            ),
            Field::tls_ca_file("ca_cert", "ssl-ca").ca_required_at(&[TlsLevel::VerifyCa]),
        ]
    }

    #[test]
    fn a_verified_mode_that_needs_a_ca_refuses_without_it() {
        let f = ca_required();
        let c = cfg(false, false, &[("ssl-mode", "VERIFY_CA")]);
        assert_eq!(c.tls_ca_missing(&f).map(|x| x.key), Some("ca_cert"));
        let e = c.require_tls_ca(&f).unwrap_err();
        let failure = e.connect_failure().unwrap();
        assert_eq!(failure.kind, crate::ConnectFailureKind::TlsUntrusted);
        assert!(
            failure.message.contains("ssl-mode=VERIFY_CA") && failure.message.contains("ssl-ca"),
            "{}",
            failure.message
        );
        // With the CA, or in a mode that doesn't need it: fine.
        let with = cfg(
            false,
            false,
            &[("ssl-mode", "VERIFY_CA"), ("ca_cert", "/ca.pem")],
        );
        assert!(with.require_tls_ca(&f).is_ok());
        for mode in ["VERIFY_IDENTITY", "REQUIRED", "DISABLED"] {
            assert!(cfg(false, false, &[("ssl-mode", mode)])
                .require_tls_ca(&f)
                .is_ok());
        }
        // The legacy path and a driver without a mode: nothing to check.
        assert!(cfg(true, false, &[]).require_tls_ca(&f).is_ok());
        assert!(cfg(true, false, &[])
            .require_tls_ca(&Field::server_set())
            .is_ok());
    }

    #[test]
    fn the_refusal_names_the_verification_select_that_set_the_level() {
        let mut f = trino();
        f[2] = Field::tls_ca_file("SSLTrustStorePath", "SSLTrustStorePath")
            .ca_required_at(&[TlsLevel::VerifyCa]);
        let c = cfg(false, false, &[("SSL", "true"), ("SSLVerification", "CA")]);
        let msg = c.require_tls_ca(&f).unwrap_err().to_string();
        assert!(
            msg.starts_with("SSLVerification=CA ") && msg.contains("SSLTrustStorePath"),
            "{msg}"
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
