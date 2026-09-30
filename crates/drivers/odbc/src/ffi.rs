//! The slice of the ODBC C API this crate uses, resolved at runtime from
//! the driver manager (unixODBC's `libodbc`, or `odbc32.dll` on Windows).
//!
//! Nothing here is linked at build time: a DBine binary starts on a machine
//! without any ODBC installed, and only an ODBC connection needs the
//! library. `odbc-sys` (and so `odbc-api`) always links `libodbc`, which
//! would make the whole app fail to launch there; hence this small table
//! of function pointers loaded with `libloading`.
//!
//! Wide (`…W`) entry points take UTF-16 (`SQLWCHAR` = u16), which is what
//! unixODBC and Windows use. iODBC (UTF-32 `wchar_t`) is not supported.

#![allow(non_snake_case, dead_code, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;

pub type Handle = *mut c_void;
pub type SqlReturn = i16;
pub type SqlLen = isize;
pub type SqlULen = usize;

pub const SQL_HANDLE_ENV: i16 = 1;
pub const SQL_HANDLE_DBC: i16 = 2;
pub const SQL_HANDLE_STMT: i16 = 3;

pub const SQL_SUCCESS: SqlReturn = 0;
pub const SQL_SUCCESS_WITH_INFO: SqlReturn = 1;
pub const SQL_NO_DATA: SqlReturn = 100;
pub const SQL_ERROR: SqlReturn = -1;
pub const SQL_INVALID_HANDLE: SqlReturn = -2;

pub const SQL_NTS: i16 = -3;
pub const SQL_NULL_DATA: SqlLen = -1;
pub const SQL_NO_TOTAL: SqlLen = -4;

pub const SQL_ATTR_ODBC_VERSION: i32 = 200;
pub const SQL_OV_ODBC3: usize = 3;
pub const SQL_ATTR_ACCESS_MODE: i32 = 101;
pub const SQL_MODE_READ_ONLY: usize = 1;
pub const SQL_ATTR_LOGIN_TIMEOUT: i32 = 103;
pub const SQL_ATTR_CURRENT_CATALOG: i32 = 109;
pub const SQL_DRIVER_NOPROMPT: u16 = 0;

pub const SQL_FETCH_NEXT: u16 = 1;
pub const SQL_FETCH_FIRST: u16 = 2;

// SQLGetInfo
pub const SQL_DATABASE_NAME: u16 = 16;
pub const SQL_DBMS_NAME: u16 = 17;
pub const SQL_DBMS_VER: u16 = 18;
pub const SQL_SEARCH_PATTERN_ESCAPE: u16 = 14;
pub const SQL_IDENTIFIER_QUOTE_CHAR: u16 = 29;

// SQLStatistics
pub const SQL_INDEX_ALL: u16 = 1;
pub const SQL_QUICK: u16 = 0;

// SQLColAttribute
pub const SQL_DESC_TYPE_NAME: u16 = 14;

// SQLBindParameter
pub const SQL_PARAM_INPUT: i16 = 1;

// C data types.
pub const SQL_C_CHAR: i16 = 1;
pub const SQL_C_WCHAR: i16 = -8;
pub const SQL_C_BINARY: i16 = -2;
pub const SQL_C_DOUBLE: i16 = 8;
pub const SQL_C_BIT: i16 = -7;
pub const SQL_C_TYPE_DATE: i16 = 91;
pub const SQL_C_TYPE_TIMESTAMP: i16 = 93;

// SQL data types.
pub const SQL_CHAR: i16 = 1;
pub const SQL_NUMERIC: i16 = 2;
pub const SQL_DECIMAL: i16 = 3;
pub const SQL_INTEGER: i16 = 4;
pub const SQL_SMALLINT: i16 = 5;
pub const SQL_FLOAT: i16 = 6;
pub const SQL_REAL: i16 = 7;
pub const SQL_DOUBLE: i16 = 8;
pub const SQL_DATETIME: i16 = 9; // ODBC 2 SQL_DATE
pub const SQL_TIMESTAMP_V2: i16 = 11;
pub const SQL_VARCHAR: i16 = 12;
pub const SQL_TYPE_DATE: i16 = 91;
pub const SQL_TYPE_TIMESTAMP: i16 = 93;
pub const SQL_LONGVARCHAR: i16 = -1;
pub const SQL_BINARY: i16 = -2;
pub const SQL_VARBINARY: i16 = -3;
pub const SQL_LONGVARBINARY: i16 = -4;
pub const SQL_BIGINT: i16 = -5;
pub const SQL_TINYINT: i16 = -6;
pub const SQL_BIT: i16 = -7;
pub const SQL_WCHAR: i16 = -8;
pub const SQL_WVARCHAR: i16 = -9;
pub const SQL_WLONGVARCHAR: i16 = -10;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SqlDate {
    pub year: i16,
    pub month: u16,
    pub day: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SqlTimestamp {
    pub year: i16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    /// Nanoseconds.
    pub fraction: u32,
}

macro_rules! api {
    ($($name:ident: fn($($arg:ty),*) -> $ret:ty;)*) => {
        /// Function pointers into the driver manager.
        pub struct Api {
            $(pub $name: unsafe extern "system" fn($($arg),*) -> $ret,)*
            /// Where the driver manager was loaded from.
            pub path: String,
        }

        impl Api {
            fn load(lib: &libloading::Library, path: String) -> Result<Self, String> {
                // SAFETY: the signatures below are the ODBC 3.x prototypes
                // (sql.h / sqlext.h / sqlucode.h).
                unsafe {
                    Ok(Api {
                        $($name: *lib
                            .get::<unsafe extern "system" fn($($arg),*) -> $ret>(concat!(stringify!($name), "\0").as_bytes())
                            .map_err(|e| format!("{path}: falta {}: {e}", stringify!($name)))?,)*
                        path,
                    })
                }
            }
        }
    };
}

api! {
    SQLAllocHandle: fn(i16, Handle, *mut Handle) -> SqlReturn;
    SQLFreeHandle: fn(i16, Handle) -> SqlReturn;
    SQLSetEnvAttr: fn(Handle, i32, *mut c_void, i32) -> SqlReturn;
    SQLSetConnectAttrW: fn(Handle, i32, *mut c_void, i32) -> SqlReturn;
    SQLDriverConnectW: fn(Handle, Handle, *const u16, i16, *mut u16, i16, *mut i16, u16) -> SqlReturn;
    SQLDisconnect: fn(Handle) -> SqlReturn;
    SQLGetInfoW: fn(Handle, u16, *mut c_void, i16, *mut i16) -> SqlReturn;
    SQLExecDirectW: fn(Handle, *const u16, i32) -> SqlReturn;
    SQLBindParameter: fn(Handle, u16, i16, i16, i16, SqlULen, i16, *mut c_void, SqlLen, *mut SqlLen) -> SqlReturn;
    SQLNumResultCols: fn(Handle, *mut i16) -> SqlReturn;
    SQLDescribeColW: fn(Handle, u16, *mut u16, i16, *mut i16, *mut i16, *mut SqlULen, *mut i16, *mut i16) -> SqlReturn;
    SQLColAttributeW: fn(Handle, u16, u16, *mut c_void, i16, *mut i16, *mut SqlLen) -> SqlReturn;
    SQLFetch: fn(Handle) -> SqlReturn;
    SQLGetData: fn(Handle, u16, i16, *mut c_void, SqlLen, *mut SqlLen) -> SqlReturn;
    SQLMoreResults: fn(Handle) -> SqlReturn;
    SQLRowCount: fn(Handle, *mut SqlLen) -> SqlReturn;
    SQLTablesW: fn(Handle, *const u16, i16, *const u16, i16, *const u16, i16, *const u16, i16) -> SqlReturn;
    SQLColumnsW: fn(Handle, *const u16, i16, *const u16, i16, *const u16, i16, *const u16, i16) -> SqlReturn;
    SQLPrimaryKeysW: fn(Handle, *const u16, i16, *const u16, i16, *const u16, i16) -> SqlReturn;
    SQLProceduresW: fn(Handle, *const u16, i16, *const u16, i16, *const u16, i16) -> SqlReturn;
    SQLForeignKeysW: fn(Handle, *const u16, i16, *const u16, i16, *const u16, i16, *const u16, i16, *const u16, i16, *const u16, i16) -> SqlReturn;
    SQLStatisticsW: fn(Handle, *const u16, i16, *const u16, i16, *const u16, i16, u16, u16) -> SqlReturn;
    SQLCancel: fn(Handle) -> SqlReturn;
    SQLGetDiagRecW: fn(i16, Handle, i16, *mut u16, *mut i32, *mut u16, i16, *mut i16) -> SqlReturn;
    SQLDriversW: fn(Handle, u16, *mut u16, i16, *mut i16, *mut u16, i16, *mut i16) -> SqlReturn;
    // Bulk transfer (transfer.rs): block fetches and parameter arrays.
    SQLPrepareW: fn(Handle, *const u16, i32) -> SqlReturn;
    SQLExecute: fn(Handle) -> SqlReturn;
    SQLBindCol: fn(Handle, u16, i16, *mut c_void, SqlLen, *mut SqlLen) -> SqlReturn;
    SQLSetStmtAttrW: fn(Handle, i32, *mut c_void, i32) -> SqlReturn;
    SQLFreeStmt: fn(Handle, u16) -> SqlReturn;
    SQLEndTran: fn(i16, Handle, i16) -> SqlReturn;
}

/// Where the driver manager usually lives, in search order.
fn candidates(explicit: Option<&str>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit.filter(|p| !p.trim().is_empty()) {
        v.push(p.trim().into());
    }
    if let Ok(p) = std::env::var("DBINE_ODBC_LIB") {
        if !p.trim().is_empty() {
            v.push(p.trim().into());
        }
    }
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["odbc32.dll"]
    } else if cfg!(target_os = "macos") {
        &["libodbc.2.dylib", "libodbc.dylib"]
    } else {
        &["libodbc.so.2", "libodbc.so.1", "libodbc.so"]
    };
    let dirs: &[&str] = if cfg!(target_os = "macos") {
        &["/opt/homebrew/lib", "/usr/local/lib", "/opt/local/lib", "/usr/lib"]
    } else if cfg!(target_os = "windows") {
        &[]
    } else {
        &["/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu", "/usr/lib64", "/usr/lib", "/usr/local/lib"]
    };
    for d in dirs {
        for n in names {
            v.push(PathBuf::from(d).join(n));
        }
    }
    // Last, the bare names: the platform's own search (PATH on Windows,
    // ld.so.cache / DYLD_* paths elsewhere).
    v.extend(names.iter().map(PathBuf::from));
    v
}

static LOADED: Mutex<Option<&'static Api>> = Mutex::new(None);

/// The driver manager, loaded on first use. A failure isn't cached, so the
/// user can install unixODBC or fix the path and retry without restarting.
/// Once loaded, the library stays for the life of the process (the first
/// successful path wins).
pub fn api(explicit: Option<&str>) -> Result<&'static Api, String> {
    let mut slot = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(api) = *slot {
        return Ok(api);
    }
    let mut tried = Vec::new();
    for path in candidates(explicit) {
        let shown = path.display().to_string();
        // Absolute paths that don't exist: skip without a dlopen.
        if path.is_absolute() && !path.exists() {
            tried.push(shown);
            continue;
        }
        // SAFETY: loading a shared library runs its initializers; the ODBC
        // driver manager's are benign.
        match unsafe { libloading::Library::new(&path) } {
            Ok(lib) => {
                let api = Api::load(&lib, shown)?;
                // Keep the library mapped forever: the pointers above point
                // into it.
                std::mem::forget(lib);
                let api: &'static Api = Box::leak(Box::new(api));
                *slot = Some(api);
                return Ok(api);
            }
            Err(_) => tried.push(shown),
        }
    }
    Err(format!(
        "No se encontró el administrador de controladores ODBC. {} Si ya está instalado, indicá la ruta de la biblioteca en la opción «Biblioteca ODBC» o en la variable de entorno DBINE_ODBC_LIB. Se buscó en: {}.",
        install_hint(),
        tried.join(", ")
    ))
}

fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Instalá unixODBC (por ejemplo con `brew install unixodbc`) y el driver ODBC del fabricante."
    } else if cfg!(target_os = "windows") {
        "Windows trae el administrador ODBC (odbc32.dll); instalá el driver ODBC del fabricante."
    } else {
        "Instalá unixODBC (paquete `unixodbc` o `unixODBC`) y el driver ODBC del fabricante."
    }
}
