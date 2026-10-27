//! What a driver tells the UI about itself: the connection form, the kinds
//! of objects its explorer shows and the language its editor speaks. The UI
//! is built from this, so adding an engine never touches the frontend.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Relational,
    Analytical,
    Document,
    KeyValue,
    WideColumn,
    Search,
    TimeSeries,
    Streaming,
    Graph,
}

/// Editor language: the UI picks syntax highlighting and completion from it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// SQL; `dialect` in [`DriverInfo`] refines it.
    Sql,
    /// Cassandra Query Language.
    Cql,
    /// JSON commands / query documents (MongoDB, CouchDB, Elasticsearch,
    /// Solr, DynamoDB).
    Json,
    /// Redis commands, one per line.
    Redis,
    /// InfluxDB Flux.
    Flux,
    /// Cypher / openCypher (Neo4j, Neptune, Memgraph…).
    Cypher,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverInfo {
    /// Stable id stored in saved connections ("postgres", "mongodb"…).
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub id: &'static str,
    /// Display name ("PostgreSQL").
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub name: &'static str,
    pub family: Family,
    pub language: Language,
    /// SQL dialect hint for the editor: "postgres", "mysql", "mssql",
    /// "sqlite", "standard"… Empty for non-SQL languages.
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub dialect: &'static str,
    pub default_port: u16,
    /// Fields of the connection form, in order.
    pub fields: Vec<Field>,
    /// What the level below the connection is called ("Bases de datos",
    /// "Keyspaces", "Índices"…). Empty when the engine has a single
    /// namespace: the explorer then shows objects right under the connection.
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub databases_label: &'static str,
    /// Objects live in schemas below the database.
    pub has_schemas: bool,
    /// Kinds of objects `list_objects` returns, in explorer order.
    pub object_kinds: Vec<ObjectKindInfo>,
    /// How the engine's connections are encrypted (absent in older
    /// manifests: [`TlsSupport::Configurable`]).
    #[serde(default)]
    pub tls: TlsSupport,
    /// The port the engine listens on for TLS when it isn't `default_port`
    /// (ClickHouse 8443, Oracle 2484…): a new connection whose TLS mode is on
    /// starts with it. `None`: the same port.
    #[serde(default)]
    pub tls_port: Option<u16>,
}

/// How a driver's connections are encrypted, for the drivers that don't
/// declare a [`TlsRole::Mode`] field (and for the registry's check that every
/// engine says something about it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsSupport {
    /// The form's TLS fields set it (a [`TlsRole::Mode`] field, or the
    /// legacy `encrypt` / `trust_server_certificate` ones).
    #[default]
    Configurable,
    /// Always TLS, verified, nothing to choose (a cloud API, IAM sign-ins).
    Forced,
    /// The URL or connection string the user types says it (`https://`,
    /// `libsql://`…).
    InUrl,
    /// The engine has no TLS (listed in docs/engine-support.md).
    None,
    /// Not a network connection (a local file).
    NotApplicable,
    /// A value a newer driver sends that this app doesn't know.
    #[serde(other)]
    Unknown,
}

/// How sure a TLS setting is about the server: for the form's warning chip,
/// the projection to the legacy `encrypt` / `trust_server_certificate` pair
/// and the connection-string export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsLevel {
    /// Plaintext.
    Off,
    /// TLS when the server offers it (`prefer`, `PREFERRED`, `Optional`…).
    Optional,
    /// TLS required, the certificate isn't checked.
    Unverified,
    /// TLS required, the chain is checked but not the host name.
    VerifyCa,
    /// TLS required, chain and host name checked.
    VerifyFull,
}

impl TlsLevel {
    /// The closest legacy `(encrypt, trust_server_certificate)` pair, written
    /// next to the native option so an older app or driver gets the nearest
    /// behaviour instead of plaintext.
    pub fn legacy_pair(self) -> (bool, bool) {
        match self {
            Self::Off | Self::Optional => (false, false),
            Self::Unverified => (true, true),
            Self::VerifyCa | Self::VerifyFull => (true, false),
        }
    }

    /// Checks the server's certificate (chain at least).
    pub fn verified(self) -> bool {
        self >= Self::VerifyCa
    }
}

/// The part a [`Field`] plays in the engine's TLS settings. Its `key` and
/// `label` are the engine's own spelling (`sslmode`, `Encrypt`,
/// `TrustServerCertificate`, `tls`…) and its values the engine's own values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "role")]
pub enum TlsRole {
    /// A select or a checkbox whose values set the level: `(value, level)`
    /// pairs (`("verify-full", VerifyFull)`, `("true", VerifyFull)`…).
    Mode {
        #[serde(deserialize_with = "crate::serde_static::level_pairs")]
        levels: Vec<(&'static str, TlsLevel)>,
    },
    /// A select that says how much of the certificate is checked, next to
    /// the mode: `(value, level)` pairs, each value with the highest level
    /// it allows (Trino `SSLVerification` `FULL` / `CA` / `NONE`,
    /// Elasticsearch `verification_mode` `full` / `certificate` / `none`,
    /// Avatica `hostname_verification` `STRICT` / `NONE`). It caps the
    /// mode's level and never raises it. An older app reads it as no role,
    /// so a driver that declares one needs an app that knows it.
    Verify {
        #[serde(deserialize_with = "crate::serde_static::level_pairs")]
        levels: Vec<(&'static str, TlsLevel)>,
    },
    /// A checkbox that skips the certificate checks when "true"
    /// (`TrustServerCertificate`, `tlsAllowInvalidCertificates`, `insecure`…).
    /// `inverted`: "false" skips them (`validateCertificate`…).
    SkipVerify {
        #[serde(default)]
        inverted: bool,
    },
    /// A checkbox that skips only the host-name check when "true"
    /// (`tlsAllowInvalidHostnames`, `disableHostVerification`…), capping the
    /// level at [`TlsLevel::VerifyCa`]. `inverted`: "false" skips it
    /// (`ssl_assert_hostname`, `verifyHost`, `check_hostname`…). `inverted`
    /// is written only when true, so a plain one serializes as before.
    SkipHostname {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        inverted: bool,
    },
    /// The CA / trust-store file. `required_at`: the levels the driver can't
    /// verify at without it (no system roots for them: MySQL `VERIFY_CA`,
    /// Netezza `onlySecured`…): the form requires it there and
    /// [`crate::ConnectionConfig::require_tls_ca`] refuses to connect
    /// without it. Empty (and not written): always optional.
    CaFile {
        #[serde(
            default,
            skip_serializing_if = "Vec::is_empty",
            deserialize_with = "crate::serde_static::levels"
        )]
        required_at: Vec<TlsLevel>,
    },
    /// A host-name override for the certificate check
    /// (`HostNameInCertificate`…).
    ServerName,
}

/// The value a saved connection that predates a TLS field behaves as, by its
/// legacy `(encrypt, trust_server_certificate)`: `[ff, ft, tf, tt]` (see
/// [`crate::ConnectionConfig::legacy_tls_index`]). An empty string means
/// "not set": the driver keeps its legacy code path for that combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Legacy(#[serde(deserialize_with = "crate::serde_static::str4")] pub [&'static str; 4]);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    /// `host`, `port`, `database`, `username`, `password`, `encrypt`,
    /// `trust_server_certificate`, `read_only` map to the typed fields of
    /// `ConnectionConfig`; any other key goes to `options`.
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub key: &'static str,
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    /// Kept in the OS keychain, never in the state file.
    pub secret: bool,
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub placeholder: &'static str,
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub default: &'static str,
    /// Help text under the field.
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub help: &'static str,
    /// The connection form's tab it's in.
    #[serde(default)]
    pub section: FieldSection,
    /// Shown (and saved) only when another field has one of these values:
    /// the fields of one authentication method, of one way to connect…
    #[serde(default)]
    pub when: Option<FieldWhen>,
    /// The group it's shown under in a long form ("Archivos", "Opciones"…):
    /// the create-database dialog makes a tab of each. Empty: the first.
    #[serde(default, deserialize_with = "crate::serde_static::str")]
    pub group: &'static str,
    /// Its part in the engine's TLS settings (absent: none). A role this
    /// build doesn't know (a newer driver's) reads as none.
    #[serde(default, deserialize_with = "crate::serde_static::lenient")]
    pub tls: Option<TlsRole>,
    /// What a saved connection without this option behaves as (TLS fields;
    /// absent: the field's `default` applies to new connections only and
    /// old ones have no value).
    #[serde(default, deserialize_with = "crate::serde_static::lenient")]
    pub legacy: Option<Legacy>,
}

/// Tabs of the connection form (besides the SSH tunnel's, which the app adds
/// to the engines that connect over the network).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldSection {
    /// Where the server is and who connects.
    #[default]
    General,
    /// Encryption and certificates.
    Ssl,
    /// Everything else: timeouts, pools, behavior.
    Advanced,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldWhen {
    /// The other field (a select or a checkbox: "true" / "false").
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub key: &'static str,
    #[serde(deserialize_with = "crate::serde_static::strs")]
    pub values: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "options")]
pub enum FieldKind {
    Text,
    Number,
    Password,
    Bool,
    /// Path to a local file (the form shows a picker).
    File,
    /// Multi-line text (e.g. a service-account JSON).
    Textarea,
    /// One of these `(value, label)` pairs.
    Select(
        #[serde(deserialize_with = "crate::serde_static::pairs")] Vec<(&'static str, &'static str)>,
    ),
}

impl Field {
    pub fn new(key: &'static str, label: &'static str, kind: FieldKind) -> Self {
        Self {
            key,
            label,
            kind,
            required: false,
            secret: false,
            placeholder: "",
            default: "",
            help: "",
            section: FieldSection::General,
            when: None,
            group: "",
            tls: None,
            legacy: None,
        }
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    pub fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = p;
        self
    }
    pub fn default_value(mut self, d: &'static str) -> Self {
        self.default = d;
        self
    }
    pub fn help(mut self, h: &'static str) -> Self {
        self.help = h;
        self
    }
    /// Under this group (a tab of the create-database dialog).
    pub fn group(mut self, g: &'static str) -> Self {
        self.group = g;
        self
    }
    /// In the SSL tab.
    pub fn ssl(mut self) -> Self {
        self.section = FieldSection::Ssl;
        self
    }
    /// In the Advanced tab.
    pub fn advanced(mut self) -> Self {
        self.section = FieldSection::Advanced;
        self
    }
    /// Only when the field `key` has one of `values` (a checkbox: "true").
    pub fn when(mut self, key: &'static str, values: &[&'static str]) -> Self {
        self.when = Some(FieldWhen {
            key,
            values: values.to_vec(),
        });
        self
    }

    /// Its part in the engine's TLS settings (in the SSL tab).
    pub fn tls(mut self, role: TlsRole) -> Self {
        self.tls = Some(role);
        self.ssl()
    }
    /// What a saved connection without this option behaves as, by its legacy
    /// `(encrypt, trust_server_certificate)`: `[ff, ft, tf, tt]`.
    pub fn legacy(mut self, values: [&'static str; 4]) -> Self {
        self.legacy = Some(Legacy(values));
        self
    }

    /// The engine's TLS mode, as a select: `key` and `label` are its own
    /// spelling (`sslmode`, `Encrypt`, `ssl-mode`…), `options` its values
    /// with a short gloss and the level each one gives
    /// (`("verify-full", "verify-full (CA y nombre)", VerifyFull)`).
    /// `default` is a new connection's value (a verified one); `legacy` what
    /// a saved connection from before the field behaves as.
    pub fn tls_mode(
        key: &'static str,
        label: &'static str,
        options: &[(&'static str, &'static str, TlsLevel)],
        default: &'static str,
        legacy: [&'static str; 4],
    ) -> Self {
        Self::new(
            key,
            label,
            FieldKind::Select(options.iter().map(|(v, l, _)| (*v, *l)).collect()),
        )
        .default_value(default)
        .tls(TlsRole::Mode {
            levels: options.iter().map(|(v, _, lv)| (*v, *lv)).collect(),
        })
        .legacy(legacy)
    }
    /// The engine's TLS switch, as a checkbox (`tls`, `ssl`, `SSL`…): "true"
    /// gives `on`, "false" gives `off`.
    pub fn tls_switch(
        key: &'static str,
        label: &'static str,
        on: TlsLevel,
        off: TlsLevel,
        default: bool,
        legacy: [&'static str; 4],
    ) -> Self {
        Self::new(key, label, FieldKind::Bool)
            .default_value(if default { "true" } else { "false" })
            .tls(TlsRole::Mode {
                levels: vec![("true", on), ("false", off)],
            })
            .legacy(legacy)
    }
    /// A checkbox that skips the certificate checks (`TrustServerCertificate`,
    /// `tlsAllowInvalidCertificates`…); `inverted`: unchecking it skips them
    /// (`validateCertificate`). A new connection verifies.
    pub fn tls_skip_verify(
        key: &'static str,
        label: &'static str,
        inverted: bool,
        legacy: [&'static str; 4],
    ) -> Self {
        Self::new(key, label, FieldKind::Bool)
            .default_value(if inverted { "true" } else { "false" })
            .tls(TlsRole::SkipVerify { inverted })
            .legacy(legacy)
    }
    /// A checkbox that skips only the host-name check
    /// (`tlsAllowInvalidHostnames`, `disableHostVerification`…), capping the
    /// level at [`TlsLevel::VerifyCa`]; `inverted`: unchecking it skips the
    /// check (`ssl_assert_hostname`, `verifyHost`). A new connection checks
    /// the host name.
    pub fn tls_skip_hostname(
        key: &'static str,
        label: &'static str,
        inverted: bool,
        legacy: [&'static str; 4],
    ) -> Self {
        Self::new(key, label, FieldKind::Bool)
            .default_value(if inverted { "true" } else { "false" })
            .tls(TlsRole::SkipHostname { inverted })
            .legacy(legacy)
    }
    /// How much of the certificate is checked, as a select next to the mode
    /// (Trino `SSLVerification`, Elasticsearch `verification_mode`…):
    /// `options` are its values with a short gloss and the highest level
    /// each one allows (`("CA", "CA", VerifyCa)`). It caps the mode's level.
    /// `default` is a new connection's value (a verified one); `legacy` what
    /// a saved connection from before the field behaves as.
    pub fn tls_verify(
        key: &'static str,
        label: &'static str,
        options: &[(&'static str, &'static str, TlsLevel)],
        default: &'static str,
        legacy: [&'static str; 4],
    ) -> Self {
        Self::new(
            key,
            label,
            FieldKind::Select(options.iter().map(|(v, l, _)| (*v, *l)).collect()),
        )
        .default_value(default)
        .tls(TlsRole::Verify {
            levels: options.iter().map(|(v, _, lv)| (*v, *lv)).collect(),
        })
        .legacy(legacy)
    }
    /// The CA / trust-store file (`sslrootcert`, `ssl-ca`, `tlsCAFile`…).
    pub fn tls_ca_file(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::File).tls(TlsRole::CaFile {
            required_at: vec![],
        })
    }
    /// Makes a CA file ([`Field::tls_ca_file`]) required at these levels,
    /// the ones the driver can't verify at without it. The form marks it
    /// required there; the driver calls
    /// [`crate::ConnectionConfig::require_tls_ca`] before connecting.
    pub fn ca_required_at(mut self, levels: &[TlsLevel]) -> Self {
        if let Some(TlsRole::CaFile { required_at }) = &mut self.tls {
            *required_at = levels.to_vec();
        }
        self
    }
    /// curl's `insecure` (`-k`): the skip-verify option of an engine whose
    /// clients have no name of their own for it.
    pub fn tls_insecure(legacy: [&'static str; 4]) -> Self {
        Self::tls_skip_verify("insecure", "insecure", false, legacy)
            .help("No verifica el certificado del servidor (como curl --insecure).")
    }
    /// curl's `cacert`: the CA file of an engine whose clients have no name
    /// of their own for it.
    pub fn tls_cacert() -> Self {
        Self::tls_ca_file("cacert", "cacert").help(
            "Archivo PEM de la CA que firmó el certificado del servidor (como curl --cacert).",
        )
    }
    /// A host-name override for the certificate check
    /// (`HostNameInCertificate`…).
    pub fn tls_server_name(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Text).tls(TlsRole::ServerName)
    }

    /// The field is one of the legacy typed TLS bools (`encrypt`,
    /// `trust_server_certificate`) instead of the engine's own options.
    pub fn is_legacy_tls(&self) -> bool {
        matches!(self.key, "encrypt" | "trust_server_certificate")
    }

    /// The level a value of this field gives, when it's the engine's TLS
    /// mode ([`TlsRole::Mode`]), or the highest level it allows when it's a
    /// verification select ([`TlsRole::Verify`]).
    pub fn tls_level_of(&self, value: &str) -> Option<TlsLevel> {
        match &self.tls {
            Some(TlsRole::Mode { levels } | TlsRole::Verify { levels }) => levels
                .iter()
                .find(|(v, _)| *v == value)
                .map(|(_, level)| *level),
            _ => None,
        }
    }

    /// This value of the field turns the certificate checks off
    /// ([`TlsRole::SkipVerify`]) or the host-name check
    /// ([`TlsRole::SkipHostname`]).
    pub fn tls_skips(&self, value: &str) -> bool {
        match &self.tls {
            Some(TlsRole::SkipVerify { inverted } | TlsRole::SkipHostname { inverted }) => {
                (value == "true") != *inverted
            }
            _ => false,
        }
    }

    /// The CA file ([`TlsRole::CaFile`]) is required at this level.
    pub fn tls_ca_required_at(&self, level: TlsLevel) -> bool {
        matches!(&self.tls, Some(TlsRole::CaFile { required_at }) if required_at.contains(&level))
    }

    // The usual ones.
    pub fn host() -> Self {
        Self::new("host", "Servidor", FieldKind::Text)
            .required()
            .placeholder("localhost")
    }
    pub fn port() -> Self {
        Self::new("port", "Puerto", FieldKind::Number)
    }
    pub fn database() -> Self {
        Self::new("database", "Base de datos", FieldKind::Text).placeholder("(la predeterminada)")
    }
    pub fn username() -> Self {
        Self::new("username", "Usuario", FieldKind::Text)
    }
    pub fn password() -> Self {
        Self::new("password", "Contraseña", FieldKind::Password).secret()
    }
    /// Legacy, for the drivers not migrated to their engine's own TLS
    /// options yet (a [`TlsRole::Mode`] field): the typed `encrypt` bool.
    pub fn encrypt() -> Self {
        Self::new("encrypt", "Cifrar la conexión (TLS)", FieldKind::Bool).ssl()
    }
    /// Legacy, like [`Field::encrypt`]: the typed `trust_server_certificate`.
    pub fn trust_cert() -> Self {
        Self::new(
            "trust_server_certificate",
            "Confiar en el certificado del servidor",
            FieldKind::Bool,
        )
        .ssl()
    }
    pub fn read_only() -> Self {
        Self::new("read_only", "Solo lectura", FieldKind::Bool)
    }
    pub fn file(label: &'static str) -> Self {
        Self::new("host", label, FieldKind::File).required()
    }

    /// host, port, database, user, password, TLS, trust, read-only.
    pub fn server_set() -> Vec<Self> {
        vec![
            Self::host(),
            Self::port(),
            Self::database(),
            Self::username(),
            Self::password(),
            Self::encrypt(),
            Self::trust_cert(),
            Self::read_only(),
        ]
    }
}

/// Well-known object kinds. Drivers may use their own ids too; the UI falls
/// back to a generic icon.
pub mod kinds {
    pub const TABLE: &str = "table";
    pub const VIEW: &str = "view";
    pub const MATERIALIZED_VIEW: &str = "materialized_view";
    pub const PROCEDURE: &str = "procedure";
    pub const FUNCTION: &str = "function";
    pub const TRIGGER: &str = "trigger";
    pub const SEQUENCE: &str = "sequence";
    pub const SYNONYM: &str = "synonym";
    /// A user-defined type (alias, table type, enum, composite…).
    pub const TYPE: &str = "type";
    pub const COLLECTION: &str = "collection";
    pub const KEY: &str = "key";
    pub const INDEX: &str = "index";
    pub const TOPIC: &str = "topic";
    pub const STREAM: &str = "stream";
    pub const MEASUREMENT: &str = "measurement";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectKindInfo {
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub id: &'static str,
    /// Folder label in the explorer ("Tablas").
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub label: &'static str,
    /// Expands to its columns / fields.
    pub has_columns: bool,
    /// Can be opened as data ([`crate::Session::browse_query`]).
    pub browsable: bool,
    /// Has source to show ([`crate::Session::definition`]).
    pub has_definition: bool,
}

impl ObjectKindInfo {
    pub const fn new(
        id: &'static str,
        label: &'static str,
        has_columns: bool,
        browsable: bool,
        has_definition: bool,
    ) -> Self {
        Self {
            id,
            label,
            has_columns,
            browsable,
            has_definition,
        }
    }
    pub const fn tables() -> Self {
        Self::new(kinds::TABLE, "Tablas", true, true, true)
    }
    pub const fn views() -> Self {
        Self::new(kinds::VIEW, "Vistas", true, true, true)
    }
    pub const fn materialized_views() -> Self {
        Self::new(
            kinds::MATERIALIZED_VIEW,
            "Vistas materializadas",
            true,
            true,
            true,
        )
    }
    pub const fn procedures() -> Self {
        Self::new(kinds::PROCEDURE, "Procedimientos", false, false, true)
    }
    pub const fn functions() -> Self {
        Self::new(kinds::FUNCTION, "Funciones", false, false, true)
    }
    pub const fn triggers() -> Self {
        Self::new(kinds::TRIGGER, "Triggers", false, false, true)
    }
    pub const fn sequences() -> Self {
        Self::new(kinds::SEQUENCE, "Secuencias", false, false, true)
    }
    pub const fn synonyms() -> Self {
        Self::new(kinds::SYNONYM, "Sinónimos", false, false, true)
    }
    pub const fn types() -> Self {
        Self::new(kinds::TYPE, "Tipos", false, false, true)
    }
    pub const fn collections() -> Self {
        Self::new(kinds::COLLECTION, "Colecciones", true, true, false)
    }
}

/// A server's suggestions for one field of a form whose values depend on the
/// server (the collations it has, its default data path, its users…).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldChoices {
    /// The field's `key`.
    pub key: String,
    /// What the server uses when the field is left empty, to show it.
    pub default: Option<String>,
    /// Values to pick from (the field still takes any other).
    pub values: Vec<String>,
}

/// One read-only fact in a database's properties (size, created, state…).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PropertyInfo {
    /// The tab it's shown in (empty: General).
    pub group: String,
    pub label: String,
    pub value: String,
}

/// What "Propiedades" shows for one database: the settings that can be
/// changed (as form fields, in tabs by `group`) with their current values,
/// read-only facts, the server's suggestions, and warnings for changes that
/// disrupt the database (they kick sessions, take it offline…).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatabaseProperties {
    #[serde(deserialize_with = "crate::serde_static::owned")]
    pub fields: Vec<Field>,
    /// Field key → its current value.
    pub values: std::collections::BTreeMap<String, String>,
    pub info: Vec<PropertyInfo>,
    pub choices: Vec<FieldChoices>,
    /// Field key → what changing it does to the database or its users, shown
    /// before the change runs.
    pub warnings: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read<T: Deserialize<'static>>(v: serde_json::Value) -> T {
        T::deserialize(v).unwrap()
    }

    fn info(fields: Vec<Field>) -> DriverInfo {
        DriverInfo {
            id: "x",
            name: "X",
            family: Family::Relational,
            language: Language::Sql,
            dialect: "",
            default_port: 1521,
            fields,
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![],
            tls: TlsSupport::Configurable,
            tls_port: Some(2484),
        }
    }

    #[test]
    fn tls_fields_round_trip() {
        let mode = Field::tls_mode(
            "sslmode",
            "sslmode",
            &[
                ("disable", "disable", TlsLevel::Off),
                (
                    "verify-full",
                    "verify-full (CA y nombre)",
                    TlsLevel::VerifyFull,
                ),
            ],
            "verify-full",
            ["disable", "disable", "verify-full", "disable"],
        );
        assert_eq!(mode.section, FieldSection::Ssl);
        assert_eq!(mode.tls_level_of("verify-full"), Some(TlsLevel::VerifyFull));
        assert_eq!(mode.tls_level_of("nope"), None);
        let i = info(vec![
            Field::host(),
            mode,
            Field::tls_skip_verify(
                "TrustServerCertificate",
                "TrustServerCertificate",
                false,
                ["false"; 4],
            ),
            Field::tls_ca_file("sslrootcert", "sslrootcert"),
            Field::tls_server_name("HostNameInCertificate", "HostNameInCertificate"),
        ]);
        let json = serde_json::to_value(&i).unwrap();
        assert_eq!(json["tls"], "configurable");
        assert_eq!(json["tls_port"], 2484);
        assert_eq!(json["fields"][1]["tls"]["role"], "mode");
        assert_eq!(json["fields"][1]["tls"]["levels"][1][1], "verify_full");
        assert_eq!(json["fields"][1]["legacy"][2], "verify-full");
        assert_eq!(json["fields"][2]["tls"]["role"], "skip_verify");
        let back: DriverInfo = read(json);
        assert_eq!(back.tls_port, Some(2484));
        assert_eq!(back.fields[1].tls, i.fields[1].tls);
        assert_eq!(back.fields[1].legacy, i.fields[1].legacy);
        assert_eq!(
            back.fields[2].tls,
            Some(TlsRole::SkipVerify { inverted: false })
        );
        assert_eq!(
            back.fields[3].tls,
            Some(TlsRole::CaFile {
                required_at: vec![]
            })
        );
        assert_eq!(back.fields[4].tls, Some(TlsRole::ServerName));
        assert!(back.fields[0].tls.is_none() && back.fields[0].legacy.is_none());
    }

    #[test]
    fn an_older_manifest_reads_with_defaults() {
        // A DriverInfo as published before the TLS fields existed.
        let mut json = serde_json::to_value(info(Field::server_set())).unwrap();
        let o = json.as_object_mut().unwrap();
        o.remove("tls");
        o.remove("tls_port");
        for f in o["fields"].as_array_mut().unwrap() {
            let f = f.as_object_mut().unwrap();
            f.remove("tls");
            f.remove("legacy");
        }
        let back: DriverInfo = read(json);
        assert_eq!(back.tls, TlsSupport::Configurable);
        assert_eq!(back.tls_port, None);
        assert!(back
            .fields
            .iter()
            .all(|f| f.tls.is_none() && f.legacy.is_none()));
        assert!(back.fields.iter().filter(|f| f.is_legacy_tls()).count() == 2);
    }

    #[test]
    fn a_newer_drivers_unknown_tls_values_dont_break_the_manifest() {
        let mut json = serde_json::to_value(info(vec![Field::tls_switch(
            "tls",
            "tls",
            TlsLevel::VerifyFull,
            TlsLevel::Off,
            true,
            ["false", "false", "true", "true"],
        )]))
        .unwrap();
        json["tls"] = "quantum".into();
        json["fields"][0]["tls"]["levels"][0][1] = "post_quantum".into();
        let back: DriverInfo = read(json.clone());
        assert_eq!(back.tls, TlsSupport::Unknown);
        assert_eq!(
            back.fields[0].tls,
            Some(TlsRole::Mode {
                levels: vec![("false", TlsLevel::Off)]
            })
        );
        json["fields"][0]["tls"] = serde_json::json!({"role": "pinned_key"});
        json["fields"][0]["legacy"] = serde_json::json!(["a", "b"]);
        let back: DriverInfo = read(json);
        assert!(back.fields[0].tls.is_none() && back.fields[0].legacy.is_none());
    }

    #[test]
    fn skip_fields_say_when_they_skip() {
        let skip = Field::tls_skip_verify("insecure", "insecure", false, ["false"; 4]);
        assert!(skip.tls_skips("true") && !skip.tls_skips("false"));
        assert_eq!(skip.default, "false");
        let validate = Field::tls_skip_verify("validate", "validate", true, ["true"; 4]);
        assert!(validate.tls_skips("false") && !validate.tls_skips("true"));
        assert_eq!(validate.default, "true");
        let host = Field::tls_skip_hostname(
            "tlsAllowInvalidHostnames",
            "tlsAllowInvalidHostnames",
            false,
            ["false"; 4],
        );
        assert!(host.tls_skips("true") && !host.tls_skips("false"));
        assert_eq!(host.default, "false");
        assert_eq!(host.section, FieldSection::Ssl);
        let assert_host = Field::tls_skip_hostname(
            "ssl_assert_hostname",
            "ssl_assert_hostname",
            true,
            ["true"; 4],
        );
        assert!(assert_host.tls_skips("false") && !assert_host.tls_skips("true"));
        assert_eq!(assert_host.default, "true");
    }

    fn ssl_verification() -> Field {
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
        )
    }

    #[test]
    fn verification_select_gives_the_level_each_value_allows() {
        let f = ssl_verification();
        assert_eq!(f.section, FieldSection::Ssl);
        assert_eq!(f.default, "FULL");
        assert_eq!(f.tls_level_of("CA"), Some(TlsLevel::VerifyCa));
        assert_eq!(f.tls_level_of("NONE"), Some(TlsLevel::Unverified));
        assert_eq!(f.tls_level_of("bogus"), None);
        assert!(!f.tls_skips("NONE"));
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["tls"]["role"], "verify");
        assert_eq!(json["tls"]["levels"][1][1], "verify_ca");
        let back: Field = read(json);
        assert_eq!(back.tls, f.tls);
        assert_eq!(back.legacy, f.legacy);
    }

    #[test]
    fn a_ca_file_can_be_required_at_some_levels() {
        let plain = Field::tls_ca_file("ssl-ca", "ssl-ca");
        assert!(!plain.tls_ca_required_at(TlsLevel::VerifyCa));
        let ca = Field::tls_ca_file("ssl-ca", "ssl-ca").ca_required_at(&[TlsLevel::VerifyCa]);
        assert!(ca.tls_ca_required_at(TlsLevel::VerifyCa));
        assert!(!ca.tls_ca_required_at(TlsLevel::VerifyFull));
        // Only a CA file takes it.
        let host = Field::host().ca_required_at(&[TlsLevel::VerifyCa]);
        assert!(host.tls.is_none() && !host.tls_ca_required_at(TlsLevel::VerifyCa));
        let mut json = serde_json::to_value(&ca).unwrap();
        assert_eq!(
            json["tls"],
            serde_json::json!({"role": "ca_file", "required_at": ["verify_ca"]})
        );
        // A level this build doesn't know is dropped.
        json["tls"]["required_at"] = serde_json::json!(["post_quantum", "verify_full"]);
        let back: Field = read(json);
        assert_eq!(
            back.tls,
            Some(TlsRole::CaFile {
                required_at: vec![TlsLevel::VerifyFull]
            })
        );
    }

    #[test]
    fn plain_roles_serialize_as_before_the_new_options() {
        // A non-inverted skip-hostname and an optional CA file are written
        // exactly as before `inverted` / `required_at` existed, and what an
        // older driver wrote reads as them.
        let host = Field::tls_skip_hostname("h", "h", false, ["false"; 4]);
        let ca = Field::tls_cacert();
        assert_eq!(
            serde_json::to_value(&host).unwrap()["tls"],
            serde_json::json!({"role": "skip_hostname"})
        );
        assert_eq!(
            serde_json::to_value(&ca).unwrap()["tls"],
            serde_json::json!({"role": "ca_file"})
        );
        assert_eq!(
            read::<TlsRole>(serde_json::json!({"role": "skip_hostname"})),
            TlsRole::SkipHostname { inverted: false }
        );
        assert_eq!(
            read::<TlsRole>(serde_json::json!({"role": "ca_file"})),
            TlsRole::CaFile {
                required_at: vec![]
            }
        );
        assert_eq!(
            serde_json::to_value(Field::tls_skip_hostname("h", "h", true, ["true"; 4])).unwrap()
                ["tls"],
            serde_json::json!({"role": "skip_hostname", "inverted": true})
        );
    }

    #[test]
    fn an_older_app_reads_the_new_options() {
        // TlsRole as it was before `Verify`, `SkipHostname { inverted }` and
        // `CaFile { required_at }`, read the way `Field::tls` reads it.
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(rename_all = "snake_case", tag = "role")]
        enum OldRole {
            Mode {
                #[serde(deserialize_with = "crate::serde_static::level_pairs")]
                levels: Vec<(&'static str, TlsLevel)>,
            },
            SkipVerify {
                #[serde(default)]
                inverted: bool,
            },
            SkipHostname,
            CaFile,
            ServerName,
        }
        #[derive(Deserialize)]
        struct OldField {
            #[serde(default, deserialize_with = "crate::serde_static::lenient")]
            tls: Option<OldRole>,
        }
        let old = |f: Field| read::<OldField>(serde_json::to_value(f).unwrap()).tls;
        let ca = Field::tls_ca_file("c", "c").ca_required_at(&[TlsLevel::VerifyCa]);
        assert_eq!(old(ca), Some(OldRole::CaFile));
        let host = Field::tls_skip_hostname("h", "h", true, ["true"; 4]);
        assert_eq!(old(host), Some(OldRole::SkipHostname));
        // An unknown role: a plain select, no TLS part.
        assert_eq!(old(ssl_verification()), None);
    }

    #[test]
    fn curl_names_for_engines_without_their_own() {
        let k = Field::tls_insecure(["false", "true", "false", "true"]);
        assert_eq!((k.key, k.label), ("insecure", "insecure"));
        assert_eq!(k.tls, Some(TlsRole::SkipVerify { inverted: false }));
        assert!(k.tls_skips("true") && !k.help.is_empty());
        let ca = Field::tls_cacert();
        assert_eq!((ca.key, ca.label), ("cacert", "cacert"));
        assert!(matches!(ca.kind, FieldKind::File) && !ca.help.is_empty());
    }
}
