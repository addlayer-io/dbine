//! Thin safe layer over the raw function table: handles that free
//! themselves, diagnostics, and reading cells as JSON.

use crate::ffi::*;
use dbine_driver::{json_bytes, json_f64, json_i64, json_u64, Error, Result};
use serde_json::Value;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// A raw handle that can cross threads. ODBC 3 handles are thread-safe at
/// the driver-manager level; we still serialize all use of a connection
/// behind a mutex, except `SQLCancel`, which ODBC allows from another thread.
#[derive(Clone, Copy)]
pub struct H(pub Handle);
// SAFETY: see above.
unsafe impl Send for H {}
unsafe impl Sync for H {}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn from_wide(buf: &[u16]) -> String {
    String::from_utf16_lossy(buf)
}

fn ok(rc: SqlReturn) -> bool {
    rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub state: String,
    pub message: String,
}

/// Every diagnostic record on a handle.
pub fn diags(api: &Api, htype: i16, h: Handle) -> Vec<Diag> {
    let mut out = Vec::new();
    for rec in 1..=32i16 {
        let mut state = [0u16; 6];
        let mut native = 0i32;
        let mut msg = vec![0u16; 2048];
        let mut len = 0i16;
        // SAFETY: buffers sized as declared.
        let rc = unsafe {
            (api.SQLGetDiagRecW)(htype, h, rec, state.as_mut_ptr(), &mut native, msg.as_mut_ptr(), msg.len() as i16, &mut len)
        };
        if !ok(rc) {
            break;
        }
        let len = (len.max(0) as usize).min(msg.len() - 1);
        out.push(Diag { state: from_wide(&state[..5]), message: clean_message(&from_wide(&msg[..len])) });
    }
    out
}

/// Drops the `[vendor][driver][server]` prefixes ODBC stacks on messages.
pub fn clean_message(m: &str) -> String {
    let mut s = m.trim();
    while s.starts_with('[') {
        match s.find(']') {
            Some(i) => s = s[i + 1..].trim_start(),
            None => break,
        }
    }
    if s.is_empty() {
        m.trim().to_string()
    } else {
        s.to_string()
    }
}

fn join(d: &[Diag]) -> String {
    let msgs: Vec<&str> = d.iter().map(|d| d.message.as_str()).filter(|m| !m.is_empty()).collect();
    if msgs.is_empty() {
        "error ODBC sin diagnóstico".into()
    } else {
        msgs.join("\n")
    }
}

/// Error while connecting.
pub fn connect_error(d: &[Diag]) -> Error {
    let msg = join(d);
    if d.iter().any(|d| d.state == "28000") {
        Error::AuthFailed(msg)
    } else if d.iter().any(|d| matches!(d.state.as_str(), "IM002" | "IM003") || d.message.contains("Can't open lib")) {
        Error::Connect(format!(
            "{msg}\nEl driver ODBC indicado no está instalado o no está registrado en odbcinst.ini. Drivers instalados: {}.",
            installed_drivers_text()
        ))
    } else {
        Error::Connect(msg)
    }
}

fn installed_drivers_text() -> String {
    match crate::installed_odbc_drivers() {
        Ok(v) if !v.is_empty() => v.join(", "),
        Ok(_) => "ninguno".into(),
        Err(_) => "no se pudieron listar".into(),
    }
}

/// Error from a statement.
pub fn query_error(d: &[Diag]) -> Error {
    if d.iter().any(|d| d.state == "HY008") {
        Error::Cancelled
    } else {
        Error::Query(join(d))
    }
}

pub struct Env {
    pub api: &'static Api,
    pub h: H,
}

impl Env {
    pub fn new(api: &'static Api) -> Result<Env> {
        let mut h: Handle = ptr::null_mut();
        // SAFETY: plain ODBC calls with valid out-pointers.
        unsafe {
            if !ok((api.SQLAllocHandle)(SQL_HANDLE_ENV, ptr::null_mut(), &mut h)) {
                return Err(Error::Connect("no se pudo crear el entorno ODBC".into()));
            }
            let env = Env { api, h: H(h) };
            if !ok((api.SQLSetEnvAttr)(h, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3 as *mut c_void, 0)) {
                return Err(connect_error(&diags(api, SQL_HANDLE_ENV, h)));
            }
            Ok(env)
        }
    }

    /// Installed drivers (odbcinst.ini / the Windows registry).
    pub fn drivers(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut dir = SQL_FETCH_FIRST;
        loop {
            let mut desc = vec![0u16; 512];
            let mut dlen = 0i16;
            let mut attrs = vec![0u16; 4096];
            let mut alen = 0i16;
            // SAFETY: buffers sized as declared.
            let rc = unsafe {
                (self.api.SQLDriversW)(
                    self.h.0,
                    dir,
                    desc.as_mut_ptr(),
                    desc.len() as i16,
                    &mut dlen,
                    attrs.as_mut_ptr(),
                    attrs.len() as i16,
                    &mut alen,
                )
            };
            if !ok(rc) {
                break;
            }
            let n = (dlen.max(0) as usize).min(desc.len() - 1);
            out.push(from_wide(&desc[..n]));
            dir = SQL_FETCH_NEXT;
        }
        out
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and freed once.
        unsafe { (self.api.SQLFreeHandle)(SQL_HANDLE_ENV, self.h.0) };
    }
}

pub struct Conn {
    pub api: &'static Api,
    pub dbc: H,
    connected: bool,
    // Dropped after the connection handle (see Drop).
    _env: Env,
}

pub struct ConnectOptions<'a> {
    pub conn_str: &'a str,
    pub login_timeout_secs: usize,
    pub read_only: bool,
    /// Switch to this catalog after connecting (SQL_ATTR_CURRENT_CATALOG).
    pub catalog: Option<&'a str>,
}

impl Conn {
    pub fn open(api: &'static Api, o: &ConnectOptions) -> Result<Conn> {
        let env = Env::new(api)?;
        let mut h: Handle = ptr::null_mut();
        // SAFETY: plain ODBC calls; string buffers outlive the calls.
        unsafe {
            if !ok((api.SQLAllocHandle)(SQL_HANDLE_DBC, env.h.0, &mut h)) {
                return Err(connect_error(&diags(api, SQL_HANDLE_ENV, env.h.0)));
            }
            let mut conn = Conn { api, dbc: H(h), connected: false, _env: env };
            (api.SQLSetConnectAttrW)(h, SQL_ATTR_LOGIN_TIMEOUT, o.login_timeout_secs as *mut c_void, 0);
            let cs = wide(o.conn_str);
            let rc = (api.SQLDriverConnectW)(
                h,
                ptr::null_mut(),
                cs.as_ptr(),
                cs.len() as i16,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                SQL_DRIVER_NOPROMPT,
            );
            if !ok(rc) {
                return Err(connect_error(&diags(api, SQL_HANDLE_DBC, h)));
            }
            conn.connected = true;
            if o.read_only {
                // A hint for many drivers, enforced by some (DB2, Informix…);
                // the app's keyword guard covers the rest.
                let rc = (api.SQLSetConnectAttrW)(h, SQL_ATTR_ACCESS_MODE, SQL_MODE_READ_ONLY as *mut c_void, 0);
                if !ok(rc) {
                    tracing::debug!("odbc: the driver ignored SQL_ATTR_ACCESS_MODE read-only");
                }
            }
            if let Some(cat) = o.catalog.filter(|c| !c.is_empty()) {
                let w = wide(cat);
                let rc = (api.SQLSetConnectAttrW)(
                    h,
                    SQL_ATTR_CURRENT_CATALOG,
                    w.as_ptr() as *mut c_void,
                    (w.len() * 2) as i32,
                );
                if !ok(rc) {
                    return Err(connect_error(&diags(api, SQL_HANDLE_DBC, h)));
                }
            }
            Ok(conn)
        }
    }

    /// A string from SQLGetInfo, empty when the driver doesn't say.
    pub fn info_string(&self, what: u16) -> String {
        let mut buf = vec![0u16; 512];
        let mut len = 0i16;
        // SAFETY: buffer sized as declared (length in bytes).
        let rc = unsafe {
            (self.api.SQLGetInfoW)(self.dbc.0, what, buf.as_mut_ptr() as *mut c_void, (buf.len() * 2) as i16, &mut len)
        };
        if !ok(rc) {
            return String::new();
        }
        let n = ((len.max(0) as usize) / 2).min(buf.len() - 1);
        from_wide(&buf[..n])
    }

    pub fn stmt<'a>(&'a self, shared: &'a StmtSlot) -> Result<Stmt<'a>> {
        let mut h: Handle = ptr::null_mut();
        // SAFETY: valid connection handle and out-pointer.
        let rc = unsafe { (self.api.SQLAllocHandle)(SQL_HANDLE_STMT, self.dbc.0, &mut h) };
        if !ok(rc) {
            return Err(query_error(&diags(self.api, SQL_HANDLE_DBC, self.dbc.0)));
        }
        *shared.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(H(h));
        Ok(Stmt { api: self.api, h: H(h), slot: shared })
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // SAFETY: handles are ours and freed once, the env after the dbc.
        unsafe {
            if self.connected {
                (self.api.SQLDisconnect)(self.dbc.0);
            }
            (self.api.SQLFreeHandle)(SQL_HANDLE_DBC, self.dbc.0);
        }
    }
}

/// The statement in flight, reachable from the interrupter, and whether a
/// cancel was asked for.
#[derive(Default)]
pub struct StmtSlot {
    current: Mutex<Option<H>>,
    pub cancelled: AtomicBool,
}

impl StmtSlot {
    /// `SQLCancel` on the running statement, from any thread. The slot's
    /// lock keeps the statement from being freed meanwhile.
    pub fn cancel(&self, api: &Api) {
        self.cancelled.store(true, Ordering::SeqCst);
        let cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = *cur {
            // SAFETY: the handle is alive while we hold the lock.
            unsafe { (api.SQLCancel)(h.0) };
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// An optional wide-string argument of a catalog function.
struct WArg(Option<Vec<u16>>);

impl WArg {
    fn new(s: Option<&str>) -> Self {
        WArg(s.map(wide))
    }
    fn ptr(&self) -> *const u16 {
        static EMPTY: [u16; 1] = [0];
        self.0.as_ref().map_or(ptr::null(), |v| if v.is_empty() { EMPTY.as_ptr() } else { v.as_ptr() })
    }
    fn len(&self) -> i16 {
        self.0.as_ref().map_or(0, |v| v.len() as i16)
    }
}

pub struct ColDesc {
    pub name: String,
    pub sql_type: i16,
    pub type_name: String,
}

/// How a column is read and turned into JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Float,
    Bit,
    Decimal,
    Date,
    Timestamp,
    Binary,
    Text,
}

pub fn kind_of(sql_type: i16) -> Kind {
    match sql_type {
        SQL_INTEGER | SQL_SMALLINT | SQL_TINYINT | SQL_BIGINT => Kind::Int,
        SQL_FLOAT | SQL_REAL | SQL_DOUBLE => Kind::Float,
        SQL_BIT => Kind::Bit,
        SQL_NUMERIC | SQL_DECIMAL => Kind::Decimal,
        SQL_TYPE_DATE | SQL_DATETIME => Kind::Date,
        SQL_TYPE_TIMESTAMP | SQL_TIMESTAMP_V2 => Kind::Timestamp,
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => Kind::Binary,
        // Chars, times, intervals, GUIDs, XML and vendor types: as text.
        _ => Kind::Text,
    }
}

/// Integers are read as text: covers unsigned 64-bit values too.
pub fn int_value(s: &str) -> Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        json_i64(i)
    } else if let Ok(u) = t.parse::<u64>() {
        json_u64(u)
    } else {
        Value::String(t.to_string())
    }
}

/// Some drivers print decimals without the leading zero (`.50`).
pub fn decimal_value(s: &str) -> Value {
    let t = s.trim();
    let fixed = if let Some(r) = t.strip_prefix("-.") {
        format!("-0.{r}")
    } else if let Some(r) = t.strip_prefix('.') {
        format!("0.{r}")
    } else {
        t.to_string()
    };
    Value::String(fixed)
}

pub fn fmt_date(d: &SqlDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

pub fn fmt_timestamp(t: &SqlTimestamp) -> String {
    let mut s = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
    if t.fraction > 0 {
        let f = format!("{:09}", t.fraction);
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    s
}

/// Longest text cell kept (in UTF-16 units); the rest isn't read.
const MAX_TEXT: usize = 8 * 1024 * 1024;

pub struct Stmt<'a> {
    api: &'static Api,
    h: H,
    slot: &'a StmtSlot,
}

impl Drop for Stmt<'_> {
    fn drop(&mut self) {
        let mut cur = self.slot.current.lock().unwrap_or_else(|e| e.into_inner());
        *cur = None;
        // SAFETY: freed once, with the slot cleared under its lock so a
        // concurrent SQLCancel can't see a dangling handle.
        unsafe { (self.api.SQLFreeHandle)(SQL_HANDLE_STMT, self.h.0) };
    }
}

impl Stmt<'_> {
    /// The raw statement handle (for transfer.rs's bound buffers).
    pub fn raw(&self) -> Handle {
        self.h.0
    }

    /// A cancel was asked for on this statement's session.
    pub fn slot_cancelled(&self) -> bool {
        self.slot.is_cancelled()
    }

    pub fn err(&self) -> Error {
        if self.slot.is_cancelled() {
            return Error::Cancelled;
        }
        query_error(&diags(self.api, SQL_HANDLE_STMT, self.h.0))
    }

    /// Informational messages (PRINT, warnings) left on the statement.
    pub fn messages(&self) -> Vec<String> {
        diags(self.api, SQL_HANDLE_STMT, self.h.0)
            .into_iter()
            .filter(|d| d.state != "01004" && !d.message.is_empty())
            .map(|d| d.message)
            .collect()
    }

    /// `SQLExecDirect`. Returns false when the statement affected no rows
    /// (SQL_NO_DATA) and whether the driver left messages.
    pub fn exec(&self, sql: &str) -> Result<SqlReturn> {
        let w = wide(sql);
        // SAFETY: the text outlives the call.
        let rc = unsafe { (self.api.SQLExecDirectW)(self.h.0, w.as_ptr(), w.len() as i32) };
        if rc == SQL_ERROR || rc == SQL_INVALID_HANDLE {
            return Err(self.err());
        }
        Ok(rc)
    }

    /// Execute with text parameters bound in order.
    pub fn exec_params(&self, sql: &str, params: &[&str]) -> Result<SqlReturn> {
        let bufs: Vec<Vec<u16>> = params.iter().map(|p| wide(p)).collect();
        let mut lens: Vec<SqlLen> = bufs.iter().map(|b| (b.len() * 2) as SqlLen).collect();
        for (i, b) in bufs.iter().enumerate() {
            // SAFETY: `bufs` and `lens` outlive the execution below.
            let rc = unsafe {
                (self.api.SQLBindParameter)(
                    self.h.0,
                    (i + 1) as u16,
                    SQL_PARAM_INPUT,
                    SQL_C_WCHAR,
                    SQL_WVARCHAR,
                    b.len().max(1),
                    0,
                    b.as_ptr() as *mut c_void,
                    (b.len() * 2) as SqlLen,
                    &mut lens[i],
                )
            };
            if !ok(rc) {
                return Err(self.err());
            }
        }
        let rc = self.exec(sql);
        drop(bufs);
        rc
    }

    pub fn num_cols(&self) -> Result<u16> {
        let mut n = 0i16;
        // SAFETY: valid out-pointer.
        let rc = unsafe { (self.api.SQLNumResultCols)(self.h.0, &mut n) };
        if !ok(rc) {
            return Err(self.err());
        }
        Ok(n.max(0) as u16)
    }

    pub fn describe(&self, ncols: u16) -> Result<Vec<ColDesc>> {
        let mut out = Vec::with_capacity(ncols as usize);
        for col in 1..=ncols {
            let mut name = vec![0u16; 512];
            let mut name_len = 0i16;
            let mut data_type = 0i16;
            let mut size: SqlULen = 0;
            let mut digits = 0i16;
            let mut nullable = 0i16;
            // SAFETY: buffers sized as declared.
            let rc = unsafe {
                (self.api.SQLDescribeColW)(
                    self.h.0,
                    col,
                    name.as_mut_ptr(),
                    name.len() as i16,
                    &mut name_len,
                    &mut data_type,
                    &mut size,
                    &mut digits,
                    &mut nullable,
                )
            };
            if !ok(rc) {
                return Err(self.err());
            }
            let n = (name_len.max(0) as usize).min(name.len() - 1);
            let mut tbuf = vec![0u16; 256];
            let mut tlen = 0i16;
            let mut num: SqlLen = 0;
            // SAFETY: buffer length in bytes.
            let rc = unsafe {
                (self.api.SQLColAttributeW)(
                    self.h.0,
                    col,
                    SQL_DESC_TYPE_NAME,
                    tbuf.as_mut_ptr() as *mut c_void,
                    (tbuf.len() * 2) as i16,
                    &mut tlen,
                    &mut num,
                )
            };
            let type_name = if ok(rc) {
                from_wide(&tbuf[..((tlen.max(0) as usize) / 2).min(tbuf.len() - 1)])
            } else {
                String::new()
            };
            out.push(ColDesc { name: from_wide(&name[..n]), sql_type: data_type, type_name });
        }
        Ok(out)
    }

    /// Next row; false at the end.
    pub fn fetch(&self) -> Result<bool> {
        if self.slot.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // SAFETY: valid statement handle.
        let rc = unsafe { (self.api.SQLFetch)(self.h.0) };
        match rc {
            SQL_NO_DATA => Ok(false),
            rc if ok(rc) => Ok(true),
            _ => Err(self.err()),
        }
    }

    /// Next result set; false when there are no more.
    pub fn more_results(&self) -> Result<bool> {
        // SAFETY: valid statement handle.
        let rc = unsafe { (self.api.SQLMoreResults)(self.h.0) };
        match rc {
            SQL_NO_DATA => Ok(false),
            rc if ok(rc) => Ok(true),
            _ => Err(self.err()),
        }
    }

    pub fn row_count(&self) -> Option<u64> {
        let mut n: SqlLen = -1;
        // SAFETY: valid out-pointer.
        let rc = unsafe { (self.api.SQLRowCount)(self.h.0, &mut n) };
        (ok(rc) && n >= 0).then_some(n as u64)
    }

    /// A fixed-size value; None when NULL.
    fn get_fixed<T: Default>(&self, col: u16, c_type: i16) -> Result<Option<T>> {
        let mut v = T::default();
        let mut ind: SqlLen = 0;
        // SAFETY: `v` is a plain C struct/number of the declared size.
        let rc = unsafe {
            (self.api.SQLGetData)(
                self.h.0,
                col,
                c_type,
                &mut v as *mut T as *mut c_void,
                std::mem::size_of::<T>() as SqlLen,
                &mut ind,
            )
        };
        if !ok(rc) {
            return Err(self.err());
        }
        Ok((ind != SQL_NULL_DATA).then_some(v))
    }

    /// A cell as text, read in chunks; None when NULL.
    pub fn get_text(&self, col: u16) -> Result<Option<String>> {
        let mut buf = vec![0u16; 4096];
        let mut out: Vec<u16> = Vec::new();
        loop {
            let mut ind: SqlLen = 0;
            let cap = buf.len() * 2;
            // SAFETY: buffer length in bytes.
            let rc = unsafe {
                (self.api.SQLGetData)(self.h.0, col, SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, cap as SqlLen, &mut ind)
            };
            if rc == SQL_NO_DATA {
                break;
            }
            if !ok(rc) {
                return Err(self.err());
            }
            if ind == SQL_NULL_DATA {
                return Ok(None);
            }
            let whole = ind != SQL_NO_TOTAL && (ind as usize) < cap;
            let bytes = if whole { ind as usize } else { cap - 2 };
            out.extend_from_slice(&buf[..bytes / 2]);
            if rc == SQL_SUCCESS || whole || out.len() >= MAX_TEXT {
                break;
            }
        }
        Ok(Some(from_wide(&out)))
    }

    /// Up to a bit more than `json_bytes` shows, so it can mark the cut.
    fn get_binary(&self, col: u16) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 2048];
        let mut ind: SqlLen = 0;
        // SAFETY: buffer length in bytes.
        let rc = unsafe {
            (self.api.SQLGetData)(self.h.0, col, SQL_C_BINARY, buf.as_mut_ptr() as *mut c_void, buf.len() as SqlLen, &mut ind)
        };
        if rc == SQL_NO_DATA {
            return Ok(Some(Vec::new()));
        }
        if !ok(rc) {
            return Err(self.err());
        }
        if ind == SQL_NULL_DATA {
            return Ok(None);
        }
        let n = if ind == SQL_NO_TOTAL || ind as usize > buf.len() { buf.len() } else { ind as usize };
        buf.truncate(n);
        Ok(Some(buf))
    }

    pub fn cell(&self, col: u16, kind: Kind) -> Result<Value> {
        Ok(match kind {
            Kind::Int => self.get_text(col)?.map_or(Value::Null, |s| int_value(&s)),
            Kind::Decimal => self.get_text(col)?.map_or(Value::Null, |s| decimal_value(&s)),
            Kind::Float => self.get_fixed::<f64>(col, SQL_C_DOUBLE)?.map_or(Value::Null, json_f64),
            Kind::Bit => self.get_fixed::<u8>(col, SQL_C_BIT)?.map_or(Value::Null, |b| Value::Bool(b != 0)),
            Kind::Date => self.get_fixed::<SqlDate>(col, SQL_C_TYPE_DATE)?.map_or(Value::Null, |d| fmt_date(&d).into()),
            Kind::Timestamp => {
                self.get_fixed::<SqlTimestamp>(col, SQL_C_TYPE_TIMESTAMP)?.map_or(Value::Null, |t| fmt_timestamp(&t).into())
            }
            Kind::Binary => self.get_binary(col)?.map_or(Value::Null, |b| json_bytes(&b)),
            Kind::Text => self.get_text(col)?.map_or(Value::Null, Value::String),
        })
    }

    /// Every row of the current result set, each column as text.
    pub fn text_rows(&self) -> Result<Vec<Vec<Option<String>>>> {
        let n = self.num_cols()?;
        let mut rows = Vec::new();
        while self.fetch()? {
            let mut r = Vec::with_capacity(n as usize);
            for c in 1..=n {
                r.push(self.get_text(c)?);
            }
            rows.push(r);
        }
        Ok(rows)
    }

    fn check(&self, rc: SqlReturn) -> Result<()> {
        if ok(rc) {
            Ok(())
        } else {
            Err(self.err())
        }
    }

    pub fn tables(&self, cat: Option<&str>, schema: Option<&str>, table: Option<&str>, ty: Option<&str>) -> Result<()> {
        let (a, b, c, d) = (WArg::new(cat), WArg::new(schema), WArg::new(table), WArg::new(ty));
        // SAFETY: arguments outlive the call.
        let rc = unsafe {
            (self.api.SQLTablesW)(self.h.0, a.ptr(), a.len(), b.ptr(), b.len(), c.ptr(), c.len(), d.ptr(), d.len())
        };
        self.check(rc)
    }

    pub fn columns(&self, schema: Option<&str>, table: &str) -> Result<()> {
        let (a, b, c, d) = (WArg::new(None), WArg::new(schema), WArg::new(Some(table)), WArg::new(None));
        // SAFETY: arguments outlive the call.
        let rc = unsafe {
            (self.api.SQLColumnsW)(self.h.0, a.ptr(), a.len(), b.ptr(), b.len(), c.ptr(), c.len(), d.ptr(), d.len())
        };
        self.check(rc)
    }

    pub fn primary_keys(&self, schema: Option<&str>, table: &str) -> Result<()> {
        let (a, b, c) = (WArg::new(None), WArg::new(schema), WArg::new(Some(table)));
        // SAFETY: arguments outlive the call.
        let rc = unsafe { (self.api.SQLPrimaryKeysW)(self.h.0, a.ptr(), a.len(), b.ptr(), b.len(), c.ptr(), c.len()) };
        self.check(rc)
    }

    /// SQLForeignKeys for the foreign keys of `table` (the referencing side).
    pub fn foreign_keys(&self, schema: Option<&str>, table: &str) -> Result<()> {
        let (pc, ps, pt) = (WArg::new(None), WArg::new(None), WArg::new(None));
        let (fc, fs, ft) = (WArg::new(None), WArg::new(schema), WArg::new(Some(table)));
        // SAFETY: arguments outlive the call.
        let rc = unsafe {
            (self.api.SQLForeignKeysW)(
                self.h.0,
                pc.ptr(),
                pc.len(),
                ps.ptr(),
                ps.len(),
                pt.ptr(),
                pt.len(),
                fc.ptr(),
                fc.len(),
                fs.ptr(),
                fs.len(),
                ft.ptr(),
                ft.len(),
            )
        };
        self.check(rc)
    }

    /// SQLStatistics: every index of `table`.
    pub fn statistics(&self, schema: Option<&str>, table: &str) -> Result<()> {
        let (a, b, c) = (WArg::new(None), WArg::new(schema), WArg::new(Some(table)));
        // SAFETY: arguments outlive the call.
        let rc = unsafe {
            (self.api.SQLStatisticsW)(self.h.0, a.ptr(), a.len(), b.ptr(), b.len(), c.ptr(), c.len(), SQL_INDEX_ALL, SQL_QUICK)
        };
        self.check(rc)
    }

    pub fn procedures(&self) -> Result<()> {
        let (a, b, c) = (WArg::new(None), WArg::new(Some("%")), WArg::new(Some("%")));
        // SAFETY: arguments outlive the call.
        let rc = unsafe { (self.api.SQLProceduresW)(self.h.0, a.ptr(), a.len(), b.ptr(), b.len(), c.ptr(), c.len()) };
        self.check(rc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_types_map_to_kinds() {
        assert_eq!(kind_of(SQL_INTEGER), Kind::Int);
        assert_eq!(kind_of(SQL_BIGINT), Kind::Int);
        assert_eq!(kind_of(SQL_DOUBLE), Kind::Float);
        assert_eq!(kind_of(SQL_DECIMAL), Kind::Decimal);
        assert_eq!(kind_of(SQL_TYPE_TIMESTAMP), Kind::Timestamp);
        assert_eq!(kind_of(SQL_TYPE_DATE), Kind::Date);
        assert_eq!(kind_of(SQL_VARBINARY), Kind::Binary);
        assert_eq!(kind_of(SQL_WVARCHAR), Kind::Text);
        assert_eq!(kind_of(-154), Kind::Text); // SQL Server time(n)
        assert_eq!(kind_of(-11), Kind::Text); // GUID
    }

    #[test]
    fn integers_keep_precision() {
        assert_eq!(int_value(" 42 "), serde_json::json!(42));
        assert_eq!(int_value("9223372036854775807"), serde_json::json!("9223372036854775807"));
        assert_eq!(int_value("18446744073709551615"), serde_json::json!("18446744073709551615"));
    }

    #[test]
    fn decimals_get_their_leading_zero() {
        assert_eq!(decimal_value(".50"), serde_json::json!("0.50"));
        assert_eq!(decimal_value("-.5"), serde_json::json!("-0.5"));
        assert_eq!(decimal_value("12.30"), serde_json::json!("12.30"));
    }

    #[test]
    fn dates_are_iso() {
        let t = SqlTimestamp { year: 2024, month: 1, day: 31, hour: 13, minute: 45, second: 0, fraction: 0 };
        assert_eq!(fmt_timestamp(&t), "2024-01-31 13:45:00");
        let t = SqlTimestamp { fraction: 120_000_000, ..t };
        assert_eq!(fmt_timestamp(&t), "2024-01-31 13:45:00.12");
        assert_eq!(fmt_date(&SqlDate { year: 7, month: 3, day: 9 }), "0007-03-09");
    }

    #[test]
    fn vendor_prefixes_are_dropped() {
        assert_eq!(
            clean_message("[Microsoft][ODBC Driver 18 for SQL Server][SQL Server]Invalid object name 'x'."),
            "Invalid object name 'x'."
        );
        assert_eq!(clean_message("plain"), "plain");
    }
}
