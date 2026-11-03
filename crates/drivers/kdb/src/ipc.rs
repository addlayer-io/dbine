//! kdb+'s IPC wire format (code.kx.com, "Serialization examples" and
//! "C client for kdb+"): an 8-byte header and a serialized q object.
//!
//! Header: byte 0 is the architecture (1 little endian, 0 big), byte 1 the
//! message type (0 async, 1 sync, 2 response), byte 2 whether the body is
//! compressed, byte 3 unused, bytes 4..8 the whole message's length.
//!
//! An object starts with its type byte: negative for an atom, 0..19 for a
//! list (then an attribute byte and a 4-byte count), 98 a table, 99 a
//! dictionary (127 a sorted one), 100 a lambda, 101..112 other functions,
//! -128 an error. Temporal values count from 2000-01-01.

use std::fmt;

/// Message types of the header's second byte.
pub const ASYNC: u8 = 0;
pub const SYNC: u8 = 1;
pub const RESPONSE: u8 = 2;

/// The capability DBine asks for in the handshake: compression and the
/// timestamp, timespan and guid types (kdb+ 3.0+), messages up to 2 GB.
pub const CAPABILITY: u8 = 3;

/// A typed vector's elements.
#[derive(Debug, Clone, PartialEq)]
pub enum Vector {
    Bool(Vec<bool>),
    Guid(Vec<[u8; 16]>),
    Byte(Vec<u8>),
    Short(Vec<i16>),
    Int(Vec<i32>),
    Long(Vec<i64>),
    Real(Vec<f32>),
    Float(Vec<f64>),
    /// A string.
    Char(Vec<u8>),
    Symbol(Vec<String>),
    Timestamp(Vec<i64>),
    Month(Vec<i32>),
    Date(Vec<i32>),
    Datetime(Vec<f64>),
    Timespan(Vec<i64>),
    Minute(Vec<i32>),
    Second(Vec<i32>),
    Time(Vec<i32>),
}

/// A q object.
#[derive(Debug, Clone, PartialEq)]
pub enum K {
    Bool(bool),
    Guid([u8; 16]),
    Byte(u8),
    Short(i16),
    Int(i32),
    Long(i64),
    Real(f32),
    Float(f64),
    Char(u8),
    Symbol(String),
    /// Nanoseconds since 2000-01-01.
    Timestamp(i64),
    /// Months since 2000.01.
    Month(i32),
    /// Days since 2000-01-01.
    Date(i32),
    /// Days (fractional) since 2000-01-01.
    Datetime(f64),
    /// Nanoseconds.
    Timespan(i64),
    Minute(i32),
    Second(i32),
    /// Milliseconds.
    Time(i32),
    /// A typed vector with its attribute (`s`, `u`, `p`, `g` as 1..4).
    Vector(u8, Vector),
    /// A general (mixed) list.
    List(Vec<K>),
    Dict(Box<K>, Box<K>),
    /// A table: its column names and columns (each a vector or list).
    Table(Vec<String>, Vec<K>),
    /// A lambda: its namespace and source.
    Lambda(String, String),
    /// A function other than a lambda (a primitive, a projection, a
    /// composition, an iterator…), as q would show it when known.
    Function(String),
    /// The generic null `::` (what an assignment or a `;` returns).
    Nil,
    /// A q error (`'type`, `'length`…): its text.
    Error(String),
}

/// Why a message can't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

type R<T> = std::result::Result<T, DecodeError>;

fn bad(m: impl Into<String>) -> DecodeError {
    DecodeError(m.into())
}

/// A parsed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub little: bool,
    pub kind: u8,
    pub compressed: bool,
    /// The whole message's length, header included.
    pub len: usize,
}

pub fn header(b: &[u8; 8]) -> R<Header> {
    let little = b[0] == 1;
    let n = if little {
        u32::from_le_bytes([b[4], b[5], b[6], b[7]])
    } else {
        u32::from_be_bytes([b[4], b[5], b[6], b[7]])
    } as usize;
    if n < 8 {
        return Err(bad(format!("mensaje de kdb+ con largo inválido ({n})")));
    }
    Ok(Header {
        little,
        kind: b[1],
        compressed: b[2] == 1,
        len: n,
    })
}

/// A whole message (header and body) holding `k`, little endian.
pub fn message(kind: u8, k: &K) -> Vec<u8> {
    let mut body = Vec::new();
    write(&mut body, k);
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&[1, kind, 0, 0]);
    out.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The sync query DBine sends: a string q evaluates (`value` on the
/// server, through `.z.pg`).
pub fn query(text: &str) -> Vec<u8> {
    message(SYNC, &K::Vector(0, Vector::Char(text.as_bytes().to_vec())))
}

/// The handshake: `user:password`, the capability and a 0 byte. An empty
/// user sends only the capability.
pub fn handshake(user: &str, password: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if !user.is_empty() || !password.is_empty() {
        out.extend_from_slice(user.as_bytes());
        out.push(b':');
        out.extend_from_slice(password.as_bytes());
    }
    out.push(CAPABILITY);
    out.push(0);
    out
}

/// The object of a message's body (`msg` is the whole message, header
/// included; a compressed one is uncompressed first).
pub fn decode(msg: &[u8]) -> R<K> {
    if msg.len() < 8 {
        return Err(bad("mensaje de kdb+ incompleto"));
    }
    let h = header(msg[..8].try_into().expect("8 bytes"))?;
    let owned;
    let msg = if h.compressed {
        owned = uncompress(msg, h.little)?;
        &owned[..]
    } else {
        msg
    };
    let mut r = Reader {
        b: msg,
        at: 8,
        little: h.little,
    };
    r.k()
}

/// kdb+'s IPC compression undone (the algorithm of kx's `c.java` and
/// `c.cs`): bytes 8..12 hold the uncompressed length, header included.
pub fn uncompress(msg: &[u8], little: bool) -> R<Vec<u8>> {
    if msg.len() < 12 {
        return Err(bad("mensaje comprimido de kdb+ incompleto"));
    }
    let raw = [msg[8], msg[9], msg[10], msg[11]];
    let n = if little {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    } as usize;
    // Each token of kdb+'s compression copies at most 257 bytes from 2 of
    // input: a length far past that is a bad header, refused before it's
    // allocated.
    if n < 8 || n > i32::MAX as usize || n / 256 > msg.len() {
        return Err(bad("mensaje comprimido de kdb+ con largo inválido"));
    }
    let mut dst = vec![0u8; n];
    dst[..8].copy_from_slice(&msg[..8]);
    dst[2] = 0;
    let src = msg;
    let mut aa = [0usize; 256];
    let (mut s, mut p, mut d) = (8usize, 8usize, 12usize);
    let (mut f, mut i) = (0u32, 0u32);
    let byte = |d: usize| -> R<u8> {
        src.get(d)
            .copied()
            .ok_or_else(|| bad("mensaje comprimido de kdb+ truncado"))
    };
    while s < n {
        if i == 0 {
            f = byte(d)? as u32;
            d += 1;
            i = 1;
        }
        let mut copied = 0usize;
        if f & i != 0 {
            let mut r = aa[byte(d)? as usize];
            d += 1;
            if s + 2 > n || r + 2 > n {
                return Err(bad("mensaje comprimido de kdb+ inválido"));
            }
            dst[s] = dst[r];
            s += 1;
            r += 1;
            dst[s] = dst[r];
            s += 1;
            r += 1;
            copied = byte(d)? as usize;
            d += 1;
            if s + copied > n || r + copied > n {
                return Err(bad("mensaje comprimido de kdb+ inválido"));
            }
            for m in 0..copied {
                dst[s + m] = dst[r + m];
            }
        } else {
            dst[s] = byte(d)?;
            s += 1;
            d += 1;
        }
        while p + 1 < s {
            aa[(dst[p] ^ dst[p + 1]) as usize] = p;
            p += 1;
        }
        if f & i != 0 {
            s += copied;
            p = s;
        }
        i *= 2;
        if i == 256 {
            i = 0;
        }
    }
    Ok(dst)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
    little: bool,
}

macro_rules! num {
    ($name:ident, $t:ty, $n:expr) => {
        fn $name(&mut self) -> R<$t> {
            let s = self.take($n)?;
            let a: [u8; $n] = s.try_into().expect("size");
            Ok(if self.little {
                <$t>::from_le_bytes(a)
            } else {
                <$t>::from_be_bytes(a)
            })
        }
    };
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> R<&[u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or_else(|| bad("mensaje de kdb+ truncado"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }

    num!(i16, i16, 2);
    num!(i32, i32, 4);
    num!(i64, i64, 8);
    num!(f32, f32, 4);
    num!(f64, f64, 8);

    fn guid(&mut self) -> R<[u8; 16]> {
        Ok(self.take(16)?.try_into().expect("16 bytes"))
    }

    /// A 0-terminated symbol.
    fn sym(&mut self) -> R<String> {
        let rest = &self.b[self.at.min(self.b.len())..];
        let end = rest
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| bad("símbolo de kdb+ sin terminar"))?;
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.at += end + 1;
        Ok(s)
    }

    fn count(&mut self) -> R<usize> {
        let n = self.i32()?;
        if n < 0 {
            return Err(bad("lista de kdb+ con largo negativo"));
        }
        let n = n as usize;
        // Every element takes at least one byte: a count past the end is
        // a corrupt message, not a reason to allocate.
        if n > self.b.len().saturating_sub(self.at).max(1) * 8 {
            return Err(bad("lista de kdb+ más larga que el mensaje"));
        }
        Ok(n)
    }

    fn many<T>(&mut self, n: usize, mut f: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        let mut v = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }

    fn k(&mut self) -> R<K> {
        let t = self.u8()? as i8;
        Ok(match t {
            -1 => K::Bool(self.u8()? != 0),
            -2 => K::Guid(self.guid()?),
            -4 => K::Byte(self.u8()?),
            -5 => K::Short(self.i16()?),
            -6 => K::Int(self.i32()?),
            -7 => K::Long(self.i64()?),
            -8 => K::Real(self.f32()?),
            -9 => K::Float(self.f64()?),
            -10 => K::Char(self.u8()?),
            -11 => K::Symbol(self.sym()?),
            -12 => K::Timestamp(self.i64()?),
            -13 => K::Month(self.i32()?),
            -14 => K::Date(self.i32()?),
            -15 => K::Datetime(self.f64()?),
            -16 => K::Timespan(self.i64()?),
            -17 => K::Minute(self.i32()?),
            -18 => K::Second(self.i32()?),
            -19 => K::Time(self.i32()?),
            -128 => K::Error(self.sym()?),
            0 => {
                let _attr = self.u8()?;
                let n = self.count()?;
                K::List(self.many(n, |r| r.k())?)
            }
            1..=19 => {
                let attr = self.u8()?;
                let n = self.count()?;
                K::Vector(attr, self.vector(t, n)?)
            }
            98 => {
                let _attr = self.u8()?;
                match self.k()? {
                    K::Dict(keys, values) => table(*keys, *values)?,
                    _ => return Err(bad("tabla de kdb+ sin diccionario")),
                }
            }
            99 | 127 => {
                let keys = self.k()?;
                let values = self.k()?;
                K::Dict(Box::new(keys), Box::new(values))
            }
            100 => {
                let ctx = self.sym()?;
                match self.k()? {
                    K::Vector(_, Vector::Char(c)) => {
                        K::Lambda(ctx, String::from_utf8_lossy(&c).into_owned())
                    }
                    _ => return Err(bad("función de kdb+ sin texto")),
                }
            }
            101 => match self.u8()? {
                // `::`, and the elided argument of a projection (`f[;1]`).
                0 | 0xff => K::Nil,
                p => K::Function(primitive(101, p)),
            },
            102 | 103 => {
                let p = self.u8()?;
                K::Function(primitive(t, p))
            }
            104 | 105 => {
                let n = self.count()?;
                let parts = self.many(n, |r| r.k())?;
                K::Function(match t {
                    104 => format!(
                        "{}[{}]",
                        parts.first().map(show).unwrap_or_default(),
                        parts[1.min(parts.len())..]
                            .iter()
                            .map(show)
                            .collect::<Vec<_>>()
                            .join(";")
                    ),
                    _ => parts.iter().map(show).collect::<Vec<_>>().join(""),
                })
            }
            106..=111 => {
                let f = self.k()?;
                let it = ["'", "/", "\\", "':", "/:", "\\:"][(t - 106) as usize];
                K::Function(format!("{}{it}", show(&f)))
            }
            112 => K::Function("foreign".into()),
            20..=76 => {
                return Err(bad(format!(
                "kdb+ envió una enumeración (tipo {t}): consultala con value o con `$ para leerla"
            )))
            }
            _ => return Err(bad(format!("tipo de kdb+ desconocido ({t})"))),
        })
    }

    fn vector(&mut self, t: i8, n: usize) -> R<Vector> {
        Ok(match t {
            1 => Vector::Bool(self.many(n, |r| Ok(r.u8()? != 0))?),
            2 => Vector::Guid(self.many(n, |r| r.guid())?),
            4 => Vector::Byte(self.take(n)?.to_vec()),
            5 => Vector::Short(self.many(n, |r| r.i16())?),
            6 => Vector::Int(self.many(n, |r| r.i32())?),
            7 => Vector::Long(self.many(n, |r| r.i64())?),
            8 => Vector::Real(self.many(n, |r| r.f32())?),
            9 => Vector::Float(self.many(n, |r| r.f64())?),
            10 => Vector::Char(self.take(n)?.to_vec()),
            11 => Vector::Symbol(self.many(n, |r| r.sym())?),
            12 => Vector::Timestamp(self.many(n, |r| r.i64())?),
            13 => Vector::Month(self.many(n, |r| r.i32())?),
            14 => Vector::Date(self.many(n, |r| r.i32())?),
            15 => Vector::Datetime(self.many(n, |r| r.f64())?),
            16 => Vector::Timespan(self.many(n, |r| r.i64())?),
            17 => Vector::Minute(self.many(n, |r| r.i32())?),
            18 => Vector::Second(self.many(n, |r| r.i32())?),
            19 => Vector::Time(self.many(n, |r| r.i32())?),
            _ => return Err(bad(format!("tipo de lista de kdb+ desconocido ({t})"))),
        })
    }
}

/// A table from its column dictionary (symbols → columns).
fn table(keys: K, values: K) -> R<K> {
    let names = match keys {
        K::Vector(_, Vector::Symbol(s)) => s,
        _ => return Err(bad("tabla de kdb+ con nombres de columna inválidos")),
    };
    let cols = match values {
        K::List(l) => l,
        // A one-column table can come with its columns as a typed list.
        v @ K::Vector(..) => vec![v],
        _ => return Err(bad("tabla de kdb+ con columnas inválidas")),
    };
    if cols.len() != names.len() {
        return Err(bad("tabla de kdb+ con columnas de más o de menos"));
    }
    Ok(K::Table(names, cols))
}

/// q's primitives by their code in a 101..103 object. Unary (101) and
/// binary (102) primitives share the codes of the verbs `: + - * % & | ^ =
/// < > $ , # _ ~ ! ? @ .`; a unary one shows as its q keyword (`(-:)` is
/// `neg`, `(#:)` is `count`), as the q console does.
fn primitive(t: i8, p: u8) -> String {
    // The codes as kdb+ sends them (`-8!` of each primitive).
    const BINARY: [&str; 35] = [
        ":", "+", "-", "*", "%", "&", "|", "^", "=", "<", ">", "$", ",", "#", "_", "~", "!", "?",
        "@", ".", "0:", "1:", "2:", "in", "within", "like", "bin", "ss", "insert", "wsum", "wavg",
        "div", "xexp", "setenv", "binr",
    ];
    const UNARY: [&str; 42] = [
        "::",
        "flip",
        "neg",
        "first",
        "reciprocal",
        "where",
        "reverse",
        "null",
        "group",
        "hopen",
        "hclose",
        "string",
        "enlist",
        "count",
        "floor",
        "not",
        "key",
        "distinct",
        "type",
        "value",
        "read0",
        "read1",
        "2::",
        "avg",
        "last",
        "sum",
        "prd",
        "min",
        "max",
        "exit",
        "getenv",
        "abs",
        "sqrt",
        "log",
        "exp",
        "sin",
        "asin",
        "cos",
        "acos",
        "tan",
        "atan",
        "enlist",
    ];
    const ITERATORS: [&str; 6] = ["'", "/", "\\", "':", "/:", "\\:"];
    let p = p as usize;
    let (table, unknown): (&[&str], &str) = match t {
        101 => (&UNARY, "prim"),
        102 => (&BINARY, "prim"),
        _ => (&ITERATORS, "iter"),
    };
    table
        .get(p)
        .map_or_else(|| format!("{unknown}{p}"), |s| s.to_string())
}

/// How a function part shows (for projections and compositions).
fn show(k: &K) -> String {
    match k {
        K::Lambda(_, src) => src.clone(),
        K::Function(f) => f.clone(),
        K::Nil => String::new(),
        K::Symbol(s) => format!("`{s}"),
        K::Vector(_, Vector::Char(c)) => format!("\"{}\"", String::from_utf8_lossy(c)),
        K::Long(n) => n.to_string(),
        K::Int(n) => format!("{n}i"),
        K::Float(f) => f.to_string(),
        _ => "…".into(),
    }
}

// --- Writing (the queries DBine sends, and the fake server of the tests).

fn write(out: &mut Vec<u8>, k: &K) {
    match k {
        K::Bool(b) => {
            out.push(-1i8 as u8);
            out.push(u8::from(*b));
        }
        K::Guid(g) => {
            out.push(-2i8 as u8);
            out.extend_from_slice(g);
        }
        K::Byte(b) => out.extend_from_slice(&[-4i8 as u8, *b]),
        K::Short(n) => {
            out.push(-5i8 as u8);
            out.extend_from_slice(&n.to_le_bytes());
        }
        K::Int(n) => atom(out, -6, &n.to_le_bytes()),
        K::Long(n) => atom(out, -7, &n.to_le_bytes()),
        K::Real(n) => atom(out, -8, &n.to_le_bytes()),
        K::Float(n) => atom(out, -9, &n.to_le_bytes()),
        K::Char(c) => out.extend_from_slice(&[-10i8 as u8, *c]),
        K::Symbol(s) => {
            out.push(-11i8 as u8);
            sym(out, s);
        }
        K::Timestamp(n) => atom(out, -12, &n.to_le_bytes()),
        K::Month(n) => atom(out, -13, &n.to_le_bytes()),
        K::Date(n) => atom(out, -14, &n.to_le_bytes()),
        K::Datetime(n) => atom(out, -15, &n.to_le_bytes()),
        K::Timespan(n) => atom(out, -16, &n.to_le_bytes()),
        K::Minute(n) => atom(out, -17, &n.to_le_bytes()),
        K::Second(n) => atom(out, -18, &n.to_le_bytes()),
        K::Time(n) => atom(out, -19, &n.to_le_bytes()),
        K::Error(e) => {
            out.push(0x80);
            sym(out, e);
        }
        K::Nil => out.extend_from_slice(&[101, 0]),
        K::Function(_) => out.extend_from_slice(&[101, 1]),
        K::Lambda(ctx, src) => {
            out.push(100);
            sym(out, ctx);
            write(out, &K::Vector(0, Vector::Char(src.as_bytes().to_vec())));
        }
        K::List(items) => {
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&(items.len() as i32).to_le_bytes());
            for i in items {
                write(out, i);
            }
        }
        K::Dict(keys, values) => {
            out.push(99);
            write(out, keys);
            write(out, values);
        }
        K::Table(names, cols) => {
            out.extend_from_slice(&[98, 0, 99]);
            write(out, &K::Vector(0, Vector::Symbol(names.clone())));
            write(out, &K::List(cols.clone()));
        }
        K::Vector(attr, v) => {
            out.push(v.type_code() as u8);
            out.push(*attr);
            out.extend_from_slice(&(v.len() as i32).to_le_bytes());
            v.write(out);
        }
    }
}

fn atom(out: &mut Vec<u8>, t: i8, bytes: &[u8]) {
    out.push(t as u8);
    out.extend_from_slice(bytes);
}

fn sym(out: &mut Vec<u8>, s: &str) {
    out.extend(s.bytes().filter(|&b| b != 0));
    out.push(0);
}

impl Vector {
    /// q's type number (1..19).
    pub fn type_code(&self) -> i8 {
        match self {
            Vector::Bool(_) => 1,
            Vector::Guid(_) => 2,
            Vector::Byte(_) => 4,
            Vector::Short(_) => 5,
            Vector::Int(_) => 6,
            Vector::Long(_) => 7,
            Vector::Real(_) => 8,
            Vector::Float(_) => 9,
            Vector::Char(_) => 10,
            Vector::Symbol(_) => 11,
            Vector::Timestamp(_) => 12,
            Vector::Month(_) => 13,
            Vector::Date(_) => 14,
            Vector::Datetime(_) => 15,
            Vector::Timespan(_) => 16,
            Vector::Minute(_) => 17,
            Vector::Second(_) => 18,
            Vector::Time(_) => 19,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Vector::Bool(v) => v.len(),
            Vector::Guid(v) => v.len(),
            Vector::Byte(v) | Vector::Char(v) => v.len(),
            Vector::Short(v) => v.len(),
            Vector::Int(v)
            | Vector::Month(v)
            | Vector::Date(v)
            | Vector::Minute(v)
            | Vector::Second(v)
            | Vector::Time(v) => v.len(),
            Vector::Long(v) | Vector::Timestamp(v) | Vector::Timespan(v) => v.len(),
            Vector::Real(v) => v.len(),
            Vector::Float(v) | Vector::Datetime(v) => v.len(),
            Vector::Symbol(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element `i` as an atom.
    pub fn atom(&self, i: usize) -> Option<K> {
        Some(match self {
            Vector::Bool(v) => K::Bool(*v.get(i)?),
            Vector::Guid(v) => K::Guid(*v.get(i)?),
            Vector::Byte(v) => K::Byte(*v.get(i)?),
            Vector::Short(v) => K::Short(*v.get(i)?),
            Vector::Int(v) => K::Int(*v.get(i)?),
            Vector::Long(v) => K::Long(*v.get(i)?),
            Vector::Real(v) => K::Real(*v.get(i)?),
            Vector::Float(v) => K::Float(*v.get(i)?),
            Vector::Char(v) => K::Char(*v.get(i)?),
            Vector::Symbol(v) => K::Symbol(v.get(i)?.clone()),
            Vector::Timestamp(v) => K::Timestamp(*v.get(i)?),
            Vector::Month(v) => K::Month(*v.get(i)?),
            Vector::Date(v) => K::Date(*v.get(i)?),
            Vector::Datetime(v) => K::Datetime(*v.get(i)?),
            Vector::Timespan(v) => K::Timespan(*v.get(i)?),
            Vector::Minute(v) => K::Minute(*v.get(i)?),
            Vector::Second(v) => K::Second(*v.get(i)?),
            Vector::Time(v) => K::Time(*v.get(i)?),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Vector::Bool(v) => out.extend(v.iter().map(|&b| u8::from(b))),
            Vector::Guid(v) => v.iter().for_each(|g| out.extend_from_slice(g)),
            Vector::Byte(v) | Vector::Char(v) => out.extend_from_slice(v),
            Vector::Short(v) => v
                .iter()
                .for_each(|n| out.extend_from_slice(&n.to_le_bytes())),
            Vector::Int(v)
            | Vector::Month(v)
            | Vector::Date(v)
            | Vector::Minute(v)
            | Vector::Second(v)
            | Vector::Time(v) => v
                .iter()
                .for_each(|n| out.extend_from_slice(&n.to_le_bytes())),
            Vector::Long(v) | Vector::Timestamp(v) | Vector::Timespan(v) => v
                .iter()
                .for_each(|n| out.extend_from_slice(&n.to_le_bytes())),
            Vector::Real(v) => v
                .iter()
                .for_each(|n| out.extend_from_slice(&n.to_le_bytes())),
            Vector::Float(v) | Vector::Datetime(v) => v
                .iter()
                .for_each(|n| out.extend_from_slice(&n.to_le_bytes())),
            Vector::Symbol(v) => v.iter().for_each(|s| sym(out, s)),
        }
    }
}

impl K {
    /// The number of rows it shows as: a table's rows, a list's items.
    pub fn count(&self) -> usize {
        match self {
            K::Vector(_, v) => v.len(),
            K::List(l) => l.len(),
            K::Table(_, cols) => cols.first().map_or(0, K::count),
            K::Dict(k, _) => k.count(),
            _ => 1,
        }
    }

    /// Item `i` of a vector or a list.
    pub fn item(&self, i: usize) -> Option<K> {
        match self {
            K::Vector(_, v) => v.atom(i),
            K::List(l) => l.get(i).cloned(),
            _ => None,
        }
    }

    /// The text of a string or a symbol.
    pub fn text(&self) -> Option<String> {
        match self {
            K::Vector(_, Vector::Char(c)) => Some(String::from_utf8_lossy(c).into_owned()),
            K::Symbol(s) => Some(s.clone()),
            K::Char(c) => Some((*c as char).to_string()),
            _ => None,
        }
    }

    /// The names of a symbol list (or a single symbol).
    pub fn symbols(&self) -> Option<Vec<String>> {
        match self {
            K::Vector(_, Vector::Symbol(s)) => Some(s.clone()),
            K::Symbol(s) => Some(vec![s.clone()]),
            K::List(l) if l.is_empty() => Some(Vec::new()),
            K::List(l) => l.iter().map(K::text).collect(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(k: K) {
        let m = message(RESPONSE, &k);
        assert_eq!(decode(&m).unwrap(), k);
    }

    #[test]
    fn known_bytes_from_the_kdb_docs() {
        // -8!1i (code.kx.com, Serialization examples)
        let m = [
            0x01, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0xfa, 0x01, 0x00, 0x00, 0x00,
        ];
        assert_eq!(decode(&m).unwrap(), K::Int(1));
        // -8!`ab`c (a symbol list)
        let m = [
            0x01, 0x00, 0x00, 0x00, 0x13, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x02, 0x00, 0x00, 0x00,
            0x61, 0x62, 0x00, 0x63, 0x00,
        ];
        assert_eq!(
            decode(&m).unwrap(),
            K::Vector(0, Vector::Symbol(vec!["ab".into(), "c".into()]))
        );
        // -8!"abc"
        let m = [
            0x01, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x03, 0x00, 0x00, 0x00,
            0x61, 0x62, 0x63,
        ];
        assert_eq!(decode(&m).unwrap().text().as_deref(), Some("abc"));
        // -8!`a`b!2 3i (a dictionary)
        let m = [
            0x01, 0x00, 0x00, 0x00, 0x21, 0x00, 0x00, 0x00, 0x63, 0x0b, 0x00, 0x02, 0x00, 0x00,
            0x00, 0x61, 0x00, 0x62, 0x00, 0x06, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        assert_eq!(
            decode(&m).unwrap(),
            K::Dict(
                Box::new(K::Vector(0, Vector::Symbol(vec!["a".into(), "b".into()]))),
                Box::new(K::Vector(0, Vector::Int(vec![2, 3]))),
            )
        );
        // -8!([]a:enlist 2i;b:enlist 3i) (a table)
        let m = [
            0x01, 0x00, 0x00, 0x00, 0x2f, 0x00, 0x00, 0x00, 0x62, 0x00, 0x63, 0x0b, 0x00, 0x02,
            0x00, 0x00, 0x00, 0x61, 0x00, 0x62, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x06,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x06, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        assert_eq!(
            decode(&m).unwrap(),
            K::Table(
                vec!["a".into(), "b".into()],
                vec![
                    K::Vector(0, Vector::Int(vec![2])),
                    K::Vector(0, Vector::Int(vec![3]))
                ]
            )
        );
        // An error: 'type
        let m = [
            0x01, 0x02, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x80, 0x74, 0x79, 0x70, 0x65, 0x00,
        ];
        assert_eq!(decode(&m).unwrap(), K::Error("type".into()));
        // The generic null (what `a:1;` answers).
        let m = [0x01, 0x02, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x65, 0x00];
        assert_eq!(decode(&m).unwrap(), K::Nil);
    }

    #[test]
    fn our_query_is_a_sync_char_vector() {
        assert_eq!(
            query("1+1"),
            vec![1, 1, 0, 0, 17, 0, 0, 0, 10, 0, 3, 0, 0, 0, b'1', b'+', b'1']
        );
        assert_eq!(handshake("u", "p"), b"u:p\x03\x00".to_vec());
        assert_eq!(handshake("", ""), vec![3, 0]);
    }

    #[test]
    fn every_type_round_trips() {
        for k in [
            K::Bool(true),
            K::Guid([7; 16]),
            K::Byte(0xab),
            K::Short(-3),
            K::Int(i32::MIN),
            K::Long(i64::MAX),
            K::Real(1.5),
            K::Float(f64::INFINITY),
            K::Char(b'x'),
            K::Symbol("a.b".into()),
            K::Timestamp(1),
            K::Month(-1),
            K::Date(9000),
            K::Datetime(0.5),
            K::Timespan(-5),
            K::Minute(61),
            K::Second(3601),
            K::Time(86_399_999),
            K::Error("length".into()),
            K::Nil,
            K::Lambda(String::new(), "{x+y}".into()),
            K::List(vec![K::Long(1), K::Symbol("s".into())]),
            K::Vector(1, Vector::Long(vec![1, 2, 3])),
            K::Vector(0, Vector::Bool(vec![true, false])),
            K::Vector(0, Vector::Guid(vec![[1; 16]])),
            K::Vector(0, Vector::Short(vec![1])),
            K::Vector(0, Vector::Real(vec![1.0])),
            K::Vector(0, Vector::Float(vec![2.0])),
            K::Vector(0, Vector::Timestamp(vec![0])),
            K::Vector(0, Vector::Month(vec![0])),
            K::Vector(0, Vector::Date(vec![0])),
            K::Vector(0, Vector::Datetime(vec![0.0])),
            K::Vector(0, Vector::Timespan(vec![0])),
            K::Vector(0, Vector::Minute(vec![0])),
            K::Vector(0, Vector::Second(vec![0])),
            K::Vector(0, Vector::Time(vec![0])),
            K::Vector(0, Vector::Byte(vec![1, 2])),
            K::Table(
                vec!["k".into(), "v".into()],
                vec![
                    K::Vector(0, Vector::Symbol(vec!["a".into(), "b".into()])),
                    K::List(vec![
                        K::Vector(0, Vector::Char(b"x".to_vec())),
                        K::Vector(0, Vector::Char(b"yz".to_vec())),
                    ]),
                ],
            ),
        ] {
            roundtrip(k);
        }
        // A keyed table: a dictionary of two tables.
        let t =
            |c: &str, v: Vec<i64>| K::Table(vec![c.into()], vec![K::Vector(0, Vector::Long(v))]);
        roundtrip(K::Dict(
            Box::new(t("k", vec![1])),
            Box::new(t("v", vec![2])),
        ));
    }

    #[test]
    fn primitives_projections_and_elided_arguments() {
        let msg = |body: &[u8]| {
            let mut m = vec![1, RESPONSE, 0, 0];
            m.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
            m.extend_from_slice(body);
            m
        };
        // `neg` is unary 2, `-` binary 2; `count` unary 13, `#` binary 13.
        assert_eq!(decode(&msg(&[101, 2])).unwrap(), K::Function("neg".into()));
        assert_eq!(decode(&msg(&[102, 2])).unwrap(), K::Function("-".into()));
        assert_eq!(
            decode(&msg(&[101, 13])).unwrap(),
            K::Function("count".into())
        );
        assert_eq!(decode(&msg(&[102, 13])).unwrap(), K::Function("#".into()));
        assert_eq!(decode(&msg(&[101, 0])).unwrap(), K::Nil);
        assert_eq!(
            decode(&msg(&[101, 99])).unwrap(),
            K::Function("prim99".into())
        );
        // `+[;1]`: a projection of `+` whose first argument is elided.
        let mut p = vec![104, 3, 0, 0, 0, 102, 1, 101, 0xff];
        p.extend_from_slice(&[0xf9]);
        p.extend_from_slice(&1i64.to_le_bytes());
        assert_eq!(decode(&msg(&p)).unwrap(), K::Function("+[;1]".into()));
    }

    #[test]
    fn a_compressed_length_far_past_the_message_is_refused() {
        // 12 bytes claiming to uncompress to 1 GB.
        let mut m = vec![1, RESPONSE, 1, 0, 12, 0, 0, 0];
        m.extend_from_slice(&(1u32 << 30).to_le_bytes());
        assert!(decode(&m).is_err());
    }

    #[test]
    fn truncated_or_corrupt_messages_fail_cleanly() {
        let m = message(RESPONSE, &K::Vector(0, Vector::Long(vec![1, 2, 3])));
        for cut in 8..m.len() {
            assert!(decode(&m[..cut]).is_err(), "cut at {cut}");
        }
        // A count far past the end allocates nothing.
        let mut m = message(RESPONSE, &K::Vector(0, Vector::Long(vec![])));
        m[10..14].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(decode(&m).is_err());
        assert!(decode(&[1, 2, 0, 0, 9, 0, 0, 0, 0x50]).is_err());
    }

    /// A compressor that mirrors `uncompress` step by step (the same
    /// table of last positions, updated at the same moments), so what it
    /// writes is what kdb+'s own compressor could have written.
    fn compress(msg: &[u8]) -> Vec<u8> {
        let n = msg.len();
        let mut y = msg[..8].to_vec();
        y[2] = 1;
        y.extend_from_slice(&(n as u32).to_le_bytes());
        let mut aa = [0usize; 256];
        let (mut s, mut p) = (8usize, 8usize);
        let (mut i, mut flag_at) = (0u32, 0usize);
        while s < n {
            if i == 0 {
                flag_at = y.len();
                y.push(0);
                i = 1;
            }
            let r = if s + 1 < n {
                aa[(msg[s] ^ msg[s + 1]) as usize]
            } else {
                0
            };
            let hit = r != 0 && s + 2 <= n && msg[r] == msg[s] && msg[r + 1] == msg[s + 1];
            if hit {
                let mut extra = 0usize;
                while extra < 255 && s + 2 + extra < n && msg[r + 2 + extra] == msg[s + 2 + extra] {
                    extra += 1;
                }
                y[flag_at] |= i as u8;
                y.push(msg[s] ^ msg[s + 1]);
                y.push(extra as u8);
                s += 2;
                while p + 1 < s {
                    aa[(msg[p] ^ msg[p + 1]) as usize] = p;
                    p += 1;
                }
                s += extra;
                p = s;
            } else {
                y.push(msg[s]);
                s += 1;
                while p + 1 < s {
                    aa[(msg[p] ^ msg[p + 1]) as usize] = p;
                    p += 1;
                }
            }
            i *= 2;
            if i == 256 {
                i = 0;
            }
        }
        let len = y.len() as u32;
        y[4..8].copy_from_slice(&len.to_le_bytes());
        y
    }

    #[test]
    fn compressed_messages_are_uncompressed() {
        // Repetitive enough that the copies kick in.
        let mut syms = Vec::new();
        for i in 0..300 {
            syms.push(format!("sym{}", i % 7));
        }
        let k = K::Table(
            vec!["s".into(), "n".into()],
            vec![
                K::Vector(0, Vector::Symbol(syms)),
                K::Vector(0, Vector::Long((0..300).map(|i| i % 5).collect())),
            ],
        );
        let m = message(RESPONSE, &k);
        let z = compress(&m);
        assert!(z.len() < m.len(), "{} vs {}", z.len(), m.len());
        assert_eq!(z[2], 1);
        assert_eq!(uncompress(&z, true).unwrap()[8..], m[8..]);
        assert_eq!(decode(&z).unwrap(), k);
        // Cut short: an error, not a panic.
        for cut in [12, z.len() / 2, z.len() - 1] {
            let mut short = z[..cut].to_vec();
            short[4..8].copy_from_slice(&(cut as u32).to_le_bytes());
            assert!(decode(&short).is_err());
        }
    }
}
