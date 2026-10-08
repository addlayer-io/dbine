//! "Renombrar…" for keys. `RENAMENX` never overwrites a key that already
//! has the new name, but answers `0` instead of failing; wrapped in an
//! `EVAL`, that `0` becomes an error the script shows. The key keeps its
//! value and its TTL. Nothing on the server names a key (no views, no
//! routines), so there are no dependents to rewrite.

use crate::command::quote_arg;
use dbine_driver::rename::{RenameRequest, RenameSpec, RenameTarget, ReferenceStyle};
use dbine_driver::{kinds, Error, Result, SyncScript};

/// `RENAMENX` that fails when the new key exists (KEYS: old, new).
const RENAME_NX: &str = "if redis.call('RENAMENX', KEYS[1], KEYS[2]) == 0 then \
return redis.error_reply('ERR ya existe una clave con el nombre nuevo: no se pisa') end return 'OK'";

pub fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::KEY.into()],
        references: ReferenceStyle::None,
        note: Some(
            "Renombra la clave con RENAMENX: si ya existe una clave con el nombre nuevo, falla sin pisarla. Conserva el valor y el TTL. \
             El servidor no guarda nada que nombre la clave; lo que la use desde una aplicación hay que cambiarlo a mano."
                .into(),
        ),
        ..Default::default()
    }
}

pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    let old = match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::KEY => object.name.as_str(),
        _ => return Err(Error::Unsupported("en Redis solo se renombran claves".into())),
    };
    let new = req.new_name.as_str();
    let mut warnings = Vec::new();
    let (from, to) = (key_slot(old.as_bytes()), key_slot(new.as_bytes()));
    if from != to {
        warnings.push(format!(
            "En Redis Cluster la clave nueva cae en otro hash slot ({from} → {to}) y el cambio falla (CROSSSLOT); fuera de un cluster no importa. \
             Para que caigan en el mismo slot, repetí la etiqueta entre llaves en los dos nombres, como {{usuario:1}}:perfil."
        ));
    }
    let line = ["EVAL", RENAME_NX, "2", old, new].iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    Ok(SyncScript { statements: vec![line], warnings })
}

/// The cluster hash slot of a key: CRC16 (XMODEM) of the key, or of its
/// hash tag (what's between the first `{` and the next `}`, when not
/// empty), modulo 16384.
pub fn key_slot(key: &[u8]) -> u16 {
    let tagged = key.iter().position(|&b| b == b'{').and_then(|open| {
        let rest = &key[open + 1..];
        rest.iter().position(|&b| b == b'}').filter(|&close| close > 0).map(|close| &rest[..close])
    });
    crc16(tagged.unwrap_or(key)) % 16384
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse_script;
    use dbine_driver::ObjectRef;

    fn req(kind: &str, old: &str, new: &str) -> RenameRequest {
        RenameRequest {
            target: RenameTarget::Object { object: ObjectRef { kind: kind.into(), schema: None, name: old.into() }, parent: None },
            new_name: new.into(),
            table: None,
            definition: None,
        }
    }

    #[test]
    fn key_rename_is_renamenx_in_eval() {
        let s = script(&req(kinds::KEY, "user:1", "User:1 nuevo")).unwrap();
        assert_eq!(s.statements.len(), 1);
        // The line parses back to the exact arguments: the case kept, the
        // space quoted.
        let args = parse_script(&s.statements[0]).unwrap().remove(0);
        let args: Vec<String> = args.into_iter().map(|a| String::from_utf8(a).unwrap()).collect();
        assert_eq!(args, vec!["EVAL".to_string(), RENAME_NX.into(), "2".into(), "user:1".into(), "User:1 nuevo".into()]);
        assert!(s.statements[0].starts_with("EVAL \"if redis.call('RENAMENX'"));
    }

    #[test]
    fn quotes_and_backslashes_survive() {
        let s = script(&req(kinds::KEY, "a\"b", "c\\d")).unwrap();
        let args = parse_script(&s.statements[0]).unwrap().remove(0);
        assert_eq!(args[3], b"a\"b");
        assert_eq!(args[4], b"c\\d");
    }

    #[test]
    fn other_slot_warns() {
        assert!(script(&req(kinds::KEY, "{u:1}:a", "{u:1}:b")).unwrap().warnings.is_empty());
        let w = script(&req(kinds::KEY, "foo", "bar")).unwrap().warnings;
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("(12182 → 5061)"), "{w:?}");
    }

    #[test]
    fn slots_match_redis() {
        // CLUSTER KEYSLOT on a real server.
        assert_eq!(key_slot(b"foo"), 12182);
        assert_eq!(key_slot(b"bar"), 5061);
        assert_eq!(key_slot(b"123456789"), 12739);
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"user1000"));
        // An empty tag hashes the whole key.
        assert_eq!(key_slot(b"{}x"), crc16(b"{}x") % 16384);
    }

    #[test]
    fn only_keys() {
        assert!(matches!(script(&req(kinds::TABLE, "a", "b")), Err(Error::Unsupported(_))));
        let col = RenameRequest {
            target: RenameTarget::Column { table: ObjectRef { kind: kinds::KEY.into(), schema: None, name: "h".into() }, column: "f".into() },
            new_name: "g".into(),
            table: None,
            definition: None,
        };
        assert!(matches!(script(&col), Err(Error::Unsupported(_))));
        let s = spec();
        assert_eq!(s.kinds, vec![kinds::KEY.to_string()]);
        assert!(!s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
    }
}
