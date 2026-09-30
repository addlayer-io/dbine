//! The subset of Avatica's protobuf messages (common.proto, requests.proto,
//! responses.proto in apache/calcite-avatica) the driver uses, written by hand
//! with prost's derive. Unknown fields are skipped when decoding, so only the
//! fields we read are declared.

use prost::Message;
use std::collections::HashMap;

pub const REQ: &str = "org.apache.calcite.avatica.proto.Requests$";
pub const RESP: &str = "org.apache.calcite.avatica.proto.Responses$";

#[derive(Clone, PartialEq, Message)]
pub struct WireMessage {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bytes = "vec", tag = "2")]
    pub wrapped_message: Vec<u8>,
}

// Requests.

#[derive(Clone, PartialEq, Message)]
pub struct OpenConnectionRequest {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(map = "string, string", tag = "2")]
    pub info: HashMap<String, String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionIdRequest {
    #[prost(string, tag = "1")]
    pub connection_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionProperties {
    #[prost(bool, tag = "1")]
    pub is_dirty: bool,
    #[prost(bool, tag = "2")]
    pub auto_commit: bool,
    #[prost(bool, tag = "7")]
    pub has_auto_commit: bool,
    #[prost(bool, tag = "3")]
    pub read_only: bool,
    #[prost(bool, tag = "8")]
    pub has_read_only: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionSyncRequest {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(message, optional, tag = "2")]
    pub conn_props: Option<ConnectionProperties>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CloseStatementRequest {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(uint32, tag = "2")]
    pub statement_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct PrepareAndExecuteRequest {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(string, tag = "2")]
    pub sql: String,
    #[prost(uint64, tag = "3")]
    pub max_row_count: u64,
    #[prost(uint32, tag = "4")]
    pub statement_id: u32,
    #[prost(int64, tag = "5")]
    pub max_rows_total: i64,
    #[prost(int32, tag = "6")]
    pub first_frame_max_size: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct FetchRequest {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(uint32, tag = "2")]
    pub statement_id: u32,
    #[prost(uint64, tag = "3")]
    pub offset: u64,
    #[prost(uint32, tag = "4")]
    pub fetch_max_row_count: u32,
    #[prost(int32, tag = "5")]
    pub frame_max_size: i32,
}

/// Avatica metadata: `DatabaseMetaData.getTables`.
#[derive(Clone, PartialEq, Message)]
pub struct TablesRequest {
    #[prost(string, tag = "1")]
    pub catalog: String,
    #[prost(string, tag = "2")]
    pub schema_pattern: String,
    #[prost(string, tag = "3")]
    pub table_name_pattern: String,
    #[prost(string, repeated, tag = "4")]
    pub type_list: Vec<String>,
    #[prost(bool, tag = "6")]
    pub has_type_list: bool,
    #[prost(string, tag = "7")]
    pub connection_id: String,
}

/// Avatica metadata: `DatabaseMetaData.getColumns`. Empty strings are nulls
/// (no filter).
#[derive(Clone, PartialEq, Message)]
pub struct ColumnsRequest {
    #[prost(string, tag = "1")]
    pub catalog: String,
    #[prost(string, tag = "2")]
    pub schema_pattern: String,
    #[prost(string, tag = "3")]
    pub table_name_pattern: String,
    #[prost(string, tag = "4")]
    pub column_name_pattern: String,
    #[prost(string, tag = "5")]
    pub connection_id: String,
}

// Responses.

#[derive(Clone, PartialEq, Message)]
pub struct ErrorResponse {
    #[prost(string, repeated, tag = "1")]
    pub exceptions: Vec<String>,
    #[prost(string, tag = "2")]
    pub error_message: String,
    #[prost(uint32, tag = "4")]
    pub error_code: u32,
    #[prost(string, tag = "5")]
    pub sql_state: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CreateStatementResponse {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(uint32, tag = "2")]
    pub statement_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecuteResponse {
    #[prost(message, repeated, tag = "1")]
    pub results: Vec<ResultSetResponse>,
    #[prost(bool, tag = "2")]
    pub missing_statement: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResultSetResponse {
    #[prost(string, tag = "1")]
    pub connection_id: String,
    #[prost(uint32, tag = "2")]
    pub statement_id: u32,
    #[prost(message, optional, tag = "4")]
    pub signature: Option<Signature>,
    #[prost(message, optional, tag = "5")]
    pub first_frame: Option<Frame>,
    #[prost(uint64, tag = "6")]
    pub update_count: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Signature {
    #[prost(message, repeated, tag = "1")]
    pub columns: Vec<ColumnMetaData>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ColumnMetaData {
    #[prost(string, tag = "9")]
    pub label: String,
    #[prost(string, tag = "10")]
    pub column_name: String,
    #[prost(message, optional, tag = "20")]
    pub r#type: Option<AvaticaType>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AvaticaType {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(int32, tag = "3")]
    pub rep: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Frame {
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    #[prost(bool, tag = "2")]
    pub done: bool,
    #[prost(message, repeated, tag = "3")]
    pub rows: Vec<Row>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Row {
    #[prost(message, repeated, tag = "1")]
    pub value: Vec<ColumnValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ColumnValue {
    /// Deprecated: servers before Avatica 1.8 put the scalar here.
    #[prost(message, repeated, tag = "1")]
    pub value: Vec<TypedValue>,
    #[prost(message, repeated, tag = "2")]
    pub array_value: Vec<TypedValue>,
    #[prost(bool, tag = "3")]
    pub has_array_value: bool,
    #[prost(message, optional, tag = "4")]
    pub scalar_value: Option<TypedValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TypedValue {
    #[prost(int32, tag = "1")]
    pub r#type: i32,
    #[prost(bool, tag = "2")]
    pub bool_value: bool,
    #[prost(string, tag = "3")]
    pub string_value: String,
    #[prost(sint64, tag = "4")]
    pub number_value: i64,
    #[prost(bytes = "vec", tag = "5")]
    pub bytes_value: Vec<u8>,
    #[prost(double, tag = "6")]
    pub double_value: f64,
    #[prost(bool, tag = "7")]
    pub null: bool,
    #[prost(message, repeated, tag = "8")]
    pub array_value: Vec<TypedValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FetchResponse {
    #[prost(message, optional, tag = "1")]
    pub frame: Option<Frame>,
    #[prost(bool, tag = "2")]
    pub missing_statement: bool,
    #[prost(bool, tag = "3")]
    pub missing_results: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct DatabaseProperty {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DatabasePropertyElement {
    #[prost(message, optional, tag = "1")]
    pub key: Option<DatabaseProperty>,
    #[prost(message, optional, tag = "2")]
    pub value: Option<TypedValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DatabasePropertyResponse {
    #[prost(message, repeated, tag = "1")]
    pub props: Vec<DatabasePropertyElement>,
}

/// Avatica's `Rep` enum values the driver cares about.
pub mod rep {
    pub const BOOLEAN: i32 = 8;
    pub const PRIMITIVE_BOOLEAN: i32 = 0;
    pub const PRIMITIVE_FLOAT: i32 = 6;
    pub const PRIMITIVE_DOUBLE: i32 = 7;
    pub const FLOAT: i32 = 14;
    pub const DOUBLE: i32 = 15;
    pub const JAVA_SQL_TIME: i32 = 16;
    pub const JAVA_SQL_TIMESTAMP: i32 = 17;
    pub const JAVA_SQL_DATE: i32 = 18;
    pub const JAVA_UTIL_DATE: i32 = 19;
    pub const BYTE_STRING: i32 = 20;
    pub const STRING: i32 = 21;
    pub const BIG_INTEGER: i32 = 25;
    pub const BIG_DECIMAL: i32 = 26;
    pub const ARRAY: i32 = 27;
    pub const NULL: i32 = 24;
}
