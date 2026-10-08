//! Masking of a subset's values, in DBine, before anything reaches the
//! target. Each column of the plan has a rule; the PII ones come
//! pre-selected by name and type ([`suggest`]).
//!
//! A rule's output depends only on the run's seed, the rule (with its
//! settings) and the original value: the same customer email masked in two
//! tables comes out the same, so joins on masked columns still match.
//! NULLs stay NULL whatever the rule.

use crate::commands::datagen::{self, Rng};
use chrono::{Duration as Days, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// What happens to a column's values.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum MaskRule {
    #[default]
    Keep,
    /// A made-up value of a kind.
    Fake { kind: FakeKind },
    /// The date moved up to `days` days either way.
    ShiftDate { days: i64 },
    /// The number moved up to `percent` % either way.
    Noise { percent: f64 },
    Fixed { value: String },
    Null,
    /// A hash of the value (salted with the run's seed).
    Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FakeKind {
    Name,
    FirstName,
    LastName,
    Email,
    Phone,
    /// Same shape as the original (DNI, CUIT, IBAN, card…): every digit
    /// replaced.
    Document,
    Address,
    City,
    Company,
}

impl FakeKind {
    /// The datagen generator that makes it (none for `Document`).
    fn generator(self) -> Option<&'static str> {
        Some(match self {
            FakeKind::Name => "full_name",
            FakeKind::FirstName => "first_name",
            FakeKind::LastName => "last_name",
            FakeKind::Email => "email",
            FakeKind::Phone => "phone",
            FakeKind::Address => "address",
            FakeKind::City => "city",
            FakeKind::Company => "company",
            FakeKind::Document => return None,
        })
    }
}

impl MaskRule {
    pub fn is_keep(&self) -> bool {
        matches!(self, MaskRule::Keep)
    }

    /// The rule and its settings: what the output depends on besides the
    /// value and the seed.
    fn tag(&self) -> String {
        match self {
            MaskRule::Keep => "keep".into(),
            MaskRule::Fake { kind } => format!("fake:{kind:?}"),
            MaskRule::ShiftDate { days } => format!("shift:{days}"),
            MaskRule::Noise { percent } => format!("noise:{percent}"),
            MaskRule::Fixed { value } => format!("fixed:{value}"),
            MaskRule::Null => "null".into(),
            MaskRule::Hash => "hash".into(),
        }
    }
}

/// What masking needs to know of the target column.
#[derive(Debug, Clone, Copy, Default)]
pub struct Shape {
    /// The text length the column takes (`varchar(40)`).
    pub len: Option<usize>,
    /// A number column: the output stays a number.
    pub numeric: bool,
}

impl Shape {
    pub fn of(data_type: &str) -> Shape {
        Shape { len: datagen::length(data_type), numeric: is_numeric(data_type) }
    }
}

pub fn is_numeric(data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    ["int", "dec", "num", "money", "float", "real", "double", "serial"].iter().any(|w| t.contains(w)) && !t.contains("interval") && !t.contains("point")
}

fn is_dateish(data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    t.contains("date") || t.contains("time")
}

fn is_texty(data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    t.is_empty() || ["char", "text", "string", "clob", "str", "varchar"].iter().any(|w| t.contains(w))
}

/// A value as text, for hashing and comparing (`12` and `"12"` alike).
pub fn canonical(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn clip(s: String, len: Option<usize>) -> String {
    match len {
        Some(n) if n > 0 && s.chars().count() > n => s.chars().take(n).collect(),
        _ => s,
    }
}

/// Applies rules with one run's seed.
pub struct Masker {
    seed: u64,
}

impl Masker {
    pub fn new(seed: u64) -> Self {
        Masker { seed }
    }

    fn digest(&self, rule: &MaskRule, v: &Value) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.seed.to_le_bytes());
        h.update(rule.tag().as_bytes());
        h.update([0u8]);
        h.update(canonical(v).as_bytes());
        h.finalize().into()
    }

    fn rng(&self, rule: &MaskRule, v: &Value) -> Rng {
        let d = self.digest(rule, v);
        Rng::new(Some(u64::from_le_bytes(d[..8].try_into().unwrap_or_default())))
    }

    pub fn apply(&self, rule: &MaskRule, v: &Value, shape: Shape) -> Value {
        if v.is_null() {
            return Value::Null;
        }
        match rule {
            MaskRule::Keep => v.clone(),
            MaskRule::Null => Value::Null,
            MaskRule::Fixed { value } => {
                if shape.numeric {
                    if let Ok(i) = value.trim().parse::<i64>() {
                        return json!(i);
                    }
                    if let Ok(f) = value.trim().parse::<f64>() {
                        return json!(f);
                    }
                }
                Value::String(clip(value.clone(), shape.len))
            }
            MaskRule::Hash => {
                let d = self.digest(rule, v);
                if shape.numeric || v.is_number() {
                    json!(u32::from_le_bytes([d[0], d[1], d[2], d[3]]) & 0x7fff_ffff)
                } else {
                    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
                    Value::String(clip(hex, shape.len))
                }
            }
            MaskRule::ShiftDate { days } => shift_date(&mut self.rng(rule, v), v, days.abs()),
            MaskRule::Noise { percent } => noise(&mut self.rng(rule, v), v, percent.abs()),
            MaskRule::Fake { kind } => {
                let mut rng = self.rng(rule, v);
                if *kind == FakeKind::Document || shape.numeric || v.is_number() {
                    return document(&mut rng, v, shape.len);
                }
                // `row` keeps emails apart: two different originals rarely
                // land on the same one.
                let row = rng.next() % 1_000_000;
                kind.generator().and_then(|g| datagen::fake(g, &mut rng, row, shape.len)).unwrap_or_else(|| v.clone())
            }
        }
    }
}

/// Every digit replaced, the rest kept (`20-12345678-9` → `27-48291034-1`).
/// A number stays a number with as many digits.
fn document(rng: &mut Rng, v: &Value, len: Option<usize>) -> Value {
    let digit = |rng: &mut Rng, first: bool| char::from(b'0' + rng.range(if first { 1 } else { 0 }, 9) as u8);
    match v {
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            let n_digits = n.to_string().trim_start_matches('-').len().clamp(1, 18);
            let s: String = (0..n_digits).map(|i| digit(rng, i == 0 && n_digits > 1)).collect();
            json!(s.parse::<i64>().unwrap_or(0))
        }
        Value::Number(_) => json!(rng.range(1, 99_999_999)),
        other => {
            let s = canonical(other);
            if !s.chars().any(|c| c.is_ascii_digit()) {
                let fresh: String = (0..8).map(|i| digit(rng, i == 0)).collect();
                return Value::String(clip(fresh, len));
            }
            Value::String(s.chars().map(|c| if c.is_ascii_digit() { digit(rng, false) } else { c }).collect())
        }
    }
}

/// `YYYY-MM-DD…` with the date moved; whatever follows (time, zone) kept.
fn shift_date(rng: &mut Rng, v: &Value, days: i64) -> Value {
    let Value::String(s) = v else { return v.clone() };
    let Some(head) = s.get(..10) else { return v.clone() };
    let Ok(d) = NaiveDate::parse_from_str(head, "%Y-%m-%d") else { return v.clone() };
    let mut by = rng.range(-days, days);
    if by == 0 && days > 0 {
        by = if rng.next() % 2 == 0 { 1 } else { -1 };
    }
    match d.checked_add_signed(Days::days(by)) {
        Some(n) => Value::String(format!("{}{}", n.format("%Y-%m-%d"), &s[10..])),
        None => v.clone(),
    }
}

/// The number times `1 ± percent/100`; integers stay integers and texts
/// keep their decimals.
fn noise(rng: &mut Rng, v: &Value, percent: f64) -> Value {
    let factor = 1.0 + (rng.float() * 2.0 - 1.0) * percent / 100.0;
    match v {
        Value::Number(n) if n.is_i64() || n.is_u64() => json!((n.as_f64().unwrap_or(0.0) * factor).round() as i64),
        Value::Number(n) => json!(n.as_f64().unwrap_or(0.0) * factor),
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(x) => {
                let decimals = s.trim().split_once('.').map_or(0, |(_, d)| d.len());
                Value::String(format!("{:.*}", decimals, x * factor))
            }
            Err(_) => v.clone(),
        },
        _ => v.clone(),
    }
}

// -- PII detection ----------------------------------------------------------------------

/// Lowercase words of a column name, accents removed: `FechaNacimiento` →
/// `fecha`, `nacimiento`; `e_mail2` → `e`, `mail`.
fn words(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        let c = match ch {
            'á' | 'Á' => 'a',
            'é' | 'É' => 'e',
            'í' | 'Í' => 'i',
            'ó' | 'Ó' => 'o',
            'ú' | 'Ú' | 'ü' | 'Ü' => 'u',
            'ñ' | 'Ñ' => 'n',
            c => c,
        };
        if !c.is_ascii_alphabetic() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Who a `name` / `nombre` column belongs to when it's a person's.
const PERSONISH: &[&str] = &[
    "customer", "client", "cliente", "user", "usuario", "contact", "contacto", "employee", "empleado", "person", "persona", "owner", "titular",
    "full", "completo", "display", "patient", "paciente", "member", "socio", "alumno", "student",
];

/// The masking rule a column gets by default when its name and type say it
/// holds personal data; `None` for the rest. Keys are never suggested: the
/// caller leaves them out.
pub fn suggest(name: &str, data_type: &str) -> Option<MaskRule> {
    let w = words(name);
    if w.is_empty() {
        return None;
    }
    let joined: String = w.concat();
    let has = |list: &[&str]| w.iter().any(|x| list.contains(&x.as_str()));
    let contains = |list: &[&str]| list.iter().any(|x| joined.contains(x));
    let t = data_type.to_ascii_lowercase();
    if t.contains("bool") || t == "bit" {
        return None;
    }
    let numeric = is_numeric(&t);
    let date = is_dateish(&t) && !numeric;
    let text = is_texty(&t) && !date;
    let fake = |kind| Some(MaskRule::Fake { kind });

    // Birth dates: moved, not replaced.
    if contains(&["nacimiento", "birth", "fechanac", "fecnac"]) || has(&["dob"]) {
        return (date || text).then_some(MaskRule::ShiftDate { days: 180 });
    }
    if date {
        return None;
    }
    // Identity documents, bank accounts and cards: same shape, other digits.
    if has(&["dni", "documento", "document", "passport", "pasaporte", "cuit", "cuil", "nif", "nie", "rut", "cpf", "ssn", "curp", "ruc", "iban", "cbu", "cvu", "card", "tarjeta", "ccnum"])
        || contains(&["nrodoc", "numdoc", "taxid", "nationalid", "cardnumber", "creditcard", "accountnumber", "nrocuenta", "numerocuenta"])
    {
        return (text || numeric).then(|| MaskRule::Fake { kind: FakeKind::Document });
    }
    if has(&["phone", "telefono", "tel", "celular", "movil", "mobile", "cell", "fax", "whatsapp"]) || contains(&["phone", "telefono"]) {
        return if numeric { fake(FakeKind::Document) } else if text { fake(FakeKind::Phone) } else { None };
    }
    if !text {
        return None;
    }
    if has(&["password", "passwd", "pwd", "clave", "contrasena", "secret", "token"]) {
        return Some(MaskRule::Hash);
    }
    if has(&["email", "mail", "correo"]) || contains(&["email", "correo"]) {
        return fake(FakeKind::Email);
    }
    if contains(&["firstname", "givenname", "nombrepila"]) || has(&["nombres"]) {
        return fake(FakeKind::FirstName);
    }
    if contains(&["lastname", "surname", "apellido", "familyname"]) {
        return fake(FakeKind::LastName);
    }
    if has(&["address", "addr", "direccion", "calle", "domicilio", "street"]) || contains(&["address", "direccion", "domicilio"]) {
        return fake(FakeKind::Address);
    }
    let named = has(&["name", "nombre"]);
    if contains(&["fullname", "nombrecompleto"]) || (named && (w.len() == 1 || has(PERSONISH))) {
        return fake(FakeKind::Name);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(len: usize) -> Shape {
        Shape { len: Some(len), numeric: false }
    }

    #[test]
    fn same_value_same_mask_within_a_run() {
        let m = Masker::new(42);
        let rule = MaskRule::Fake { kind: FakeKind::Email };
        let a = m.apply(&rule, &json!("ana@empresa.com"), text(100));
        let b = m.apply(&rule, &json!("ana@empresa.com"), text(100));
        let c = m.apply(&rule, &json!("juan@empresa.com"), text(100));
        assert_eq!(a, b, "the same original masks the same way");
        assert_ne!(a, c);
        assert!(a.as_str().unwrap().contains('@'));
        // Another run, another mask.
        assert_ne!(Masker::new(43).apply(&rule, &json!("ana@empresa.com"), text(100)), a);
        // `12` and `"12"` are the same value (a key read as text on one side).
        let h = MaskRule::Hash;
        assert_eq!(m.apply(&h, &json!(12), Shape { len: None, numeric: true }), m.apply(&h, &json!("12"), Shape { len: None, numeric: true }));
    }

    #[test]
    fn rules_keep_types_and_shapes() {
        let m = Masker::new(7);
        let doc = m.apply(&MaskRule::Fake { kind: FakeKind::Document }, &json!("20-12345678-9"), text(20));
        let s = doc.as_str().unwrap();
        assert_eq!(s.len(), 13);
        assert_eq!((&s[2..3], &s[11..12]), ("-", "-"));
        assert_ne!(s, "20-12345678-9");
        let n = m.apply(&MaskRule::Fake { kind: FakeKind::Document }, &json!(30123456), Shape { len: None, numeric: true });
        assert_eq!(n.as_i64().unwrap().to_string().len(), 8);

        let d = m.apply(&MaskRule::ShiftDate { days: 10 }, &json!("1990-05-17 08:30:00"), text(30));
        let shifted = NaiveDate::parse_from_str(&d.as_str().unwrap()[..10], "%Y-%m-%d").unwrap();
        let by = (shifted - NaiveDate::from_ymd_opt(1990, 5, 17).unwrap()).num_days();
        assert!(by != 0 && by.abs() <= 10, "{by}");
        assert!(d.as_str().unwrap().ends_with(" 08:30:00"));

        let x = m.apply(&MaskRule::Noise { percent: 10.0 }, &json!(1000), Shape::default()).as_i64().unwrap();
        assert!((900..=1100).contains(&x));
        let y = m.apply(&MaskRule::Noise { percent: 10.0 }, &json!("250.50"), text(20));
        assert_eq!(y.as_str().unwrap().split_once('.').unwrap().1.len(), 2);

        assert_eq!(m.apply(&MaskRule::Fixed { value: "7".into() }, &json!(3), Shape { len: None, numeric: true }), json!(7));
        assert_eq!(m.apply(&MaskRule::Fixed { value: "xxxxxx".into() }, &json!("a"), text(3)), json!("xxx"));
        assert_eq!(m.apply(&MaskRule::Null, &json!("a"), text(3)), Value::Null);
        assert_eq!(m.apply(&MaskRule::Fake { kind: FakeKind::Name }, &Value::Null, text(30)), Value::Null, "NULL stays NULL");
        assert_eq!(m.apply(&MaskRule::Hash, &json!("secreto"), text(8)).as_str().unwrap().len(), 8);
        assert!(m.apply(&MaskRule::Fake { kind: FakeKind::Name }, &json!("María Pérez"), text(5)).as_str().unwrap().chars().count() <= 5);
    }

    #[test]
    fn pii_by_name_and_type() {
        let fake = |kind| Some(MaskRule::Fake { kind });
        let cases: &[(&str, &str, Option<MaskRule>)] = &[
            ("email", "varchar(100)", fake(FakeKind::Email)),
            ("CorreoElectronico", "nvarchar(80)", fake(FakeKind::Email)),
            ("nombre", "varchar(50)", fake(FakeKind::Name)),
            ("customer_name", "text", fake(FakeKind::Name)),
            ("first_name", "text", fake(FakeKind::FirstName)),
            ("apellido", "text", fake(FakeKind::LastName)),
            ("telefono", "varchar(20)", fake(FakeKind::Phone)),
            ("celular", "bigint", fake(FakeKind::Document)),
            ("dni", "int", fake(FakeKind::Document)),
            ("nro_cuit", "varchar(13)", fake(FakeKind::Document)),
            ("cbu", "char(22)", fake(FakeKind::Document)),
            ("card_number", "varchar(19)", fake(FakeKind::Document)),
            ("direccion", "varchar(200)", fake(FakeKind::Address)),
            ("FechaNacimiento", "date", Some(MaskRule::ShiftDate { days: 180 })),
            ("birth_date", "timestamp", Some(MaskRule::ShiftDate { days: 180 })),
            ("password", "varchar(64)", Some(MaskRule::Hash)),
            // Not personal data.
            ("product_name", "varchar(50)", None),
            ("table_name", "text", None),
            ("username", "text", None),
            ("total", "decimal(10,2)", None),
            ("creado", "datetime", None),
            ("email_verified", "boolean", None),
            ("hotel", "text", None),
        ];
        for (name, ty, want) in cases {
            assert_eq!(&suggest(name, ty), want, "{name} {ty}");
        }
    }
}
