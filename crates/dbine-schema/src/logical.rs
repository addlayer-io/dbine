//! The engine-neutral type model. Every dialect parses its native type
//! names into a [`LogicalType`] and renders a [`LogicalType`] back into its
//! own names; conversion between two engines is parse-then-render.
//!
//! The model keeps what matters for moving data without surprises: sizes,
//! precision, signedness, time zones, character sets (unicode or not).
//! Anything a dialect can't classify stays as [`LogicalType::Other`] with
//! its native spelling, and the converter reports it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogicalType {
    Bool,
    /// Whole number stored in `bytes` bytes (1, 2, 3, 4, 8 or 16).
    Int { bytes: u8, unsigned: bool },
    /// Exact number. `None` precision: the engine's unbounded / default
    /// numeric (PostgreSQL `numeric`, Oracle `NUMBER`).
    Decimal { precision: Option<u32>, scale: Option<u32> },
    /// Binary floating point: 4 bytes (`real`) or 8 (`double`).
    Float { bytes: u8 },
    /// Currency types (SQL Server `money`, PostgreSQL `money`).
    Money,
    /// Fixed-length text.
    Char { len: Option<u32>, unicode: bool },
    /// Variable-length text with a limit.
    Varchar { len: Option<u32>, unicode: bool },
    /// Unbounded text (`text`, `clob`, `nvarchar(max)`…).
    Text { unicode: bool },
    /// Fixed-length bytes.
    Binary { len: Option<u32> },
    /// Variable-length bytes with a limit.
    Varbinary { len: Option<u32> },
    /// Unbounded bytes (`bytea`, `blob`, `varbinary(max)`…).
    Blob,
    /// Fixed-length bit string.
    Bit { len: Option<u32> },
    Date,
    /// Time of day; `precision` = fractional-second digits.
    Time { precision: Option<u8>, tz: bool },
    /// Date and time; `tz` = the value carries or is normalized by a time
    /// zone (`timestamptz`, `datetimeoffset`, `TIMESTAMP WITH TIME ZONE`).
    Timestamp { precision: Option<u8>, tz: bool },
    Interval,
    Year,
    Uuid,
    /// JSON document; `binary` for the parsed/indexed kind (`jsonb`).
    Json { binary: bool },
    Xml,
    /// One value out of a fixed list.
    Enum { values: Vec<String> },
    /// Any subset of a fixed list (MySQL `SET`).
    Set { values: Vec<String> },
    Array { of: Box<LogicalType> },
    /// Key → value map (ClickHouse `Map`, Cassandra `map`).
    Map { key: Box<LogicalType>, value: Box<LogicalType> },
    /// Spatial value; `kind` = point, polygon… when the engine says.
    Geometry { kind: Option<String>, srid: Option<u32>, geography: bool },
    /// IP address or network (`inet`, `cidr`, `IPv4`, `IPv6`).
    Inet,
    MacAddr,
    /// Row identifier / version stamp (`rowversion`, `ROWID`).
    RowVersion,
    /// Not classified: the native spelling, kept as is.
    Other { native: String },
}

impl LogicalType {
    pub fn int(bytes: u8) -> Self {
        Self::Int { bytes, unsigned: false }
    }

    /// Text of any kind.
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Char { .. } | Self::Varchar { .. } | Self::Text { .. })
    }

    /// Short Spanish description for reports ("entero de 8 bytes").
    pub fn describe(&self) -> String {
        match self {
            Self::Bool => "booleano".into(),
            Self::Int { bytes, unsigned } => {
                format!("entero {}de {bytes} bytes", if *unsigned { "sin signo " } else { "" })
            }
            Self::Decimal { precision: Some(p), scale } => format!("decimal({p}, {})", scale.unwrap_or(0)),
            Self::Decimal { .. } => "decimal sin precisión fija".into(),
            Self::Float { bytes } => format!("coma flotante de {bytes} bytes"),
            Self::Money => "moneda".into(),
            Self::Char { len, .. } => format!("texto fijo de {}", len.map_or("1".into(), |l| l.to_string())),
            Self::Varchar { len: Some(l), .. } => format!("texto de hasta {l}"),
            Self::Varchar { len: None, .. } | Self::Text { .. } => "texto sin límite".into(),
            Self::Binary { .. } | Self::Varbinary { .. } | Self::Blob => "binario".into(),
            Self::Bit { .. } => "bits".into(),
            Self::Date => "fecha".into(),
            Self::Time { tz, .. } => if *tz { "hora con zona" } else { "hora" }.into(),
            Self::Timestamp { tz, .. } => if *tz { "fecha y hora con zona" } else { "fecha y hora" }.into(),
            Self::Interval => "intervalo".into(),
            Self::Year => "año".into(),
            Self::Uuid => "UUID".into(),
            Self::Json { .. } => "JSON".into(),
            Self::Xml => "XML".into(),
            Self::Enum { .. } => "enumerado".into(),
            Self::Set { .. } => "conjunto".into(),
            Self::Array { of } => format!("arreglo de {}", of.describe()),
            Self::Map { .. } => "mapa".into(),
            Self::Geometry { .. } => "espacial".into(),
            Self::Inet => "dirección IP".into(),
            Self::MacAddr => "dirección MAC".into(),
            Self::RowVersion => "versión de fila".into(),
            Self::Other { native } => format!("«{native}»"),
        }
    }

    /// Smallest signed integer size (in bytes) that holds every value of an
    /// integer of `bytes` bytes, signed or not.
    pub fn signed_bytes_for(bytes: u8, unsigned: bool) -> u8 {
        if !unsigned {
            return bytes;
        }
        match bytes {
            1 => 2,
            2 => 4,
            3 => 4,
            4 => 8,
            _ => 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_widening() {
        assert_eq!(LogicalType::signed_bytes_for(4, true), 8);
        assert_eq!(LogicalType::signed_bytes_for(4, false), 4);
        assert_eq!(LogicalType::signed_bytes_for(8, true), 16);
    }

    #[test]
    fn serializes_tagged() {
        let t = LogicalType::Varchar { len: Some(10), unicode: true };
        assert_eq!(serde_json::to_string(&t).unwrap(), r#"{"type":"varchar","len":10,"unicode":true}"#);
    }
}
