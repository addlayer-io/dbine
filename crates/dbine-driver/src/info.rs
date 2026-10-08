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
}

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
    Select(#[serde(deserialize_with = "crate::serde_static::pairs")] Vec<(&'static str, &'static str)>),
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
        self.when = Some(FieldWhen { key, values: values.to_vec() });
        self
    }

    // The usual ones.
    pub fn host() -> Self {
        Self::new("host", "Servidor", FieldKind::Text).required().placeholder("localhost")
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
    pub fn encrypt() -> Self {
        Self::new("encrypt", "Cifrar la conexión (TLS)", FieldKind::Bool).ssl()
    }
    pub fn trust_cert() -> Self {
        Self::new("trust_server_certificate", "Confiar en el certificado del servidor", FieldKind::Bool).ssl()
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
    pub const fn new(id: &'static str, label: &'static str, has_columns: bool, browsable: bool, has_definition: bool) -> Self {
        Self { id, label, has_columns, browsable, has_definition }
    }
    pub const fn tables() -> Self {
        Self::new(kinds::TABLE, "Tablas", true, true, true)
    }
    pub const fn views() -> Self {
        Self::new(kinds::VIEW, "Vistas", true, true, true)
    }
    pub const fn materialized_views() -> Self {
        Self::new(kinds::MATERIALIZED_VIEW, "Vistas materializadas", true, true, true)
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
