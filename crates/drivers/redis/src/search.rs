//! What a session reads of the server's vectors (see [`crate::vectors`]):
//! its search indexes (kept for [`crate::FT_TTL`]), the vector fields of a
//! hash, a vector set's shape and elements, and the replies of the editor's
//! commands shaped with what it knows of their vectors.

use crate::shape::{self, text_of, Table};
use crate::vectors::{
    aggregate_table, hybrid_table, knn_capped, quant_flag, search_table, spell, vemb_text,
    vsim_table, FtIndex, FtVector, Server, VType, FT_READS,
};
use crate::RedisSession;
use dbine_driver::{vector, VectorElement, VectorMeta};
use redis::Value;
use serde_json::Value as J;

fn items(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) | Value::Set(a) => a.iter().collect(),
        Value::Map(m) => m.iter().flat_map(|(k, v)| [k, v]).collect(),
        _ => Vec::new(),
    }
}

impl RedisSession {
    /// The search indexes of the session's database (`FT._LIST`, then each
    /// one's `FT.INFO` in one round trip), as read in the last
    /// [`crate::FT_TTL`]. None inside an open `MULTI` (it would queue the
    /// reads), nor where the server's indexes don't cover this database,
    /// nor without a search module.
    pub(crate) async fn ft_indexes(&mut self) -> Vec<FtIndex> {
        if self.multi || !self.server.indexes_in(self.db) {
            return Vec::new();
        }
        if let Some((at, ix)) = &self.ft {
            if at.elapsed() < crate::FT_TTL {
                return ix.clone();
            }
        }
        let names: Vec<String> = match self.run(&[b"FT._LIST"]).await {
            Ok(v) => items(&v).into_iter().map(text_of).collect(),
            Err(_) => Vec::new(),
        };
        let mut out = Vec::new();
        if !names.is_empty() {
            let mut pipe = redis::pipe();
            pipe.ignore_errors();
            for n in &names {
                pipe.cmd("FT.INFO").arg(n);
            }
            if let Ok(replies) = pipe.query_async::<Vec<Value>>(&mut self.conn).await {
                for (n, r) in names.iter().zip(replies) {
                    if let Some(ix) = FtIndex::from_info(n, &r) {
                        out.push(ix);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        self.known().set_indexes(&self.server_id, self.db, &out);
        self.ft = Some((std::time::Instant::now(), out.clone()));
        out
    }

    /// One search index, read now (`FT.INFO`).
    pub(crate) async fn ft_index(&mut self, name: &str) -> Option<FtIndex> {
        if self.multi {
            return None;
        }
        let v = match self.run(&[b"FT.INFO", name.as_bytes()]).await {
            Ok(v) => v,
            Err(e) => {
                // Gone (dropped by another client): the scripts forget it.
                if crate::vectors::no_such_index(&e.to_string()) {
                    self.known().forget_index(&self.server_id, self.db, name);
                    if let Some((_, list)) = &mut self.ft {
                        list.retain(|i| i.name != name);
                    }
                }
                return None;
            }
        };
        let ix = FtIndex::from_info(name, &v)?;
        self.known().put_index(&self.server_id, self.db, &ix);
        if let Some((_, list)) = &mut self.ft {
            match list.iter_mut().find(|i| i.name == ix.name) {
                Some(slot) => *slot = ix.clone(),
                None => list.push(ix.clone()),
            }
        }
        Some(ix)
    }

    /// What a command (that the server took) changes of the search indexes
    /// the session read: an `FT.*` that isn't a read, another database,
    /// or flushed keys. The scripts' registry follows: a dropped index is
    /// forgotten, a created or altered one read again at once.
    pub(crate) async fn ft_after(&mut self, name: &str, cmd: &[Vec<u8>]) {
        let changes = (name.starts_with("FT.") && !FT_READS.contains(&name))
            || matches!(
                name,
                "SELECT" | "FLUSHDB" | "FLUSHALL" | "SWAPDB" | "EXEC" | "DISCARD"
            );
        if !changes {
            return;
        }
        self.ft = None;
        if self.multi {
            // Queued, not run yet: EXEC reads them again.
            return;
        }
        let arg = cmd
            .get(1)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default();
        match name {
            "FT.DROPINDEX" | "FT.DROP" => {
                self.known().forget_index(&self.server_id, self.db, &arg);
            }
            "FT.CREATE" | "FT.ALTER" => {
                self.ft_index(&arg).await;
            }
            // Read again now (a transaction may have created or dropped
            // some): forgetting would leave the scripts guessing.
            "FLUSHDB" | "EXEC" => {
                self.ft_indexes().await;
            }
            "FLUSHALL" | "SWAPDB" => {
                self.known().forget_db(&self.server_id, None);
                self.ft_indexes().await;
            }
            _ => {}
        }
    }

    pub(crate) fn known(&self) -> std::sync::MutexGuard<'_, crate::vectors::Known> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The vector fields of the hash `key` (by hash field name): those of
    /// the search indexes over hashes that cover it.
    pub(crate) async fn hash_vectors(&mut self, key: &[u8]) -> Vec<(String, FtVector)> {
        self.ft_indexes()
            .await
            .into_iter()
            .filter(|i| !i.on_json && i.covers(key))
            .flat_map(|i| {
                i.fields
                    .into_iter()
                    .filter_map(|f| f.vector.map(|v| (f.identifier, v)))
            })
            .collect()
    }

    /// The server has `VRANGE` (asked once).
    pub(crate) async fn has_vrange(&mut self) -> bool {
        if let Some(v) = self.vrange {
            return v;
        }
        if self.multi {
            return false;
        }
        let v = match self.run(&[b"COMMAND", b"INFO", b"VRANGE"]).await {
            Ok(Value::Array(a)) => a.first().is_some_and(|x| !matches!(x, Value::Nil)),
            _ => false,
        };
        self.vrange = Some(v);
        v
    }

    /// A vector set's dimension and quantization (`VDIM`, `VINFO`), the
    /// latter kept for the edit scripts.
    pub(crate) async fn vset_shape(&mut self, key: &[u8]) -> (Option<usize>, Option<String>) {
        if self.multi {
            return (None, None);
        }
        let mut pipe = redis::pipe();
        pipe.ignore_errors();
        pipe.cmd("VDIM").arg(key).cmd("VINFO").arg(key);
        let Ok(r) = pipe.query_async::<Vec<Value>>(&mut self.conn).await else {
            return (None, None);
        };
        let dims = match r.first() {
            Some(Value::Int(n)) if *n > 0 => Some(*n as usize),
            _ => None,
        };
        let quant = r.get(1).and_then(|v| {
            let list = items(v);
            list.chunks(2)
                .find(|kv| text_of(kv[0]) == "quant-type")
                .and_then(|kv| kv.get(1).map(|q| text_of(q)))
        });
        if let Some(flag) = quant.as_deref().and_then(quant_flag) {
            let key = String::from_utf8_lossy(key).into_owned();
            self.known().put_vset(&self.server_id, self.db, &key, flag);
        }
        (dims, quant)
    }

    /// The elements of a vector set with their vector (`VEMB`) and
    /// attributes (`VGETATTR`), in one round trip.
    pub(crate) async fn vset_rows(
        &mut self,
        key: &[u8],
        elements: &[Vec<u8>],
    ) -> Vec<(Vec<u8>, Option<String>, Option<String>)> {
        if elements.is_empty() {
            return Vec::new();
        }
        let mut pipe = redis::pipe();
        pipe.ignore_errors();
        for e in elements {
            pipe.cmd("VEMB").arg(key).arg(e.as_slice());
            pipe.cmd("VGETATTR").arg(key).arg(e.as_slice());
        }
        let r: Vec<Value> = pipe.query_async(&mut self.conn).await.unwrap_or_default();
        elements
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let v = r.get(i * 2).and_then(vemb_text);
                let a = match r.get(i * 2 + 1) {
                    Some(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Some(Value::SimpleString(s)) => Some(s.clone()),
                    Some(Value::VerbatimString { text, .. }) => Some(text.clone()),
                    _ => None,
                };
                (e.clone(), v, a)
            })
            .collect()
    }

    /// An editor command's reply as a table that knows its vectors, or
    /// `None` for the plain shape: search results (`FT.SEARCH`, a vector
    /// field decoded by its index), `FT.AGGREGATE`'s and `FT.HYBRID`'s rows, a vector set's
    /// elements with their vectors (`VRANGE`, `VRANDMEMBER`), `VSIM` and
    /// `VEMB`, and a hash's vector fields (`HGETALL`, `HSCAN`, `HGET`,
    /// `HMGET`, `HRANDFIELD … WITHVALUES`) decoded by the index that
    /// covers the key.
    pub(crate) async fn vector_table(
        &mut self,
        name: &str,
        cmd: &[Vec<u8>],
        reply: &Value,
        max_rows: usize,
    ) -> Option<Table> {
        if self.multi || matches!(reply, Value::ServerError(_)) {
            return None;
        }
        let key = cmd.get(1)?;
        let has = |w: &str| {
            cmd.iter()
                .skip(2)
                .any(|a| a.eq_ignore_ascii_case(w.as_bytes()))
        };
        match name {
            "FT.SEARCH" => {
                let ix = self.ft_index(&String::from_utf8_lossy(key)).await;
                let mut t = search_table(cmd, reply, ix.as_ref());
                if self.server == Server::Valkey {
                    t.partial = knn_capped(cmd, t.rows.len(), max_rows);
                }
                Some(t)
            }
            "FT.AGGREGATE" => {
                let ix = self.ft_index(&String::from_utf8_lossy(key)).await;
                Some(aggregate_table(cmd, reply, ix.as_ref()))
            }
            "FT.HYBRID" => {
                let ix = self.ft_index(&String::from_utf8_lossy(key)).await;
                Some(hybrid_table(cmd, reply, ix.as_ref()))
            }
            // FT.CURSOR READ <index> <cursor>: more of an FT.AGGREGATE.
            "FT.CURSOR" if key.eq_ignore_ascii_case(b"READ") => {
                let ix = match cmd.get(2) {
                    Some(i) => self.ft_index(&String::from_utf8_lossy(i)).await,
                    None => None,
                };
                Some(aggregate_table(&[], reply, ix.as_ref()))
            }
            "VEMB" if !has("RAW") => {
                let text = vemb_text(reply)?;
                let dims = vector::parse(&text)?.len();
                Some(vector_cell_table("vector", text, dims, VType::F32))
            }
            "VSIM" => vsim_table(cmd, reply),
            "VRANGE" | "VRANDMEMBER" => {
                let Value::Array(list) = reply else {
                    return None;
                };
                let elements: Vec<Vec<u8>> = list
                    .iter()
                    .take(max_rows.max(1))
                    .filter_map(|v| match v {
                        Value::BulkString(b) => Some(b.clone()),
                        _ => None,
                    })
                    .collect();
                let (dims, _) = self.vset_shape(key).await;
                let rows = self.vset_rows(key, &elements).await;
                let mut t = Table {
                    columns: vec!["element".into(), "vector".into(), "attributes".into()],
                    types: vec![
                        String::new(),
                        dims.map(|d| spell(d as u32, VType::F32))
                            .unwrap_or_default(),
                        "json".into(),
                    ],
                    vectors: vec![
                        None,
                        Some(VectorMeta::dense(
                            dims.map(|d| d as u32),
                            VectorElement::F32,
                        )),
                        None,
                    ],
                    ..Default::default()
                };
                for (e, v, a) in rows {
                    t.rows.push(vec![
                        shape::cell(&Value::BulkString(e)),
                        v.map_or(J::Null, J::String),
                        a.map_or(J::Null, J::String),
                    ]);
                }
                Some(t)
            }
            "HGETALL" | "HSCAN" | "HGET" | "HMGET" | "HRANDFIELD" => {
                if name == "HRANDFIELD" && !has("WITHVALUES") {
                    return None;
                }
                let fields = self.hash_vectors(key).await;
                if fields.is_empty() {
                    return None;
                }
                hash_table(name, cmd, reply, &fields)
            }
            _ => None,
        }
    }
}

/// One column of a single vector (`VEMB`, `HGET` of a vector field).
fn vector_cell_table(column: &str, text: String, dims: usize, t: VType) -> Table {
    Table {
        columns: vec![column.into()],
        rows: vec![vec![J::String(text)]],
        types: vec![spell(dims as u32, t)],
        vectors: vec![Some(VectorMeta::dense(Some(dims as u32), t.element()))],
        message: None,
        partial: None,
    }
}

/// A hash command's reply with its vector fields (`fields`, by name)
/// decoded to their text.
fn hash_table(
    name: &str,
    cmd: &[Vec<u8>],
    reply: &Value,
    fields: &[(String, FtVector)],
) -> Option<Table> {
    let find = |f: &[u8]| {
        fields
            .iter()
            .find(|(n, _)| n.as_bytes() == f)
            .map(|(_, v)| v)
    };
    let dec = |v: &Value, vf: &FtVector| -> Option<Value> {
        match v {
            Value::BulkString(b) => vf
                .ty
                .decode(b, Some(vf.dims))
                .map(|t| Value::BulkString(t.into_bytes())),
            _ => None,
        }
    };
    // Field / value pairs: flat, nested (RESP3) or a map.
    let decode_pairs = |v: &Value| -> Value {
        match v {
            Value::Map(m) => Value::Map(
                m.iter()
                    .map(|(k, val)| {
                        let f = match k {
                            Value::BulkString(b) => find(b),
                            _ => None,
                        };
                        let val = f.and_then(|vf| dec(val, vf)).unwrap_or(val.clone());
                        (k.clone(), val)
                    })
                    .collect(),
            ),
            Value::Array(a) => {
                let mut out = a.clone();
                let mut i = 0;
                while i + 1 < a.len() {
                    if let Value::BulkString(b) = &a[i] {
                        if let Some(d) = find(b).and_then(|vf| dec(&a[i + 1], vf)) {
                            out[i + 1] = d;
                        }
                    }
                    i += 2;
                }
                Value::Array(out)
            }
            other => other.clone(),
        }
    };
    match name {
        "HGET" => {
            let vf = find(cmd.get(2)?)?;
            let text = match dec(reply, vf)? {
                Value::BulkString(b) => String::from_utf8(b).ok()?,
                _ => return None,
            };
            Some(vector_cell_table("value", text, vf.dims as usize, vf.ty))
        }
        "HMGET" => {
            let Value::Array(a) = reply else { return None };
            let out: Vec<Value> = a
                .iter()
                .zip(cmd.iter().skip(2))
                .map(|(v, f)| find(f).and_then(|vf| dec(v, vf)).unwrap_or(v.clone()))
                .collect();
            Some(shape::shape(cmd, Value::Array(out)))
        }
        "HSCAN" => {
            let Value::Array(parts) = reply else {
                return None;
            };
            if parts.len() != 2 {
                return None;
            }
            let fixed = Value::Array(vec![parts[0].clone(), decode_pairs(&parts[1])]);
            Some(shape::shape(cmd, fixed))
        }
        _ => Some(shape::shape(cmd, decode_pairs(reply))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
    }

    fn emb() -> Vec<(String, FtVector)> {
        vec![(
            "emb".into(),
            FtVector {
                algorithm: "HNSW".into(),
                ty: VType::F32,
                dims: 3,
                metric: "COSINE".into(),
                params: Vec::new(),
            },
        )]
    }

    #[test]
    fn hash_replies_show_their_vectors() {
        let blob = VType::F32.encode(&[0.1, 0.2, 0.3]);
        let b = |s: &[u8]| Value::BulkString(s.to_vec());
        let t = hash_table(
            "HGETALL",
            &args("HGETALL doc:1"),
            &Value::Array(vec![b(b"title"), b(b"hola"), b(b"emb"), b(&blob)]),
            &emb(),
        )
        .unwrap();
        assert_eq!(t.columns, ["field", "value"]);
        assert_eq!(t.rows[1], vec![J::from("emb"), J::from("[0.1,0.2,0.3]")]);
        // RESP3 map.
        let t = hash_table(
            "HGETALL",
            &args("HGETALL doc:1"),
            &Value::Map(vec![(b(b"emb"), b(&blob))]),
            &emb(),
        )
        .unwrap();
        assert_eq!(t.rows[0][1], J::from("[0.1,0.2,0.3]"));
        // HGET: a vector column of its own.
        let t = hash_table("HGET", &args("HGET doc:1 emb"), &b(&blob), &emb()).unwrap();
        assert_eq!(t.types, ["VECTOR(3, FLOAT32)"]);
        assert_eq!(
            t.vectors[0],
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert!(hash_table("HGET", &args("HGET doc:1 title"), &b(b"x"), &emb()).is_none());
        // HMGET and HSCAN.
        let t = hash_table(
            "HMGET",
            &args("HMGET doc:1 title emb"),
            &Value::Array(vec![b(b"hola"), b(&blob)]),
            &emb(),
        )
        .unwrap();
        assert_eq!(t.rows[1][0], J::from("[0.1,0.2,0.3]"));
        let t = hash_table(
            "HSCAN",
            &args("HSCAN doc:1 0"),
            &Value::Array(vec![b(b"0"), Value::Array(vec![b(b"emb"), b(&blob)])]),
            &emb(),
        )
        .unwrap();
        assert_eq!(t.rows[0][1], J::from("[0.1,0.2,0.3]"));
        // A blob of another length stays as it was (hex).
        let t = hash_table(
            "HGETALL",
            &args("HGETALL doc:1"),
            &Value::Array(vec![b(b"emb"), b(&[0xff, 0, 0])]),
            &emb(),
        )
        .unwrap();
        assert_eq!(t.rows[0][1], J::from("0xFF0000"));
    }
}
