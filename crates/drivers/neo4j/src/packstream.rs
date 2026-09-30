//! PackStream v1, the binary format of the Bolt protocol: encoding of the
//! values we send (strings, integers, maps, lists) and decoding of every
//! value the server returns, graph structures included.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<Value>),
    /// Keys in the order the server sent them.
    Map(Vec<(String, Value)>),
    Struct(u8, Vec<Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
    pub fn as_list(&self) -> &[Value] {
        match self {
            Value::List(l) => l,
            _ => &[],
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

/// A map from `(key, value)` pairs.
pub fn map<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn encode(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0xC0),
        Value::Bool(b) => out.push(if *b { 0xC3 } else { 0xC2 }),
        Value::Int(i) => encode_int(*i, out),
        Value::Float(f) => {
            out.push(0xC1);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Bytes(b) => {
            match b.len() {
                n if n < 0x100 => out.extend_from_slice(&[0xCC, n as u8]),
                n if n < 0x10000 => {
                    out.push(0xCD);
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                }
                n => {
                    out.push(0xCE);
                    out.extend_from_slice(&(n as u32).to_be_bytes());
                }
            }
            out.extend_from_slice(b);
        }
        Value::String(s) => {
            header(s.len(), 0x80, 0xD0, out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::List(l) => {
            header(l.len(), 0x90, 0xD4, out);
            for x in l {
                encode(x, out);
            }
        }
        Value::Map(m) => {
            header(m.len(), 0xA0, 0xD8, out);
            for (k, x) in m {
                encode(&Value::String(k.clone()), out);
                encode(x, out);
            }
        }
        Value::Struct(tag, fields) => {
            out.push(0xB0 | fields.len() as u8);
            out.push(*tag);
            for x in fields {
                encode(x, out);
            }
        }
    }
}

/// The bytes [`encode`] writes for `v`.
pub fn encoded_len(v: &Value) -> usize {
    let head = |n: usize| match n {
        n if n < 16 => 1,
        n if n < 0x100 => 2,
        n if n < 0x10000 => 3,
        _ => 5,
    };
    match v {
        Value::Null | Value::Bool(_) => 1,
        Value::Int(i) => match *i {
            -16..=127 => 1,
            -128..=127 => 2,
            -32_768..=32_767 => 3,
            -2_147_483_648..=2_147_483_647 => 5,
            _ => 9,
        },
        Value::Float(_) => 9,
        Value::Bytes(b) => b.len() + if b.len() < 0x100 { 2 } else if b.len() < 0x10000 { 3 } else { 5 },
        Value::String(s) => head(s.len()) + s.len(),
        Value::List(l) => head(l.len()) + l.iter().map(encoded_len).sum::<usize>(),
        Value::Map(m) => head(m.len()) + m.iter().map(|(k, x)| head(k.len()) + k.len() + encoded_len(x)).sum::<usize>(),
        Value::Struct(_, f) => 2 + f.iter().map(encoded_len).sum::<usize>(),
    }
}

/// Size header: tiny (`tiny | n`) or 8/16/32-bit (`sized`, `sized+1`, `sized+2`).
fn header(n: usize, tiny: u8, sized: u8, out: &mut Vec<u8>) {
    if n < 16 {
        out.push(tiny | n as u8);
    } else if n < 0x100 {
        out.extend_from_slice(&[sized, n as u8]);
    } else if n < 0x10000 {
        out.push(sized + 1);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(sized + 2);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
}

fn encode_int(i: i64, out: &mut Vec<u8>) {
    if (-16..128).contains(&i) {
        out.push(i as i8 as u8);
    } else if (i8::MIN as i64..=i8::MAX as i64).contains(&i) {
        out.extend_from_slice(&[0xC8, i as i8 as u8]);
    } else if (i16::MIN as i64..=i16::MAX as i64).contains(&i) {
        out.push(0xC9);
        out.extend_from_slice(&(i as i16).to_be_bytes());
    } else if (i32::MIN as i64..=i32::MAX as i64).contains(&i) {
        out.push(0xCA);
        out.extend_from_slice(&(i as i32).to_be_bytes());
    } else {
        out.push(0xCB);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err("mensaje Bolt truncado".into());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn len(&mut self, bytes: usize) -> Result<usize, String> {
        let b = self.take(bytes)?;
        Ok(match bytes {
            1 => b[0] as usize,
            2 => u16::from_be_bytes([b[0], b[1]]) as usize,
            _ => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize,
        })
    }

    pub fn value(&mut self) -> Result<Value, String> {
        let m = self.u8()?;
        Ok(match m {
            0x00..=0x7F => Value::Int(m as i64),
            0xF0..=0xFF => Value::Int(m as i8 as i64),
            0x80..=0x8F => self.string((m & 0x0F) as usize)?,
            0x90..=0x9F => self.list((m & 0x0F) as usize)?,
            0xA0..=0xAF => self.map((m & 0x0F) as usize)?,
            0xB0..=0xBF => {
                let tag = self.u8()?;
                let mut fields = Vec::with_capacity((m & 0x0F) as usize);
                for _ in 0..(m & 0x0F) {
                    fields.push(self.value()?);
                }
                Value::Struct(tag, fields)
            }
            0xC0 => Value::Null,
            0xC1 => {
                let b = self.take(8)?;
                Value::Float(f64::from_be_bytes(b.try_into().expect("8 bytes")))
            }
            0xC2 => Value::Bool(false),
            0xC3 => Value::Bool(true),
            0xC8 => Value::Int(self.take(1)?[0] as i8 as i64),
            0xC9 => Value::Int(i16::from_be_bytes(self.take(2)?.try_into().expect("2")) as i64),
            0xCA => Value::Int(i32::from_be_bytes(self.take(4)?.try_into().expect("4")) as i64),
            0xCB => Value::Int(i64::from_be_bytes(self.take(8)?.try_into().expect("8"))),
            0xCC..=0xCE => {
                let n = self.len(1 << (m - 0xCC))?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            0xD0..=0xD2 => {
                let n = self.len(1 << (m - 0xD0))?;
                self.string(n)?
            }
            0xD4..=0xD6 => {
                let n = self.len(1 << (m - 0xD4))?;
                self.list(n)?
            }
            0xD8..=0xDA => {
                let n = self.len(1 << (m - 0xD8))?;
                self.map(n)?
            }
            other => return Err(format!("marcador PackStream desconocido 0x{other:02X}")),
        })
    }

    fn string(&mut self, n: usize) -> Result<Value, String> {
        Ok(Value::String(String::from_utf8_lossy(self.take(n)?).into_owned()))
    }

    fn list(&mut self, n: usize) -> Result<Value, String> {
        let mut l = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            l.push(self.value()?);
        }
        Ok(Value::List(l))
    }

    fn map(&mut self, n: usize) -> Result<Value, String> {
        let mut m = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let k = match self.value()? {
                Value::String(s) => s,
                other => format!("{other:?}"),
            };
            m.push((k, self.value()?));
        }
        Ok(Value::Map(m))
    }
}

/// Struct tags of the graph and temporal types.
pub mod tag {
    pub const NODE: u8 = 0x4E;
    pub const RELATIONSHIP: u8 = 0x52;
    pub const UNBOUND_RELATIONSHIP: u8 = 0x72;
    pub const PATH: u8 = 0x50;
    pub const DATE: u8 = 0x44;
    pub const TIME: u8 = 0x54;
    pub const LOCAL_TIME: u8 = 0x74;
    pub const DATE_TIME: u8 = 0x49;
    pub const DATE_TIME_ZONE_ID: u8 = 0x69;
    pub const LEGACY_DATE_TIME: u8 = 0x46;
    pub const LEGACY_DATE_TIME_ZONE_ID: u8 = 0x66;
    pub const LOCAL_DATE_TIME: u8 = 0x64;
    pub const DURATION: u8 = 0x45;
    pub const POINT_2D: u8 = 0x58;
    pub const POINT_3D: u8 = 0x59;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(v: Value) {
        let mut b = Vec::new();
        encode(&v, &mut b);
        assert_eq!(encoded_len(&v), b.len(), "{v:?}");
        assert_eq!(Decoder::new(&b).value().unwrap(), v, "{b:02X?}");
    }

    #[test]
    fn round_trips() {
        for i in [0, 1, -1, -16, -17, 127, 128, -128, -129, 32767, 32768, -32769, 1 << 31, i64::MIN, i64::MAX] {
            round(Value::Int(i));
        }
        round(Value::Float(1.5));
        round(Value::String("é".repeat(300)));
        round(Value::List((0..20).map(Value::Int).collect()));
        round(map([("a", Value::Null), ("b", Value::Bool(true)), ("c", Value::Bytes(vec![1, 2]))]));
        round(Value::Struct(tag::NODE, vec![Value::Int(1), Value::List(vec![]), map([])]));
        // Every size header (for `encoded_len`).
        for n in [15, 16, 255, 256, 65_535, 65_536] {
            round(Value::String("x".repeat(n)));
            round(Value::Bytes(vec![7; n]));
            round(Value::List(vec![Value::Null; n]));
            round(Value::Map((0..n).map(|i| (format!("{i:k$}", k = if n > 300 { 1 } else { 20 }), Value::Int(i as i64))).collect()));
        }
        for i in [2_147_483_647, -2_147_483_648, 2_147_483_648, -2_147_483_649] {
            round(Value::Int(i));
        }
    }

    #[test]
    fn known_encodings() {
        let mut b = Vec::new();
        encode(&Value::Int(-16), &mut b);
        encode(&Value::String("A".into()), &mut b);
        encode(&map([("n", Value::Int(-1))]), &mut b);
        assert_eq!(b, [0xF0, 0x81, 0x41, 0xA1, 0x81, 0x6E, 0xFF]);
    }
}
