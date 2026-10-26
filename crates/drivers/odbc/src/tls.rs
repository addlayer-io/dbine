//! Each ODBC engine's own TLS options (docs/tls-options.md, "ODBC crate"):
//! the form fields, spelled as the vendor's ODBC driver documents them, and
//! the connection-string attributes they turn into.
//!
//! Saved connections predate these fields and never wrote a TLS keyword, so
//! every legacy table is empty ([`L0`]: nothing is written and the vendor
//! driver's own default and `odbc.ini` apply, as before). The exceptions
//! are the ones whose template always wrote it: Cloudera's `SSL=1` and
//! Informix / GBase 8s's `PROTOCOL=onsoctcp`.

use crate::presets::Preset;
use dbine_driver::{
    ConnectFailure, ConnectFailureKind, ConnectionConfig, Error, Field, Result, TlsLevel,
    TlsSupport, TUNNEL_ORIGIN_HOST,
};
use TlsLevel::{Off, Optional, Unverified, VerifyCa, VerifyFull};

/// No keyword for any legacy `(encrypt, trust_server_certificate)` pair.
const L0: [&str; 4] = ["", "", "", ""];

const CA_HELP: &str = "Opcional: la CA que firmó el certificado del servidor, en PEM.";
const DB2I_HELP: &str = "El certificado se verifica con el almacén de certificados del sistema.";
const TERADATA_HELP: &str =
    "Con TLS el driver usa el puerto HTTPS del servidor (HTTPS_PORT, 443), no el de TDMST.";
const VERTICA_HELP: &str = "verify-ca y verify-full necesitan el root.crt de la CA en el cliente.";
const INFORMIX_HELP: &str = "onsocssl verifica el certificado con el almacén de claves del cliente (SSL_KEYSTORE_FILE en conssl.cfg).";
const NETSUITE_TRUSTSTORE_HELP: &str =
    "«system» usa los certificados del sistema; en Linux, la ruta del archivo .cer de NetSuite.";

/// How the engine's connections are encrypted, and its TLS port when it
/// isn't the default one.
pub fn support(p: &Preset) -> (TlsSupport, Option<u16>) {
    match p.id {
        // The DSN or the typed string; IRIS / Caché read TLS from SSLDefs.ini.
        "odbc" | "iris" | "cache" => (TlsSupport::InUrl, None),
        "access" | "dbase" => (TlsSupport::NotApplicable, None),
        // No TLS over ODBC (docs/engine-support.md, "Connection encryption").
        "zen" | "mimer" | "nuodb" | "cubrid" => (TlsSupport::None, None),
        // Always `Encrypted=1`, verified against `Truststore`.
        "netsuite" => (TlsSupport::Forced, None),
        // The database host server's TLS port (as-database-s).
        "db2i" => (TlsSupport::Configurable, Some(9471)),
        // The server's SSL_PORT_NO in Altibase's documentation.
        "altibase" => (TlsSupport::Configurable, Some(20443)),
        _ => (TlsSupport::Configurable, None),
    }
}

/// The SSL tab of a preset. Empty for the ids without TLS fields (and for
/// the ones still in `NOT_YET_MIGRATED`).
pub fn fields(p: &Preset) -> Vec<Field> {
    match p.id {
        "db2" | "db2zos" => vec![
            Field::tls_switch("Security", "Security=SSL", VerifyFull, Off, true, L0),
            Field::tls_ca_file("SSLServerCertificate", "SSLServerCertificate").help(CA_HELP),
            Field::tls_skip_hostname(
                "SSLClientHostnameValidation",
                "SSLClientHostnameValidation=Basic",
                true,
                L0,
            ),
        ],
        // Whether the driver checks the host name isn't documented: the level
        // doesn't claim it.
        "db2i" => vec![Field::tls_switch("SSL", "SSL", VerifyCa, Off, true, L0).help(DB2I_HELP)],
        "hive" | "impala" | "spark" | "kyuubi" | "cloudera" => {
            let legacy = if p.id == "cloudera" { ["true"; 4] } else { L0 };
            vec![
                Field::tls_switch("SSL", "SSL", VerifyFull, Off, true, legacy),
                Field::tls_skip_verify(
                    "AllowSelfSignedServerCert",
                    "AllowSelfSignedServerCert",
                    false,
                    L0,
                ),
                Field::tls_skip_hostname(
                    "AllowHostNameCNMismatch",
                    "AllowHostNameCNMismatch",
                    false,
                    L0,
                ),
                Field::tls_ca_file("TrustedCerts", "TrustedCerts").help(CA_HELP),
            ]
        }
        "teradata" => vec![
            Field::tls_mode(
                "SSLMode",
                "SSLMode",
                &[
                    ("Disable", "Disable", Off),
                    ("Allow", "Allow", Optional),
                    ("Prefer", "Prefer", Optional),
                    ("Require", "Require", Unverified),
                    ("Verify-CA", "Verify-CA", VerifyCa),
                    ("Verify-Full", "Verify-Full", VerifyFull),
                ],
                "Verify-Full",
                L0,
            )
            .help(TERADATA_HELP),
            Field::tls_ca_file("SSLCA", "SSLCA").help(CA_HELP),
        ],
        "vertica" => vec![Field::tls_mode(
            "SSLMode",
            "SSLMode",
            &[
                ("disable", "disable", Off),
                ("prefer", "prefer", Optional),
                ("require", "require", Unverified),
                ("verify-ca", "verify-ca", VerifyCa),
                ("verify-full", "verify-full", VerifyFull),
            ],
            "verify-full",
            L0,
        )
        .help(VERTICA_HELP)],
        // The host-name check isn't documented for the ODBC driver: the
        // level stops at the chain.
        "exasol" => vec![
            Field::tls_switch("ENCRYPTION", "ENCRYPTION", VerifyCa, Off, true, L0),
            Field::tls_verify(
                "SSLCERTIFICATE",
                "SSLCERTIFICATE",
                &[
                    ("SSL_VERIFY_SERVER", "SSL_VERIFY_SERVER", VerifyCa),
                    ("SSL_VERIFY_NONE", "SSL_VERIFY_NONE", Unverified),
                ],
                "SSL_VERIFY_SERVER",
                L0,
            ),
        ],
        // Without CACertFile the driver doesn't authenticate the server.
        "netezza" => vec![
            Field::tls_mode(
                "SecurityLevel",
                "SecurityLevel",
                &[
                    ("onlyUnSecured", "onlyUnSecured", Off),
                    ("preferredUnSecured", "preferredUnSecured", Optional),
                    ("preferredSecured", "preferredSecured", Optional),
                    ("onlySecured", "onlySecured", VerifyCa),
                ],
                "onlySecured",
                L0,
            ),
            Field::tls_ca_file("CACertFile", "CACertFile").ca_required_at(&[VerifyCa]),
        ],
        "openedge" => vec![
            Field::tls_switch(
                "EncryptionMethod",
                "EncryptionMethod",
                VerifyFull,
                Off,
                true,
                L0,
            ),
            Field::tls_skip_verify(
                "ValidateServerCertificate",
                "ValidateServerCertificate",
                true,
                L0,
            ),
            Field::tls_ca_file("Truststore", "Truststore").help(CA_HELP),
            Field::tls_server_name("HostNameInCertificate", "HostNameInCertificate"),
        ],
        "sqlanywhere" => vec![
            Field::tls_mode(
                "Encryption",
                "Encryption",
                &[
                    ("NONE", "NONE", Off),
                    // Obfuscation, not encryption.
                    ("SIMPLE", "SIMPLE", Off),
                    ("TLS", "TLS", VerifyFull),
                ],
                "TLS",
                L0,
            ),
            Field::tls_ca_file("trusted_certificates", "trusted_certificates").help(CA_HELP),
            Field::tls_skip_hostname(
                "skip_certificate_name_check",
                "skip_certificate_name_check",
                false,
                L0,
            ),
            Field::tls_server_name("certificate_name", "certificate_name"),
        ],
        // The trust comes from the GSKit keystore (conssl.cfg); the host name
        // isn't checked.
        "informix" | "gbase8s" => vec![Field::tls_mode(
            "PROTOCOL",
            "PROTOCOL",
            &[
                ("onsoctcp", "onsoctcp", Off),
                ("onsocssl", "onsocssl", VerifyCa),
            ],
            "onsocssl",
            ["onsoctcp"; 4],
        )
        .help(INFORMIX_HELP)],
        // Both drivers verify the chain and never the host name.
        "ignite" | "ignite3" => vec![
            Field::tls_mode(
                "SSL_MODE",
                "SSL_MODE",
                &[
                    ("disable", "disable", Off),
                    ("require", "require", VerifyCa),
                ],
                "require",
                L0,
            ),
            Field::tls_ca_file("SSL_CA_FILE", "SSL_CA_FILE").help(CA_HELP),
        ],
        "monetdb" => vec![
            Field::tls_switch("TLS", "TLS", VerifyFull, Off, true, L0),
            Field::tls_ca_file("CERT", "CERT").help(CA_HELP),
        ],
        "ocient" => vec![Field::tls_mode(
            "TLS",
            "TLS",
            &[
                ("UNVERIFIED", "UNVERIFIED", Unverified),
                ("ON", "ON", VerifyCa),
            ],
            "ON",
            L0,
        )],
        "altibase" => vec![
            Field::tls_mode(
                "CONNTYPE",
                "CONNTYPE",
                &[("1", "1 (TCP)", Off), ("6", "6 (SSL)", VerifyCa)],
                "6",
                L0,
            ),
            Field::tls_skip_verify("SSL_VERIFY", "SSL_VERIFY", true, L0),
            Field::tls_ca_file("SSL_CA", "SSL_CA").help(CA_HELP),
        ],
        // The stored key stays `truststore`.
        "netsuite" => vec![Field::tls_ca_file("truststore", "Truststore")
            .placeholder("system")
            .help(NETSUITE_TRUSTSTORE_HELP)],
        _ => vec![],
    }
}

/// An attribute is in the connection string the user typed in "Atributos
/// adicionales" (case-insensitive, as in ODBC).
pub fn has_key(conn_str: &str, key: &str) -> bool {
    conn_str.split(';').any(|kv| {
        kv.split('=')
            .next()
            .is_some_and(|k| k.trim().eq_ignore_ascii_case(key))
    })
}

/// One TLS attribute: `raw` values go as they are (SQL Anywhere's
/// `TLS(…)`), the others are escaped like any other attribute.
pub struct Part {
    pub key: &'static str,
    pub value: String,
    pub raw: bool,
}

/// The TLS attributes of this connection, in the vendor's spelling. Fails
/// instead of connecting with less than the chosen level: a verified mode
/// that needs a CA file it doesn't have, or a host-name check that a driver
/// without a server-name override can't do through an SSH tunnel.
pub fn parts(p: &Preset, cfg: &ConnectionConfig, extra: &str) -> Result<Vec<Part>> {
    let fields = fields(p);
    if fields.is_empty() {
        return Ok(vec![]);
    }
    let v = |key: &str| -> Option<&str> {
        fields
            .iter()
            .find(|f| f.key == key)
            .and_then(|f| cfg.tls_value(f))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let on = |key: &str| v(key) == Some("true");
    let flag = |key: &str, yes: &'static str, no: &'static str| {
        v(key).map(move |x| if x == "true" { yes } else { no })
    };

    if p.id == "netezza" && !has_key(extra, "CACertFile") {
        cfg.require_tls_ca(&fields)?;
    }
    let level = cfg.tls_level(&fields);
    let tunnelled = cfg
        .option(TUNNEL_ORIGIN_HOST)
        .is_some_and(|h| !h.trim().is_empty());
    if tunnelled && level == Some(VerifyFull) {
        if let Some(fix) = tunnel_fix(p.id) {
            if !has_key(extra, fix.split('=').next().unwrap_or(fix)) {
                let msg = if p.id == "monetdb" {
                    format!(
                        "Por el túnel SSH, el driver ODBC de MonetDB no puede comprobar que el certificado sea de {}: fijalo con CERTHASH en los atributos adicionales o conectate sin túnel.",
                        cfg.tls_server_name()
                    )
                } else {
                    format!(
                        "Por el túnel SSH, el driver ODBC no puede comprobar que el certificado sea de {}: usá {} (verifica solo la CA) o conectate sin túnel.",
                        cfg.tls_server_name(),
                        fix
                    )
                };
                return Err(Error::connect_failed(ConnectFailure::new(
                    ConnectFailureKind::TlsUntrusted,
                    msg,
                )));
            }
        }
    }

    let mut out = Vec::new();
    match p.id {
        "db2" | "db2zos" => {
            if on("Security") {
                add(&mut out, "Security", "SSL");
                if let Some(ca) = v("SSLServerCertificate") {
                    add(&mut out, "SSLServerCertificate", ca);
                }
                if let Some(h) = flag("SSLClientHostnameValidation", "Basic", "OFF") {
                    add(&mut out, "SSLClientHostnameValidation", h);
                }
            }
        }
        "db2i" => {
            if let Some(s) = flag("SSL", "1", "0") {
                add(&mut out, "SSL", s);
            }
        }
        "hive" | "impala" | "spark" | "kyuubi" | "cloudera" => {
            if let Some(s) = flag("SSL", "1", "0") {
                add(&mut out, "SSL", s);
            }
            if on("SSL") {
                for key in ["AllowSelfSignedServerCert", "AllowHostNameCNMismatch"] {
                    if let Some(x) = flag(key, "1", "0") {
                        add(&mut out, key, x);
                    }
                }
                if let Some(ca) = v("TrustedCerts") {
                    add(&mut out, "TrustedCerts", ca);
                }
            }
        }
        "teradata" => {
            if let Some(m) = v("SSLMode") {
                add(&mut out, "SSLMode", m);
            }
            if let Some(ca) = v("SSLCA") {
                add(&mut out, "SSLCA", ca);
            }
        }
        "vertica" => {
            if let Some(m) = v("SSLMode") {
                add(&mut out, "SSLMode", m);
            }
        }
        "exasol" => {
            if let Some(e) = flag("ENCRYPTION", "Y", "N") {
                add(&mut out, "ENCRYPTION", e);
            }
            if on("ENCRYPTION") {
                if let Some(c) = v("SSLCERTIFICATE") {
                    add(&mut out, "SSLCERTIFICATE", c);
                }
            }
        }
        "netezza" => {
            if let Some(m) = v("SecurityLevel") {
                add(&mut out, "SecurityLevel", m);
            }
            if let Some(ca) = v("CACertFile") {
                add(&mut out, "CACertFile", ca);
            }
        }
        "openedge" => {
            if let Some(e) = flag("EncryptionMethod", "1", "0") {
                add(&mut out, "EncryptionMethod", e);
            }
            if on("EncryptionMethod") {
                if let Some(x) = flag("ValidateServerCertificate", "1", "0") {
                    add(&mut out, "ValidateServerCertificate", x);
                }
                if let Some(ts) = v("Truststore") {
                    add(&mut out, "Truststore", ts);
                }
                // The driver checks the host name only when it's given: the
                // real server's (through a tunnel too) unless one is typed.
                match v("HostNameInCertificate") {
                    Some(h) => add(&mut out, "HostNameInCertificate", h),
                    None if level == Some(VerifyFull) => {
                        add(&mut out, "HostNameInCertificate", cfg.tls_server_name())
                    }
                    None => {}
                }
            }
        }
        "sqlanywhere" => match v("Encryption") {
            Some("TLS") => {
                let mut clauses = Vec::new();
                if let Some(ca) = v("trusted_certificates") {
                    clauses.push(format!("trusted_certificates={ca}"));
                }
                if on("skip_certificate_name_check") {
                    clauses.push("skip_certificate_name_check=ON".into());
                } else if let Some(name) = v("certificate_name") {
                    clauses.push(format!("certificate_name={name}"));
                } else if tunnelled {
                    clauses.push(format!("certificate_name={}", cfg.tls_server_name()));
                }
                let value = if clauses.is_empty() {
                    "TLS".to_string()
                } else {
                    format!("TLS({})", clauses.join(";"))
                };
                if !has_key(extra, "ENC") {
                    out.push(Part {
                        key: "Encryption",
                        value,
                        raw: true,
                    });
                }
            }
            Some(m) => add(&mut out, "Encryption", m),
            None => {}
        },
        "informix" | "gbase8s" => {
            if let Some(m) = v("PROTOCOL") {
                add(&mut out, "PROTOCOL", m);
            }
        }
        "ignite" | "ignite3" => {
            if let Some(m) = v("SSL_MODE") {
                add(&mut out, "SSL_MODE", m);
                if m == "require" {
                    if let Some(ca) = v("SSL_CA_FILE") {
                        add(&mut out, "SSL_CA_FILE", ca);
                    }
                }
            }
        }
        "monetdb" => {
            if let Some(t) = flag("TLS", "On", "Off") {
                add(&mut out, "TLS", t);
            }
            if on("TLS") {
                if let Some(ca) = v("CERT") {
                    add(&mut out, "CERT", ca);
                }
            }
        }
        "ocient" => {
            if let Some(m) = v("TLS") {
                add(&mut out, "TLS", m);
            }
        }
        "altibase" => {
            if let Some(m) = v("CONNTYPE") {
                add(&mut out, "CONNTYPE", m);
                if m == "6" {
                    if let Some(x) = flag("SSL_VERIFY", "1", "0") {
                        add(&mut out, "SSL_VERIFY", x);
                    }
                    if let Some(ca) = v("SSL_CA") {
                        add(&mut out, "SSL_CA", ca);
                    }
                }
            }
        }
        // NetSuite's truststore is in its template.
        _ => {}
    }
    out.retain(|part| !has_key(extra, part.key));
    Ok(out)
}

fn add(out: &mut Vec<Part>, key: &'static str, value: &str) {
    out.push(Part {
        key,
        value: value.to_string(),
        raw: false,
    });
}

/// What turns the host-name check off for a driver that can't be told the
/// real server's name through an SSH tunnel (`None`: it can, or it doesn't
/// check the name).
fn tunnel_fix(id: &str) -> Option<&'static str> {
    match id {
        "db2" | "db2zos" => Some("SSLClientHostnameValidation=OFF"),
        "hive" | "impala" | "spark" | "kyuubi" | "cloudera" => Some("AllowHostNameCNMismatch=1"),
        "teradata" => Some("SSLMode=Verify-CA"),
        "vertica" => Some("SSLMode=verify-ca"),
        "monetdb" => Some("CERTHASH"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connstr::build;
    use crate::presets::PRESETS;
    use dbine_driver::FieldKind;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn cfg(encrypt: bool, trust: bool, opts: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            host: "db.example.com".into(),
            username: Some("app".into()),
            password: Some("pw".into()),
            encrypt,
            trust_server_certificate: trust,
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    /// What a new connection saves: each TLS field's default.
    fn new_conn(id: &str, extra: &[(&str, &str)]) -> ConnectionConfig {
        let mut opts: Vec<(String, String)> = fields(preset(id))
            .into_iter()
            .filter(|f| !f.default.is_empty())
            .map(|f| (f.key.to_string(), f.default.to_string()))
            .collect();
        opts.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let mut c = cfg(false, false, &[]);
        c.options = opts.into_iter().collect();
        c
    }

    fn conn_str(id: &str, c: &ConnectionConfig) -> String {
        build(preset(id), c, "db").unwrap()
    }

    const TLS_KEYS: &[&str] = &[
        "Security=",
        "SSLServerCertificate",
        "SSLClientHostnameValidation",
        "SSL=",
        "AllowSelfSignedServerCert",
        "AllowHostNameCNMismatch",
        "TrustedCerts",
        "SSLMode",
        "SSLCA",
        "ENCRYPTION",
        "SSLCERTIFICATE",
        "SecurityLevel",
        "CACertFile",
        "EncryptionMethod",
        "ValidateServerCertificate",
        "HostNameInCertificate",
        "Encryption=",
        "SSL_MODE",
        "SSL_CA_FILE",
        "TLS=",
        "CERT=",
        "CONNTYPE",
        "SSL_VERIFY",
        "SSL_CA=",
    ];

    /// The four legacy pairs build today's string: no TLS keyword, except
    /// Cloudera's `SSL=1` and Informix / GBase 8s's `PROTOCOL=onsoctcp`.
    #[test]
    fn saved_connections_build_todays_string() {
        for p in PRESETS {
            if p.is_generic() {
                continue;
            }
            for (e, t) in [(false, false), (false, true), (true, false), (true, true)] {
                let s = build(p, &cfg(e, t, &[]), "db").unwrap();
                let tls: Vec<&&str> = TLS_KEYS
                    .iter()
                    .filter(|k| s.split(';').any(|kv| kv.starts_with(**k)))
                    .collect();
                match p.id {
                    "cloudera" => {
                        assert!(s.contains(";SSL=1"), "{s}");
                        assert_eq!(tls, [&"SSL="], "{s}");
                    }
                    "informix" | "gbase8s" => {
                        assert!(s.contains(";PROTOCOL=onsoctcp"), "{s}");
                        assert!(tls.is_empty(), "{s}");
                    }
                    "netsuite" => {
                        assert!(s.contains(";Encrypted=1;"), "{s}");
                        assert!(s.contains(";Truststore=system;"), "{s}");
                    }
                    // Machbase's template writes its own CONNTYPE=1.
                    _ if fields(p).is_empty() => {}
                    _ => assert!(tls.is_empty(), "{}: {s}", p.id),
                }
                // Nothing to refuse either.
                assert_eq!(
                    cfg(e, t, &[]).tls_level(&fields(p)).map(|_| p.id),
                    match p.id {
                        "cloudera" | "informix" | "gbase8s" => Some(p.id),
                        _ => None,
                    }
                );
            }
        }
        // Cloudera's legacy SSL=1 verifies, as it did (the driver's defaults).
        assert_eq!(
            cfg(true, true, &[]).tls_level(&fields(preset("cloudera"))),
            Some(VerifyFull)
        );
    }

    /// A new connection is encrypted and verified, in each vendor's words.
    #[test]
    fn new_connections_verify() {
        let cases: &[(&str, &[&str], TlsLevel)] = &[
            (
                "db2",
                &["Security=SSL", "SSLClientHostnameValidation=Basic"],
                VerifyFull,
            ),
            (
                "db2zos",
                &["Security=SSL", "SSLClientHostnameValidation=Basic"],
                VerifyFull,
            ),
            ("db2i", &["SSL=1"], VerifyCa),
            (
                "hive",
                &[
                    "SSL=1",
                    "AllowSelfSignedServerCert=0",
                    "AllowHostNameCNMismatch=0",
                ],
                VerifyFull,
            ),
            (
                "impala",
                &[
                    "SSL=1",
                    "AllowSelfSignedServerCert=0",
                    "AllowHostNameCNMismatch=0",
                ],
                VerifyFull,
            ),
            (
                "spark",
                &[
                    "SSL=1",
                    "AllowSelfSignedServerCert=0",
                    "AllowHostNameCNMismatch=0",
                ],
                VerifyFull,
            ),
            (
                "kyuubi",
                &[
                    "SSL=1",
                    "AllowSelfSignedServerCert=0",
                    "AllowHostNameCNMismatch=0",
                ],
                VerifyFull,
            ),
            (
                "cloudera",
                &[
                    "SSL=1",
                    "AllowSelfSignedServerCert=0",
                    "AllowHostNameCNMismatch=0",
                ],
                VerifyFull,
            ),
            ("teradata", &["SSLMode=Verify-Full"], VerifyFull),
            ("vertica", &["SSLMode=verify-full"], VerifyFull),
            (
                "exasol",
                &["ENCRYPTION=Y", "SSLCERTIFICATE=SSL_VERIFY_SERVER"],
                VerifyCa,
            ),
            (
                "openedge",
                &[
                    "EncryptionMethod=1",
                    "ValidateServerCertificate=1",
                    "HostNameInCertificate=db.example.com",
                ],
                VerifyFull,
            ),
            ("sqlanywhere", &["Encryption=TLS"], VerifyFull),
            ("informix", &["PROTOCOL=onsocssl"], VerifyCa),
            ("gbase8s", &["PROTOCOL=onsocssl"], VerifyCa),
            ("ignite", &["SSL_MODE=require"], VerifyCa),
            ("ignite3", &["SSL_MODE=require"], VerifyCa),
            ("monetdb", &["TLS=On"], VerifyFull),
            ("ocient", &["TLS=ON"], VerifyCa),
            ("altibase", &["CONNTYPE=6", "SSL_VERIFY=1"], VerifyCa),
        ];
        for (id, attrs, level) in cases {
            let c = new_conn(id, &[]);
            assert_eq!(c.tls_level(&fields(preset(id))), Some(*level), "{id}");
            let s = conn_str(id, &c);
            for a in *attrs {
                assert!(s.split(';').any(|kv| kv == *a), "{id}: {a} in {s}");
            }
        }
        // Netezza verifies only with its CA file, and refuses without it.
        let e = build(preset("netezza"), &new_conn("netezza", &[]), "db").unwrap_err();
        assert!(e.to_string().contains("CACertFile"), "{e}");
        let s = conn_str(
            "netezza",
            &new_conn("netezza", &[("CACertFile", "/ca.pem")]),
        );
        assert!(
            s.ends_with(";SecurityLevel=onlySecured;CACertFile=/ca.pem"),
            "{s}"
        );
        // …unless the additional attributes carry it.
        let s = conn_str(
            "netezza",
            &new_conn("netezza", &[("extra", "CACertFile=/x.pem")]),
        );
        assert!(
            s.ends_with(";SecurityLevel=onlySecured;CACertFile=/x.pem"),
            "{s}"
        );
    }

    #[test]
    fn every_setting_turns_off() {
        let off = |id: &str, opts: &[(&str, &str)], want: &[&str], level: TlsLevel| {
            let c = new_conn(id, opts);
            assert_eq!(c.tls_level(&fields(preset(id))), Some(level), "{id}");
            let s = conn_str(id, &c);
            for a in want {
                assert!(s.split(';').any(|kv| kv == *a), "{id}: {a} in {s}");
            }
            s
        };
        let s = off("db2", &[("Security", "false")], &[], Off);
        assert!(!s.contains("Security") && !s.contains("SSLClient"), "{s}");
        off(
            "db2",
            &[
                ("SSLClientHostnameValidation", "false"),
                ("SSLServerCertificate", "/ca.arm"),
            ],
            &[
                "SSLClientHostnameValidation=OFF",
                "SSLServerCertificate=/ca.arm",
            ],
            VerifyCa,
        );
        off("db2i", &[("SSL", "false")], &["SSL=0"], Off);
        let s = off("hive", &[("SSL", "false")], &["SSL=0"], Off);
        assert!(!s.contains("AllowSelfSigned"), "{s}");
        off(
            "spark",
            &[("AllowSelfSignedServerCert", "true")],
            &["AllowSelfSignedServerCert=1"],
            Unverified,
        );
        off(
            "impala",
            &[
                ("AllowHostNameCNMismatch", "true"),
                ("TrustedCerts", "/ca.pem"),
            ],
            &["AllowHostNameCNMismatch=1", "TrustedCerts=/ca.pem"],
            VerifyCa,
        );
        off(
            "teradata",
            &[("SSLMode", "Disable")],
            &["SSLMode=Disable"],
            Off,
        );
        off(
            "teradata",
            &[("SSLMode", "Require"), ("SSLCA", "/ca.pem")],
            &["SSLMode=Require", "SSLCA=/ca.pem"],
            Unverified,
        );
        off(
            "vertica",
            &[("SSLMode", "prefer")],
            &["SSLMode=prefer"],
            Optional,
        );
        off(
            "exasol",
            &[("SSLCERTIFICATE", "SSL_VERIFY_NONE")],
            &["SSLCERTIFICATE=SSL_VERIFY_NONE"],
            Unverified,
        );
        let s = off("exasol", &[("ENCRYPTION", "false")], &["ENCRYPTION=N"], Off);
        assert!(!s.contains("SSLCERTIFICATE"), "{s}");
        off(
            "netezza",
            &[("SecurityLevel", "preferredSecured")],
            &["SecurityLevel=preferredSecured"],
            Optional,
        );
        let s = off(
            "openedge",
            &[("ValidateServerCertificate", "false")],
            &["ValidateServerCertificate=0"],
            Unverified,
        );
        assert!(!s.contains("HostNameInCertificate"), "{s}");
        off(
            "openedge",
            &[("EncryptionMethod", "false")],
            &["EncryptionMethod=0"],
            Off,
        );
        off(
            "openedge",
            &[
                ("HostNameInCertificate", "#SERVERNAME#"),
                ("Truststore", "/ts.pem"),
            ],
            &["HostNameInCertificate=#SERVERNAME#", "Truststore=/ts.pem"],
            VerifyFull,
        );
        off(
            "sqlanywhere",
            &[("Encryption", "SIMPLE")],
            &["Encryption=SIMPLE"],
            Off,
        );
        let s = off(
            "sqlanywhere",
            &[
                ("trusted_certificates", "/ca.pem"),
                ("skip_certificate_name_check", "true"),
            ],
            &[],
            VerifyCa,
        );
        assert!(
            s.ends_with(
                ";Encryption=TLS(trusted_certificates=/ca.pem;skip_certificate_name_check=ON)"
            ),
            "{s}"
        );
        off(
            "informix",
            &[("PROTOCOL", "onsoctcp")],
            &["PROTOCOL=onsoctcp"],
            Off,
        );
        off(
            "ignite3",
            &[("SSL_MODE", "disable"), ("SSL_CA_FILE", "/ca.pem")],
            &["SSL_MODE=disable"],
            Off,
        );
        off(
            "ignite",
            &[("SSL_CA_FILE", "/ca.pem")],
            &["SSL_CA_FILE=/ca.pem"],
            VerifyCa,
        );
        off(
            "monetdb",
            &[("TLS", "false"), ("CERT", "/ca.pem")],
            &["TLS=Off"],
            Off,
        );
        off(
            "monetdb",
            &[("CERT", "/ca.pem")],
            &["TLS=On", "CERT=/ca.pem"],
            VerifyFull,
        );
        off(
            "ocient",
            &[("TLS", "UNVERIFIED")],
            &["TLS=UNVERIFIED"],
            Unverified,
        );
        off(
            "altibase",
            &[("SSL_VERIFY", "false")],
            &["CONNTYPE=6", "SSL_VERIFY=0"],
            Unverified,
        );
        let s = off("altibase", &[("CONNTYPE", "1")], &["CONNTYPE=1"], Off);
        assert!(!s.contains("SSL_VERIFY"), "{s}");
    }

    /// A key typed in the additional attributes wins over the field.
    #[test]
    fn additional_attributes_win() {
        let s = conn_str(
            "vertica",
            &new_conn("vertica", &[("extra", "sslmode=require")]),
        );
        assert_eq!(
            s.matches("SSLMode=").count() + s.matches("sslmode=").count(),
            1,
            "{s}"
        );
        assert!(s.ends_with(";sslmode=require"), "{s}");
        let s = conn_str(
            "sqlanywhere",
            &new_conn("sqlanywhere", &[("extra", "ENC=NONE")]),
        );
        assert!(!s.contains("Encryption="), "{s}");
    }

    /// Through an SSH tunnel the drivers that can't be told the real host
    /// name refuse a host-name check, naming what to change; the ones that
    /// can get it.
    #[test]
    fn tunnels_never_silently_drop_the_host_name_check() {
        let tunnel = |id: &str, opts: &[(&str, &str)]| {
            let mut c = new_conn(id, opts);
            c.host = "127.0.0.1".into();
            c.options
                .insert(TUNNEL_ORIGIN_HOST.into(), "real.example.com".into());
            build(preset(id), &c, "db")
        };
        for (id, fix) in [
            ("db2", "SSLClientHostnameValidation=OFF"),
            ("spark", "AllowHostNameCNMismatch=1"),
            ("teradata", "SSLMode=Verify-CA"),
            ("vertica", "SSLMode=verify-ca"),
            ("monetdb", "CERTHASH"),
        ] {
            let e = tunnel(id, &[]).unwrap_err().to_string();
            assert!(
                e.contains(fix) && e.contains("real.example.com"),
                "{id}: {e}"
            );
        }
        assert!(tunnel("db2", &[("SSLClientHostnameValidation", "false")]).is_ok());
        assert!(tunnel("teradata", &[("SSLMode", "Verify-CA")]).is_ok());
        assert!(tunnel("monetdb", &[("extra", "CERTHASH=sha256:ab12")]).is_ok());
        // Levels that don't check the name go through as they are.
        assert!(tunnel("ignite", &[]).is_ok());
        assert!(tunnel("informix", &[]).is_ok());
        let s = tunnel("openedge", &[]).unwrap();
        assert!(s.contains(";HostNameInCertificate=real.example.com"), "{s}");
        let s = tunnel("sqlanywhere", &[]).unwrap();
        assert!(
            s.ends_with(";Encryption=TLS(certificate_name=real.example.com)"),
            "{s}"
        );
    }

    #[test]
    fn declarations() {
        for p in PRESETS {
            let (support, _) = support(p);
            let f = fields(p);
            let has_mode = f
                .iter()
                .any(|x| matches!(x.tls, Some(dbine_driver::TlsRole::Mode { .. })));
            match support {
                TlsSupport::Configurable => {}
                _ => assert!(!has_mode, "{}", p.id),
            }
            // Native names: no field is a translated label.
            for x in &f {
                if !matches!(x.kind, FieldKind::Select(_)) {
                    assert!(x.label.is_ascii(), "{}: {}", p.id, x.label);
                }
            }
        }
        assert_eq!(support(preset("db2i")).1, Some(9471));
        assert_eq!(support(preset("altibase")).1, Some(20443));
        assert_eq!(support(preset("odbc")).0, TlsSupport::InUrl);
        assert_eq!(support(preset("access")).0, TlsSupport::NotApplicable);
        assert_eq!(support(preset("zen")).0, TlsSupport::None);
        assert_eq!(support(preset("netsuite")).0, TlsSupport::Forced);
    }
}
