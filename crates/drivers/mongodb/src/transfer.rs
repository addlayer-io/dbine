//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! - Reading: one `find` with large cursor batches. Rows are the top-level
//!   fields, as in the grid. Without an explicit column list, the fields
//!   come from one aggregation over the whole collection (`$objectToArray`),
//!   so a field that only a few documents have is not lost, with the
//!   column types (`int|long`…) of every document; with a column list they
//!   come from a sample the server describes (names, types and sizes, not
//!   the values), and from the documents that have a field when the
//!   sample doesn't. Cursor batches hold ~2 MiB by the largest document
//!   the read can meet (the server's `$bsonSize` over the matched
//!   documents, or the sample's largest when it can't tell), never by an
//!   average: a few big documents after many small ones don't swell a
//!   batch. The column types carry a `bson:` mark (`bson:long`,
//!   `bson:object|bson:null`…) that only this crate writes, so a load knows
//!   they are MongoDB's. A requested field that no
//!   document has is an error, and a name requested twice fills both
//!   positions. Cells keep the BSON type: ObjectId → its 24 hex digits as
//!   text, int / long → integer, double → float, Decimal128 → exact decimal
//!   (any exponent), date → date-time with zone (UTC, milliseconds),
//!   binary → bytes whole (UUID subtype → UUID), nested documents and
//!   arrays → canonical Extended JSON, so every BSON type inside survives
//!   the round trip. Other scalars (Timestamp, regex, code, MinKey…) also go
//!   as canonical Extended JSON. A document the rows can't hold as it is
//!   fails the read, naming its `_id`: a field it repeats (a row has one
//!   value per field), a subdocument that repeats a key, or a subdocument
//!   whose `$`-keys Extended JSON would read as another type (a stored
//!   `{ $date: "…" }` or `{ $numberLong: "7" }`, which a load would turn
//!   into a date or a long).
//! - Loading: unordered `insertMany` of up to [`LOAD_DOCS`] documents or
//!   [`LOAD_BYTES`], up to [`LOAD_CONCURRENCY`] requests and
//!   [`INFLIGHT_BYTES`] in flight. Documents are rebuilt from the cells by
//!   the source column's type: 24 hex digits become an ObjectId only in an
//!   `objectId` column, integers are 64-bit in `long`/`bigint` columns, and
//!   a JSON cell is Extended JSON only when the column has the `bson:` mark
//!   of this crate's reads (from any other engine, even with types called
//!   `object` or `array`, `$`-keys are plain keys and numbers stay exact:
//!   integers by size, a fraction as a double only when the double spells
//!   the same digits and scale, `1.50` or `1e400` as Decimal128). No transactions: every request is durable when it returns,
//!   and progress reports the documents the server acknowledged, the
//!   rejected ones of a failed request excluded. If the load is dropped
//!   (cancelled, or the reader failed), the requests already sent are
//!   killed (`killOp` by the session's `comment`) and waited for, so no
//!   document lands after the drop.
//! - Copy between MongoDB, FerretDB and DocumentDB ([`copy_native`]): the
//!   documents travel as raw BSON, never decoded, with the same loader.
//!   That is the lossless path from MongoDB to MongoDB.
//!
//! What a load from rows can't keep (MongoDB's own limits):
//! - Dates have millisecond precision: finer fractions are cut
//!   (`10:45:00.999999999` → `10:45:00.999`). `TIME` values go as text.
//! - Decimal128 holds 34 significant digits: a longer decimal fails the
//!   load instead of changing its type.
//! - A NULL cell leaves the field out, as `insert_script` does.
//! - Between MongoDB collections by rows (only when the direct copy isn't
//!   available): a column that mixes `objectId` and `string`, or `int` and
//!   `long`, can't be told apart value by value, so the load refuses it.

use super::{err, kill_tagged, shell, MongoSession};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result, Session};
use futures::TryStreamExt;
use mongodb::bson::{doc, oid::ObjectId, spec::BinarySubtype, Binary, Bson, Decimal128, Document, RawBsonRef, RawDocument, RawDocumentBuf};
use mongodb::error::ErrorKind;
use std::collections::HashMap;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

/// Documents per cursor batch at most…
const READ_BATCH: u32 = 10_000;
/// …and about these BSON bytes, by the largest document the read can meet
/// (the server also stops a batch at 16 MiB, and the driver holds it twice).
const READ_BATCH_BYTES: u64 = 2 * 1024 * 1024;
/// The mark on the column types of this crate's reads (`bson:long`): other
/// engines also have types called `object` or `array`, and only a column
/// with this mark holds Extended JSON.
const TYPE_MARK: &str = "bson:";
/// Documents sampled for the column types.
const TYPE_SAMPLE: i64 = 1_000;
/// Sampled documents per cursor batch when the server can't describe them
/// itself (see [`sample`]).
const SAMPLE_BATCH: u32 = 8;
/// Documents per `insertMany`…
const LOAD_DOCS: usize = 10_000;
/// …or BSON bytes, whichever comes first.
const LOAD_BYTES: usize = 2 * 1024 * 1024;
/// `insertMany` requests in flight…
const LOAD_CONCURRENCY: usize = 4;
/// …and at most these BSON bytes among them (one request goes anyway,
/// however big). The driver keeps two or three copies of a request while
/// it's sent: loading 1 MiB rows raises the process's resident memory by
/// about 32 MiB.
const INFLIGHT_BYTES: usize = 4 * 1024 * 1024;
/// MongoDB's largest document.
const MAX_DOC: usize = 16 * 1024 * 1024;
/// A dropped load waits this long at most for its requests (they're
/// killed first, so it's normally far less).
const DROP_WAIT: Duration = Duration::from_secs(120);
/// Longest server text kept in an error.
const ERROR_TEXT: usize = 300;

// ---------------------------------------------------------------- reading

fn parse_filter(filter: Option<&str>) -> Result<Document> {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => match Bson::try_from(shell::parse_value(f).map_err(Error::Query)?) {
            Ok(Bson::Document(d)) => Ok(d),
            _ => Err(Error::Query("El filtro tiene que ser un documento { … }.".into())),
        },
        None => Ok(Document::new()),
    }
}

pub(crate) async fn read_batches(s: &mut MongoSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let coll = s.db.collection::<Document>(&spec.table.name);
    let filter = parse_filter(spec.filter.as_deref())?;

    let Sample { docs: sample, largest } = sample(s, &spec.table.name, &filter).await?;
    // Types found beyond the sample, which win over it; and the largest
    // document the read can meet.
    let (names, found, biggest) = match &spec.columns {
        Some(c) if !c.is_empty() => (c.clone(), check_fields(&coll, c, &sample, &s.tag).await?, largest_doc(&coll, &filter, &s.tag).await),
        _ => all_fields(&coll, &filter, &sample, &s.tag).await?,
    };
    let batch = batch_for(largest.max(biggest.unwrap_or(0)));
    let types = super::convert::infer_columns(&sample);
    let columns: Vec<TransferColumn> = names
        .iter()
        .map(|n| {
            let seen = types.iter().find(|c| c.name == *n);
            TransferColumn {
                name: n.clone(),
                // Every field of the rows has a type, marked as this
                // crate's: a load takes only marked ones for MongoDB's
                // (see `hint`).
                type_name: marked(&found.get(n).cloned().or_else(|| seen.map(|c| c.data_type.clone())).unwrap_or_default()),
                nullable: n != "_id" || seen.is_none_or(|c| c.nullable),
            }
        })
        .collect();
    // A name asked for twice fills every position.
    let mut index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in names.iter().enumerate() {
        index.entry(n.as_str()).or_default().push(i);
    }
    drop(sample);

    // Raw documents, converted field by field: decoding a `Document` through
    // serde would take a stored `{ $date: … }` subdocument for a date.
    let raw_coll = s.db.collection::<RawDocumentBuf>(&spec.table.name);
    let mut cursor = raw_coll.find(filter).batch_size(batch).comment(s.tag.as_str()).await.map_err(err)?;
    let lock_err = |_| Error::State("destino de lotes".into());
    // Locked per call: the guard can't be held across the cursor's awaits.
    sink.lock().map_err(lock_err)?.begin(&columns)?;
    let mut builder = BatchBuilder::new();
    let mut seen = vec![false; names.len()];
    while let Some(d) = cursor.try_next().await.map_err(err)? {
        let mut row = vec![Cell::Null; names.len()];
        seen.fill(false);
        for field in d.iter() {
            let (k, v) = field.map_err(raw_err)?;
            if let Some(at) = index.get(k) {
                // The server keeps a repeated field; a row can't.
                if std::mem::replace(&mut seen[at[0]], true) {
                    return Err(refused(&d, &format!("repite el campo «{k}»")));
                }
                let c = raw_cell(v)?.map_err(|why| refused(&d, &format!("tiene en el campo «{k}» {why}")))?;
                let (last, rest) = at.split_last().expect("one position at least");
                for &i in rest {
                    row[i] = c.clone();
                }
                row[*last] = c;
            }
        }
        builder.push(row, &mut *sink.lock().map_err(lock_err)?)?;
    }
    builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
    Ok(builder.rows)
}

fn raw_err(e: mongodb::bson::raw::Error) -> Error {
    Error::Query(format!("documento BSON inválido: {e}"))
}

/// A document the rows can't hold as it is, named by its `_id`.
fn refused(d: &RawDocument, why: &str) -> Error {
    let id = d.get("_id").ok().flatten().and_then(|v| Bson::try_from(v).ok()).map(|v| clip(&v.into_relaxed_extjson().to_string(), 80));
    Error::Unsupported(format!(
        "El documento con _id {} {why}: por filas se perdería o cambiaría el dato. La copia directa entre bases MongoDB lo conserva tal cual.",
        id.as_deref().unwrap_or("(sin _id)")
    ))
}

/// A raw field value as a cell, or why the cell can't hold it as it is
/// (`Ok(Err(why))`): a subdocument that repeats a key (decoding keeps the
/// last value only), or `$`-keys whose Extended JSON reads back as another
/// value (a stored `{ $date: "…" }` would load as a date).
fn raw_cell(v: RawBsonRef<'_>) -> Result<std::result::Result<Cell, String>> {
    let mut dollar = None;
    if let Err(why) = scan(v, &mut dollar) {
        return Ok(Err(why));
    }
    let b = Bson::try_from(v).map_err(raw_err)?;
    let Some(key) = dollar else {
        return Ok(Ok(cell(b)));
    };
    let c = cell(b.clone());
    if let Cell::Json(s) = &c {
        // What a load of this cell gives back (see `bson`).
        let back = serde_json::from_str::<serde_json::Value>(s).ok().and_then(|j| Bson::try_from(j).ok());
        let bytes = |x: Bson| mongodb::bson::to_vec(&doc! { "v": x }).ok();
        if back.and_then(bytes).is_none_or(|x| Some(x) != bytes(b)) {
            return Ok(Err(format!("subdocumentos con claves como «{key}», que en Extended JSON se leerían como otro tipo de dato")));
        }
    }
    Ok(Ok(c))
}

/// Walks a nested value: a key repeated in one of its documents is an
/// error, and `dollar` gets the first `$`-key.
fn scan(v: RawBsonRef<'_>, dollar: &mut Option<String>) -> std::result::Result<(), String> {
    let d = match v {
        RawBsonRef::Document(d) => d,
        RawBsonRef::JavaScriptCodeWithScope(c) => c.scope,
        RawBsonRef::Array(a) => {
            for x in a {
                scan(x.map_err(|e| format!("un valor BSON inválido ({e})"))?, dollar)?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    let mut keys = Vec::new();
    for f in d {
        let (k, x) = f.map_err(|e| format!("un valor BSON inválido ({e})"))?;
        if dollar.is_none() && k.starts_with('$') {
            *dollar = Some(k.to_string());
        }
        keys.push(k);
        scan(x, dollar)?;
    }
    keys.sort_unstable();
    match keys.windows(2).find(|w| w[0] == w[1]) {
        Some(w) => Err(format!("un subdocumento que repite la clave «{}»", w[0])),
        None => Ok(()),
    }
}

/// A column type with this crate's mark on each part (`long|int` →
/// `bson:long|bson:int`); an empty one stays empty.
fn marked(t: &str) -> String {
    t.split('|').map(str::trim).filter(|p| !p.is_empty()).map(|p| format!("{TYPE_MARK}{p}")).collect::<Vec<_>>().join("|")
}

/// Documents per cursor batch so that one batch of the largest document
/// holds about [`READ_BATCH_BYTES`].
fn batch_for(largest: u64) -> u32 {
    (READ_BATCH_BYTES / largest.max(1)).clamp(1, READ_BATCH as u64) as u32
}

/// The largest document `filter` matches (of the whole collection when
/// `$match` doesn't take the filter), in BSON bytes, by the server; `None`
/// when it can't tell.
async fn largest_doc(coll: &mongodb::Collection<Document>, filter: &Document, tag: &str) -> Option<u64> {
    let mut pipeline = Vec::with_capacity(2);
    if !filter.is_empty() && match_takes(filter) {
        pipeline.push(doc! { "$match": filter.clone() });
    }
    pipeline.push(doc! { "$group": { "_id": Bson::Null, "m": { "$max": { "$bsonSize": "$$ROOT" } } } });
    let found: Vec<Document> = coll.aggregate(pipeline).comment(tag).await.ok()?.try_collect().await.ok()?;
    Some(found.first().map_or(0, |d| group_number(d, "m").max(0) as u64))
}

struct Sample {
    /// Up to [`TYPE_SAMPLE`] documents as skeletons (see [`skeleton`]).
    docs: Vec<Document>,
    /// The largest sampled document, in BSON bytes.
    largest: u64,
}

/// Up to [`TYPE_SAMPLE`] documents `filter` matches, as skeletons: only
/// their field names and types are used, and whole documents of up to
/// 16 MiB each would not fit the read's memory. The server describes them
/// (names, types, size), so the values never travel; a server without
/// those expressions sends the documents a few at a time.
async fn sample(s: &MongoSession, collection: &str, filter: &Document) -> Result<Sample> {
    let coll = s.db.collection::<RawDocumentBuf>(collection);
    let described = doc! {
        "_id": 0,
        "size": { "$bsonSize": "$$ROOT" },
        "fields": { "$map": { "input": { "$objectToArray": "$$ROOT" }, "in": ["$$this.k", { "$type": "$$this.v" }] } },
    };
    let mut docs = Vec::new();
    let mut largest = 0u64;
    let described = async {
        let mut cursor = coll.find(filter.clone()).limit(TYPE_SAMPLE).projection(described).comment(s.tag.as_str()).await?;
        while let Some(d) = cursor.try_next().await? {
            match described_skeleton(&d) {
                Some((size, sk)) => {
                    largest = largest.max(size);
                    docs.push(sk);
                }
                None => return Ok(false),
            }
        }
        Ok::<bool, mongodb::error::Error>(true)
    }
    .await;
    if !matches!(described, Ok(true)) {
        (docs, largest) = (Vec::new(), 0);
        let mut cursor = coll.find(filter.clone()).limit(TYPE_SAMPLE).batch_size(SAMPLE_BATCH).comment(s.tag.as_str()).await.map_err(err)?;
        while let Some(d) = cursor.try_next().await.map_err(err)? {
            largest = largest.max(d.as_bytes().len() as u64);
            docs.push(skeleton(&d)?);
        }
    }
    Ok(Sample { docs, largest })
}

/// A `{ size, fields: [[name, type]…] }` the server made as a skeleton;
/// `None` when it isn't one (or has a type this code doesn't know).
fn described_skeleton(d: &mongodb::bson::RawDocument) -> Option<(u64, Document)> {
    let size = match d.get("size").ok()?? {
        mongodb::bson::RawBsonRef::Int32(n) => n as u64,
        mongodb::bson::RawBsonRef::Int64(n) => n as u64,
        _ => return None,
    };
    let mut out = Document::new();
    for pair in d.get_array("fields").ok()? {
        let pair = pair.ok()?.as_array()?;
        let mut it = pair.into_iter();
        let (k, t) = (it.next()?.ok()?.as_str()?, it.next()?.ok()?.as_str()?);
        out.insert(k, empty_of(t)?);
    }
    Some((size, out))
}

/// An empty value of the BSON type `$type` names.
fn empty_of(t: &str) -> Option<Bson> {
    Some(match t {
        "double" => Bson::Double(0.0),
        "string" => Bson::String(String::new()),
        "object" => Bson::Document(Document::new()),
        "array" => Bson::Array(Vec::new()),
        "binData" => Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: Vec::new() }),
        "undefined" => Bson::Undefined,
        "objectId" => Bson::ObjectId(ObjectId::from_bytes([0; 12])),
        "bool" => Bson::Boolean(false),
        "date" => Bson::DateTime(mongodb::bson::DateTime::from_millis(0)),
        "null" => Bson::Null,
        "regex" => Bson::RegularExpression(mongodb::bson::Regex { pattern: String::new(), options: String::new() }),
        "javascript" | "javascriptWithScope" => Bson::JavaScriptCode(String::new()),
        "symbol" => Bson::Symbol(String::new()),
        "int" => Bson::Int32(0),
        "timestamp" => Bson::Timestamp(mongodb::bson::Timestamp { time: 0, increment: 0 }),
        "long" => Bson::Int64(0),
        "decimal" => Bson::Decimal128(Decimal128::from_bytes([0; 16])),
        "minKey" => Bson::MinKey,
        "maxKey" => Bson::MaxKey,
        "dbPointer" => {
            Bson::try_from(serde_json::json!({ "$dbPointer": { "$ref": "", "$id": { "$oid": "000000000000000000000000" } } })).ok()?
        }
        _ => return None,
    })
}

/// A document's top-level fields in order, each with an empty value of
/// its own BSON type (`""`, `{}`, `[]`, empty binary…): what the column
/// names and types need, without keeping the values.
fn skeleton(d: &mongodb::bson::RawDocument) -> Result<Document> {
    use mongodb::bson::RawBsonRef as R;
    let mut out = Document::new();
    for field in d.iter() {
        let (k, v) = field.map_err(raw_err)?;
        let empty = match v {
            R::String(_) => Bson::String(String::new()),
            R::Document(_) => Bson::Document(Document::new()),
            R::Array(_) => Bson::Array(Vec::new()),
            R::Binary(b) => Bson::Binary(Binary { subtype: b.subtype, bytes: Vec::new() }),
            R::JavaScriptCode(_) | R::JavaScriptCodeWithScope(_) => Bson::JavaScriptCode(String::new()),
            R::Symbol(_) => Bson::Symbol(String::new()),
            R::RegularExpression(_) => Bson::RegularExpression(mongodb::bson::Regex { pattern: String::new(), options: String::new() }),
            // Fixed-size scalars (and the deprecated DBPointer) as they are.
            other => Bson::try_from(other).map_err(raw_err)?,
        };
        out.insert(k, empty);
    }
    Ok(out)
}

/// Requested fields that no document of the collection has are an error
/// (as a missing column is elsewhere). Checked only when the read has rows.
/// The types of the fields outside the sample, from up to [`TYPE_SAMPLE`]
/// documents that have them.
async fn check_fields(coll: &mongodb::Collection<Document>, names: &[String], sample: &[Document], tag: &str) -> Result<Types> {
    let mut types = Types::new();
    if sample.is_empty() {
        return Ok(types);
    }
    let known = super::convert::union_keys(sample);
    for n in names {
        if known.contains(n) || types.contains_key(n) {
            continue;
        }
        let pipeline = vec![
            doc! { "$match": { n.as_str(): { "$exists": true } } },
            doc! { "$limit": TYPE_SAMPLE },
            doc! { "$group": { "_id": { "$type": format!("${n}") }, "n": { "$sum": 1 } } },
        ];
        let found: Vec<Document> = coll.aggregate(pipeline).comment(tag).await.map_err(err)?.try_collect().await.map_err(err)?;
        if found.is_empty() {
            return Err(Error::Query(format!("La colección {} no tiene el campo «{n}».", coll.name())));
        }
        types.insert(n.clone(), joined(found.iter().filter_map(|d| Some((d.get_str("_id").ok()?.to_string(), group_number(d, "n")))).collect()));
    }
    Ok(types)
}

/// Column types by field name.
type Types = HashMap<String, String>;

/// `(type, documents)` pairs as a column type, spelled as `infer_columns`
/// does: the most frequent first, `null` only when there's nothing else.
fn joined(mut seen: Vec<(String, i64)>) -> String {
    seen.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let types: Vec<String> = seen.into_iter().map(|(t, _)| t).filter(|t| t != "null" && t != "undefined").collect();
    if types.is_empty() {
        "null".into()
    } else {
        types.join("|")
    }
}

/// A `$group`'s number (`n: { $sum: 1 }`, `m: { $max: … }`).
fn group_number(d: &Document, key: &str) -> i64 {
    match d.get(key) {
        Some(Bson::Int32(i)) => *i as i64,
        Some(Bson::Int64(i)) => *i,
        Some(Bson::Double(f)) => *f as i64,
        _ => 0,
    }
}

/// Whether an aggregation `$match` takes this filter: `$where`, `$near`
/// and `$nearSphere` only work in `find`.
fn match_takes(filter: &Document) -> bool {
    fn ok(v: &Bson) -> bool {
        match v {
            Bson::Document(d) => match_takes(d),
            Bson::Array(a) => a.iter().all(ok),
            _ => true,
        }
    }
    filter.iter().all(|(k, v)| !matches!(k.as_str(), "$where" | "$near" | "$nearSphere") && ok(v))
}

/// Every top-level field of the documents `filter` matches: `_id` first,
/// then in the order the sample shows them, then the rest by name; and the
/// types each field has in all of them (not just the sample). A filter
/// that `$match` doesn't take (`$where`…) looks at the whole collection:
/// fields the filtered documents lack come out NULL. In the same pass, the
/// largest of those documents in BSON bytes (`None` when the server can't
/// tell).
async fn all_fields(coll: &mongodb::Collection<Document>, filter: &Document, sample: &[Document], tag: &str) -> Result<(Vec<String>, Types, Option<u64>)> {
    let pipeline = |sized: bool| {
        let mut pipeline = Vec::with_capacity(4);
        if !filter.is_empty() && match_takes(filter) {
            pipeline.push(doc! { "$match": filter.clone() });
        }
        let mut project = doc! { "_id": 0, "kv": { "$objectToArray": "$$ROOT" } };
        let mut group = doc! { "_id": { "k": "$kv.k", "t": { "$type": "$kv.v" } }, "n": { "$sum": 1 } };
        if sized {
            project.insert("sz", doc! { "$bsonSize": "$$ROOT" });
            group.insert("m", doc! { "$max": "$sz" });
        }
        pipeline.push(doc! { "$project": project });
        pipeline.push(doc! { "$unwind": "$kv" });
        pipeline.push(doc! { "$group": group });
        pipeline
    };
    let run = |sized: bool| async move { coll.aggregate(pipeline(sized)).allow_disk_use(true).comment(tag).await?.try_collect::<Vec<Document>>().await };
    let (found, sized) = match run(true).await {
        Ok(found) => (found, true),
        // A server without `$bsonSize` (an interrupted one doesn't retry).
        Err(e) if !matches!(e.kind.as_ref(), ErrorKind::Command(c) if c.code == 11601) => (run(false).await.map_err(err)?, false),
        Err(e) => return Err(err(e)),
    };
    let largest = sized.then(|| found.iter().map(|d| group_number(d, "m").max(0) as u64).max().unwrap_or(0));
    let mut seen: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    for d in &found {
        let Ok(id) = d.get_document("_id") else { continue };
        if let (Ok(k), Ok(t)) = (id.get_str("k"), id.get_str("t")) {
            seen.entry(k.to_string()).or_default().push((t.to_string(), group_number(d, "n")));
        }
    }
    let types: Types = seen.into_iter().map(|(k, t)| (k, joined(t))).collect();
    let mut rest: Vec<String> = types.keys().cloned().collect();
    rest.sort();
    let mut names = super::convert::union_keys(sample);
    rest.retain(|k| !names.contains(k));
    if !names.is_empty() && names[0] != "_id" {
        if let Some(i) = rest.iter().position(|k| k == "_id") {
            names.insert(0, rest.remove(i));
        }
    }
    names.extend(rest);
    Ok((names, types, largest))
}

/// A BSON value as a cell.
fn cell(v: Bson) -> Cell {
    match v {
        Bson::Null | Bson::Undefined => Cell::Null,
        Bson::Boolean(b) => Cell::Bool(b),
        Bson::Int32(i) => Cell::Int(i as i64),
        Bson::Int64(i) => Cell::Int(i),
        Bson::Double(f) => Cell::Float(f),
        Bson::String(s) => Cell::Text(s),
        Bson::ObjectId(o) => Cell::Text(o.to_hex()),
        Bson::DateTime(d) => match date_text(d.timestamp_millis()) {
            Some(t) => Cell::DateTimeTz(t),
            None => Cell::Json(Bson::DateTime(d).into_canonical_extjson().to_string()),
        },
        // Every finite decimal, whatever its exponent (at most ~6,200 digits).
        Bson::Decimal128(d) => match plain_decimal(&d.to_string()) {
            Some(t) => Cell::Decimal(t),
            None => Cell::Json(Bson::Decimal128(d).into_canonical_extjson().to_string()),
        },
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => {
            let h: String = b.bytes.iter().map(|x| format!("{x:02x}")).collect();
            Cell::Uuid(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
        }
        Bson::Binary(b) if matches!(b.subtype, BinarySubtype::Generic) => Cell::Bytes(b.bytes),
        other => Cell::Json(other.into_canonical_extjson().to_string()),
    }
}

/// `YYYY-MM-DD HH:MM:SS[.mmm]+00:00`; `None` outside chrono's range.
fn date_text(ms: i64) -> Option<String> {
    let d = DateTime::<Utc>::from_timestamp_millis(ms)?;
    let f = if ms.rem_euclid(1000) == 0 { "%Y-%m-%d %H:%M:%S+00:00" } else { "%Y-%m-%d %H:%M:%S%.3f+00:00" };
    Some(d.format(f).to_string())
}

/// Decimal128 text (`1.5E+3`, `-0.00`…) as plain digits; `None` for NaN
/// and infinities.
fn plain_decimal(s: &str) -> Option<String> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()?),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{int}{frac}");
    let point = int.len() as i64 + exp as i64;
    let mut out = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    // Keep the decimal's own scale (`1.50` stays `1.50`), only drop leading zeros.
    let lead = out.len() - out.trim_start_matches('0').len();
    out.drain(..lead);
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '0');
    }
    Some(if neg { format!("-{out}") } else { out })
}

// ---------------------------------------------------------------- loading

/// How a column's cells turn back into BSON.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Hint {
    /// An `objectId` column: 24 hex digits become an ObjectId.
    object_id: bool,
    /// Integers stay 64-bit even when they fit 32.
    long: bool,
    /// A MongoDB column: JSON cells are canonical Extended JSON.
    extjson: bool,
}

/// BSON type names, as `convert::type_name` and `$type` give them (every
/// one must have an [`empty_of`]).
#[cfg(test)]
const MONGO_TYPES: [&str; 21] = [
    "double", "string", "object", "array", "bindata", "undefined", "objectid", "bool", "date", "null", "regex", "javascript", "symbol", "int",
    "timestamp", "long", "decimal", "minkey", "maxkey", "dbpointer", "javascriptwithscope",
];

/// The column's hint, from its source type. A MongoDB column whose values
/// can't be told apart from the cells is refused.
fn hint(c: &TransferColumn) -> Result<Hint> {
    let t = c.type_name.trim().to_ascii_lowercase();
    let parts: Vec<&str> = t.split('|').map(str::trim).filter(|p| !p.is_empty()).collect();
    // Only a column with this crate's mark is MongoDB's: its reads mark
    // every field's type, while other engines also call types `object` or
    // `array` (Elasticsearch, Snowflake…), and a column without a type
    // (DynamoDB's, a copy's fallback…) comes from elsewhere too. Their JSON
    // is plain: `$`-keys are keys and numbers stay exact.
    let mongo = !parts.is_empty() && parts.iter().all(|p| p.starts_with(TYPE_MARK));
    let bare: Vec<&str> = parts.iter().map(|p| p.strip_prefix(TYPE_MARK).unwrap_or(p)).collect();
    let has = |x: &str| mongo && bare.contains(&x);
    let ambiguous = |what: &str| {
        Err(Error::Unsupported(format!(
            "La columna «{}» mezcla {what}: cargada por filas no se distingue cuál era cuál. La copia directa entre bases MongoDB la conserva tal cual.",
            c.name
        )))
    };
    if has("objectid") && has("string") {
        return ambiguous("objectId y string");
    }
    if has("int") && has("long") {
        return ambiguous("int y long");
    }
    let wide = |p: &&str| matches!(*p, "long" | "int64" | "int8" | "bigint") || p.starts_with("bigint") || p.starts_with("int64");
    Ok(Hint {
        object_id: has("objectid"),
        long: bare.iter().any(wide),
        extjson: mongo,
    })
}

/// A cell as BSON for its column (`None`: the field is left out). The
/// error says why the value can't be stored as it is.
fn bson(cell: Cell, h: Hint) -> std::result::Result<Option<Bson>, String> {
    Ok(Some(match cell {
        Cell::Null => return Ok(None),
        Cell::Bool(b) => Bson::Boolean(b),
        Cell::Int(i) if !h.long && i32::try_from(i).is_ok() => Bson::Int32(i as i32),
        Cell::Int(i) => Bson::Int64(i),
        Cell::UInt(u) => match i64::try_from(u) {
            Ok(i) if !h.long && i32::try_from(i).is_ok() => Bson::Int32(i as i32),
            Ok(i) => Bson::Int64(i),
            Err(_) => Bson::Decimal128(decimal(&u.to_string())?),
        },
        Cell::Float(f) => Bson::Double(f),
        Cell::Decimal(s) => Bson::Decimal128(decimal(&s)?),
        Cell::Text(s) if h.object_id && s.len() == 24 => ObjectId::parse_str(&s).map(Bson::ObjectId).unwrap_or(Bson::String(s)),
        Cell::Text(s) | Cell::Time(s) => Bson::String(s),
        Cell::Bytes(b) => Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: b }),
        Cell::Uuid(s) => match uuid_bytes(&s) {
            Some(bytes) => Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes }),
            None => Bson::String(s),
        },
        Cell::Date(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) => match parse_date(&s) {
            Some(ms) => Bson::DateTime(mongodb::bson::DateTime::from_millis(ms)),
            None => return Err(format!("la fecha «{}» no se puede guardar como fecha de MongoDB", clip(&s, 60))),
        },
        Cell::Json(s) if h.extjson => {
            let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| format!("no es JSON válido ({e})"))?;
            Bson::try_from(v).map_err(|e| format!("no es Extended JSON válido ({e})"))?
        }
        Cell::Json(s) => plain_json(&s)?,
    }))
}

/// An exact decimal as Decimal128, or why it doesn't fit.
fn decimal(s: &str) -> std::result::Result<Decimal128, String> {
    // A long integer's trailing zeros go to the exponent (`1` + 100 zeros →
    // `1E+100`, as MongoDB's own decimal read it).
    let (sign, body) = s.split_at(if s.starts_with(['-', '+']) { 1 } else { 0 });
    let digits = body.trim_start_matches('0');
    let zeros = digits.len() - digits.trim_end_matches('0').len();
    let short;
    let text = if digits.len() > 34 && zeros > 0 && digits.bytes().all(|b| b.is_ascii_digit()) {
        short = format!("{sign}{}E+{zeros}", &digits[..digits.len() - zeros]);
        short.as_str()
    } else {
        s
    };
    Decimal128::from_str(text).map_err(|_| {
        let digits = s.trim_start_matches(['-', '+']).bytes().filter(u8::is_ascii_digit).skip_while(|b| *b == b'0').count();
        format!(
            "el número {} tiene {digits} dígitos significativos o un exponente fuera de rango, y el Decimal128 de MongoDB admite hasta 34 dígitos",
            clip(s, 60)
        )
    })
}

fn uuid_bytes(s: &str) -> Option<Vec<u8>> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    if h.len() != 32 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..32).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect()
}

/// Milliseconds since the epoch: with an offset it's applied, without one
/// the value is UTC. Finer fractions are cut (BSON dates are milliseconds).
fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let t = s.replacen(' ', "T", 1);
    // `…+00:00` / `Z` (also with a space before the offset).
    let tz = t.replace(" +", "+").replace(" -", "-");
    if let Ok(d) = DateTime::parse_from_rfc3339(&tz) {
        return Some(d.timestamp_millis());
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%#z"] {
        if let Ok(d) = DateTime::parse_from_str(&tz, f) {
            return Some(d.timestamp_millis());
        }
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t.trim_end_matches('Z'), f) {
            return Some(d.and_utc().timestamp_millis());
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|d| d.and_utc().timestamp_millis())
}

/// A JSON document from another engine as BSON: keys are plain keys (`$date`
/// means nothing), integers are int / long / Decimal128 by size, and a
/// fraction is a double only when the double gives back the same number
/// (otherwise Decimal128, or an error past 34 digits).
fn plain_json(s: &str) -> std::result::Result<Bson, String> {
    let mut p = Json { s: s.as_bytes(), text: s, i: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err("no es JSON válido (sobra texto al final)".into());
    }
    Ok(v)
}

struct Json<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl Json<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|b| b.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn bad(&self) -> String {
        format!("no es JSON válido (posición {})", self.i)
    }

    fn value(&mut self, depth: usize) -> std::result::Result<Bson, String> {
        // BSON nests at most 100 levels.
        if depth > 100 {
            return Err("el JSON anida más de 100 niveles, el máximo de MongoDB".into());
        }
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut d = Document::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Bson::Document(d));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(self.bad());
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    // A document keeps one value per key: loading a repeated
                    // key would drop the earlier values without a word.
                    if d.contains_key(&k) {
                        return Err(format!("el JSON repite la clave «{k}»"));
                    }
                    d.insert(k, v);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Bson::Document(d));
                        }
                        _ => return Err(self.bad()),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Bson::Array(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Bson::Array(a));
                        }
                        _ => return Err(self.bad()),
                    }
                }
            }
            Some(b'"') => Ok(Bson::String(self.string()?)),
            Some(b't') => self.word("true", Bson::Boolean(true)),
            Some(b'f') => self.word("false", Bson::Boolean(false)),
            Some(b'n') => self.word("null", Bson::Null),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|b| matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                json_number(&self.text[start..self.i])
            }
            _ => Err(self.bad()),
        }
    }

    fn word(&mut self, w: &str, v: Bson) -> std::result::Result<Bson, String> {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(self.bad())
        }
    }

    /// A string literal, escapes decoded by serde_json.
    fn string(&mut self) -> std::result::Result<String, String> {
        if self.s.get(self.i) != Some(&b'"') {
            return Err(self.bad());
        }
        let start = self.i;
        self.i += 1;
        loop {
            match self.s.get(self.i) {
                Some(b'\\') => self.i += 2,
                Some(b'"') => {
                    self.i += 1;
                    break;
                }
                Some(_) => self.i += 1,
                None => return Err(self.bad()),
            }
        }
        serde_json::from_str(&self.text[start..self.i]).map_err(|_| self.bad())
    }
}

/// A JSON number's text as exact BSON: a fraction is a double only when
/// the double spells the same digits and scale (`1.50` and `1e400` are
/// Decimal128).
fn json_number(t: &str) -> std::result::Result<Bson, String> {
    if !is_json_number(t) {
        return Err(format!("no es JSON válido (número «{}»)", clip(t, 60)));
    }
    if !t.contains(['.', 'e', 'E']) {
        if let Ok(i) = t.parse::<i64>() {
            return Ok(i32::try_from(i).map(Bson::Int32).unwrap_or(Bson::Int64(i)));
        }
    } else if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() && scaled(t) == scaled(&format!("{f:e}")) {
            return Ok(Bson::Double(f));
        }
    }
    decimal(t).map(Bson::Decimal128)
}

/// JSON's number grammar (any size: `serde_json::Number` refuses `1e400`).
fn is_json_number(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    if b.get(i) == Some(&b'0') {
        i += 1;
    } else if !digits(&mut i) {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

/// A decimal's sign, digits (trailing zeros kept: they are its scale) and
/// the exponent of its last digit (`0.0120` → `(false, "120", -4)`), to
/// compare two spellings.
fn scaled(t: &str) -> (bool, String, i64) {
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let all = format!("{int}{frac}");
    let digits = match all.trim_start_matches('0') {
        "" => "0",
        d => d,
    };
    (neg, digits.to_string(), exp.saturating_sub(frac.len() as i64))
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// One request's outcome.
struct Sent {
    bytes: usize,
    /// Documents the server acknowledged.
    inserted: u64,
    /// Why it failed, and the row of its first rejection (so the earliest
    /// failure is the one reported, whichever request answers first).
    error: Option<(u64, Error)>,
}

/// What [`Loader::send`] and [`Loader::wait`] return once a request
/// failed; [`Loader::finish`] swaps it for the earliest failure.
const STOPPED: &str = "la carga se detuvo por un envío fallido";

/// Unordered `insertMany`s with bounded concurrency and memory, progress
/// of acknowledged documents, and no request left running when dropped.
struct Loader<'a> {
    coll: mongodb::Collection<RawDocumentBuf>,
    client: mongodb::Client,
    /// The session's `comment`, so its interrupter (and a drop) finds the
    /// requests.
    tag: String,
    progress: Progress<'a>,
    every_rows: u64,
    every_bytes: u64,
    inflight: tokio::task::JoinSet<Sent>,
    inflight_bytes: usize,
    chunk: Vec<RawDocumentBuf>,
    chunk_bytes: usize,
    /// Documents handed over so far (for the row numbers in errors).
    rows: u64,
    done: u64,
    reported: u64,
    bytes_since_report: u64,
    /// The earliest failure so far (by row).
    failed: Option<(u64, Error)>,
}

impl<'a> Loader<'a> {
    fn new(s: &MongoSession, spec: &LoadSpec, progress: Progress<'a>) -> Self {
        Loader {
            coll: s.db.collection::<RawDocumentBuf>(&spec.table.name),
            client: s.client.clone(),
            tag: s.tag.clone(),
            progress,
            every_rows: spec.commit_rows.max(1),
            every_bytes: spec.commit_bytes.max(1),
            inflight: tokio::task::JoinSet::new(),
            inflight_bytes: 0,
            chunk: Vec::new(),
            chunk_bytes: 0,
            rows: 0,
            done: 0,
            reported: 0,
            bytes_since_report: 0,
            failed: None,
        }
    }

    /// Queue one document; a full chunk is sent.
    async fn push(&mut self, doc: RawDocumentBuf) -> Result<()> {
        let size = doc.as_bytes().len();
        self.rows += 1;
        if size > MAX_DOC {
            return Err(Error::Unsupported(format!(
                "La fila {} ocupa {:.1} MiB como documento y MongoDB admite hasta 16 MiB por documento.",
                self.rows,
                size as f64 / (1024.0 * 1024.0)
            )));
        }
        self.chunk.push(doc);
        self.chunk_bytes += size;
        if self.chunk.len() >= LOAD_DOCS || self.chunk_bytes >= LOAD_BYTES {
            self.send().await?;
        }
        Ok(())
    }

    /// Send the chunk once there's room in flight.
    async fn send(&mut self) -> Result<()> {
        while !self.inflight.is_empty() && (self.inflight.len() >= LOAD_CONCURRENCY || self.inflight_bytes + self.chunk_bytes > INFLIGHT_BYTES) {
            let r = self.inflight.join_next().await;
            self.settled(r);
        }
        if self.failed.is_some() {
            return Err(Error::State(STOPPED.into()));
        }
        if self.chunk.is_empty() {
            return Ok(());
        }
        let docs = std::mem::take(&mut self.chunk);
        let bytes = std::mem::take(&mut self.chunk_bytes);
        let first_row = self.rows - docs.len() as u64 + 1;
        self.inflight_bytes += bytes;
        self.inflight.spawn(insert(self.coll.clone(), docs, bytes, first_row, self.tag.clone()));
        Ok(())
    }

    /// `fut`'s output, counting the requests that finish meanwhile, so
    /// progress doesn't wait for the source. A failed request stops it.
    async fn wait<F: Future + Unpin>(&mut self, mut fut: F) -> Result<F::Output> {
        loop {
            if self.failed.is_some() {
                return Err(Error::State(STOPPED.into()));
            }
            if self.inflight.is_empty() {
                return Ok(fut.await);
            }
            let r = tokio::select! {
                biased;
                out = &mut fut => return Ok(out),
                r = self.inflight.join_next() => r,
            };
            self.settled(r);
        }
    }

    fn settled(&mut self, r: Option<std::result::Result<Sent, tokio::task::JoinError>>) {
        let Some(r) = r else { return };
        let sent = r.unwrap_or_else(|e| Sent { bytes: 0, inserted: 0, error: Some((0, Error::State(format!("la carga se interrumpió: {e}")))) });
        self.inflight_bytes = self.inflight_bytes.saturating_sub(sent.bytes);
        self.done += sent.inserted;
        self.bytes_since_report += sent.bytes as u64;
        if let Some(e) = sent.error {
            keep_earliest(&mut self.failed, e);
        }
        if self.done / self.every_rows > self.reported / self.every_rows || self.bytes_since_report >= self.every_bytes {
            self.report();
        }
    }

    fn report(&mut self) {
        (self.progress)(self.done);
        self.reported = self.done;
        self.bytes_since_report = 0;
    }

    /// Send what's left and wait for every request. Also on failure: when
    /// this returns, nothing of the load is still running, and progress
    /// has every acknowledged document.
    async fn finish(&mut self, fed: Result<()>) -> Result<u64> {
        let mut result = match fed {
            Ok(()) => self.send().await,
            Err(e) => Err(e),
        };
        while let Some(r) = self.inflight.join_next().await {
            self.settled(Some(r));
        }
        // Every request has answered: the earliest failure is known.
        if let Some((_, e)) = self.failed.take() {
            if result.is_ok() || matches!(&result, Err(Error::State(m)) if m == STOPPED) {
                result = Err(e);
            }
        }
        if self.done != self.reported || (result.is_ok() && self.done == 0) {
            self.report();
        }
        result.map(|()| self.done)
    }
}

impl Drop for Loader<'_> {
    /// Dropped mid-load (cancelled, or the reader failed): kill the requests
    /// already sent and wait until the server answered each, so no document
    /// lands after the drop (the table is emptied right after a cancel).
    fn drop(&mut self) {
        if self.inflight.is_empty() {
            return;
        }
        let mut set = std::mem::take(&mut self.inflight);
        let (client, tag) = (self.client.clone(), self.tag.clone());
        let settle = async move {
            let _ = tokio::time::timeout(DROP_WAIT, async {
                kill_tagged(client, tag).await;
                while set.join_next().await.is_some() {}
            })
            .await;
        };
        let Ok(h) = tokio::runtime::Handle::try_current() else { return };
        if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
            tokio::task::block_in_place(|| h.block_on(settle));
        } else {
            // A single-threaded runtime can't wait here: the requests are
            // killed and left to finish, never aborted halfway.
            h.spawn(settle);
        }
    }
}

/// Keeps whichever failure has the lower row.
fn keep_earliest(failed: &mut Option<(u64, Error)>, e: (u64, Error)) {
    if failed.as_ref().is_none_or(|(first, _)| e.0 < *first) {
        *failed = Some(e);
    }
}

/// One unordered `insertMany`, tagged for the session's interrupter.
async fn insert(coll: mongodb::Collection<RawDocumentBuf>, docs: Vec<RawDocumentBuf>, bytes: usize, first_row: u64, tag: String) -> Sent {
    let n = docs.len() as u64;
    match coll.insert_many(docs).ordered(false).comment(tag).await {
        Ok(_) => Sent { bytes, inserted: n, error: None },
        Err(e) => {
            let (inserted, row, error) = load_error(e, n, first_row);
            Sent { bytes, inserted, error: Some((row, error)) }
        }
    }
}

/// A failed `insertMany`: the documents it still inserted (unordered, the
/// others go in), the row of its first rejection and a short error about
/// that rejection only.
fn load_error(e: mongodb::error::Error, n: u64, first_row: u64) -> (u64, u64, Error) {
    if let ErrorKind::InsertMany(im) = e.kind.as_ref() {
        let rejected = im.write_errors.as_ref().map_or(0, Vec::len) as u64;
        let inserted = n.saturating_sub(rejected);
        if let Some(w) = im.write_errors.as_ref().and_then(|v| v.iter().min_by_key(|w| w.index)) {
            let row = first_row + w.index as u64;
            let code = w.code_name.clone().unwrap_or_else(|| w.code.to_string());
            let others = if rejected > 1 { format!(" (y {} documentos más de ese envío)", rejected - 1) } else { String::new() };
            return (inserted, row, Error::Query(format!("MongoDB rechazó la fila {row}{others}: {} ({code})", clip(&w.message, ERROR_TEXT))));
        }
        if let Some(w) = &im.write_concern_error {
            return (
                inserted,
                first_row,
                Error::Query(format!("MongoDB no confirmó la escritura de las filas {first_row} a {}: {} ({})", first_row + n - 1, clip(&w.message, ERROR_TEXT), w.code_name)),
            );
        }
    }
    let e = match err(e) {
        Error::Query(m) => Error::Query(clip(&m, ERROR_TEXT)),
        other => other,
    };
    (0, first_row, e)
}

pub(crate) async fn bulk_load(
    s: &mut MongoSession,
    spec: &LoadSpec,
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    s.refuse_if_read_only("cargar datos")?;
    // `columns` are the read's, in the order of the cells (the target's
    // names in `spec.columns` may differ).
    let hints: Vec<Hint> = spec
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let c = if columns.len() == spec.columns.len() { columns.get(i) } else { columns.iter().find(|c| c.name == *name) };
            c.map(hint).unwrap_or(Ok(Hint::default()))
        })
        .collect::<Result<_>>()?;
    let mut loader = Loader::new(s, spec, progress);
    let fed = feed_rows(&mut loader, spec, &hints, source).await;
    loader.finish(fed).await
}

async fn feed_rows(loader: &mut Loader<'_>, spec: &LoadSpec, hints: &[Hint], source: &mut dyn BatchSource) -> Result<()> {
    while let Some(batch) = loader.wait(source.next()).await? {
        for row in batch.rows {
            let mut d = Document::new();
            for ((cell, name), h) in row.into_iter().zip(&spec.columns).zip(hints) {
                let v = bson(cell, *h).map_err(|why| Error::Unsupported(format!("Fila {}, columna «{name}»: {why}.", loader.rows + 1)))?;
                if let Some(v) = v {
                    d.insert(name.clone(), v);
                }
            }
            let raw = RawDocumentBuf::from_document(&d).map_err(|e| Error::Query(format!("Fila {}: {e}", loader.rows + 1)))?;
            loader.push(raw).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- native copy

fn mongo(s: &mut dyn Session) -> Option<&mut MongoSession> {
    s.as_any()?.downcast_mut::<MongoSession>()
}

/// Between two sessions of this crate (MongoDB, FerretDB, DocumentDB): the
/// source's documents as raw BSON, never decoded, so every type, explicit
/// nulls and field order survive. `source` is only read (`find`).
pub(crate) async fn copy_native(source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let unsupported = |why: &str| Err(Error::Unsupported(format!("copia directa no disponible: {why}")));
    let Some(src) = mongo(source) else { return unsupported("el origen no es una sesión de MongoDB") };
    let Some(dst) = mongo(target) else { return unsupported("el destino no es una sesión de MongoDB") };
    dst.refuse_if_read_only("cargar datos")?;

    let filter = parse_filter(spec.source.filter.as_deref())?;
    // (source field, target field), in order; `None`: the whole document.
    let target_cols = &spec.target.columns;
    let pairs: Option<Vec<(String, String)>> = match spec.source.columns.as_ref().filter(|c| !c.is_empty()) {
        Some(from) if target_cols.is_empty() || target_cols.len() == from.len() => {
            let to = if target_cols.is_empty() { from } else { target_cols };
            Some(from.iter().cloned().zip(to.iter().cloned()).collect())
        }
        Some(from) => return Err(Error::State(format!("la copia lee {} columnas y carga {}", from.len(), target_cols.len()))),
        None if !target_cols.is_empty() => Some(target_cols.iter().map(|c| (c.clone(), c.clone())).collect()),
        None => None,
    };
    let docs = src.db.collection::<Document>(&spec.source.table.name);
    // For the batch size (and the requested fields' check).
    let sampled = sample(src, &spec.source.table.name, &filter).await?;
    let mut projection = None;
    if let Some(p) = &pairs {
        let mut seen: Vec<&str> = Vec::new();
        for (_, t) in p {
            if seen.contains(&t.as_str()) {
                return Err(Error::State(format!("la columna de destino «{t}» está repetida")));
            }
            seen.push(t);
        }
        let from: Vec<String> = p.iter().map(|(f, _)| f.clone()).collect();
        check_fields(&docs, &from, &sampled.docs, &src.tag).await?;
        // Only plain names: a dotted one would project a nested path.
        if from.iter().all(|f| !f.contains('.') && !f.starts_with('$')) {
            let mut pr = Document::new();
            for f in &from {
                pr.insert(f.clone(), 1);
            }
            if !from.iter().any(|f| f == "_id") {
                pr.insert("_id", 0);
            }
            projection = Some(pr);
        }
    }

    // Whole documents' size, even with a projection: a bound from above.
    let batch = batch_for(sampled.largest.max(largest_doc(&docs, &filter, &src.tag).await.unwrap_or(0)));
    let coll = src.db.collection::<RawDocumentBuf>(&spec.source.table.name);
    let mut find = coll.find(filter).batch_size(batch).comment(src.tag.as_str());
    if let Some(p) = projection {
        find = find.projection(p);
    }
    let mut cursor = find.await.map_err(err)?;
    let mut loader = Loader::new(dst, &spec.target, progress);
    let fed = async {
        while let Some(raw) = loader.wait(cursor.try_next()).await?.map_err(err)? {
            let doc = match &pairs {
                None => raw,
                Some(p) => {
                    let mut out = RawDocumentBuf::new();
                    for (from, to) in p {
                        if let Some(v) = raw.get(from).map_err(|e| Error::Query(e.to_string()))? {
                            out.append_ref(to, v);
                        }
                    }
                    out
                }
            };
            loader.push(doc).await?;
        }
        Ok::<(), Error>(())
    }
    .await;
    loader.finish(fed).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{Regex, Timestamp};

    const PLAIN: Hint = Hint { object_id: false, long: false, extjson: true };

    fn col(t: &str) -> TransferColumn {
        TransferColumn { name: "x".into(), type_name: t.into(), nullable: true }
    }

    fn round_trip(v: Bson, h: Hint) -> Option<Bson> {
        bson(cell(v), h).unwrap()
    }

    #[test]
    fn scalars_keep_their_type() {
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        assert_eq!(cell(Bson::ObjectId(oid)), Cell::Text("65a1b2c3d4e5f60718293a4b".into()));
        assert_eq!(round_trip(Bson::ObjectId(oid), hint(&col("bson:objectId")).unwrap()), Some(Bson::ObjectId(oid)));
        assert_eq!(round_trip(Bson::Int32(7), PLAIN), Some(Bson::Int32(7)));
        assert_eq!(round_trip(Bson::Int64(7), hint(&col("bson:long")).unwrap()), Some(Bson::Int64(7)));
        assert_eq!(round_trip(Bson::Int64(1 << 40), PLAIN), Some(Bson::Int64(1 << 40)));
        assert_eq!(round_trip(Bson::Double(0.1), PLAIN), Some(Bson::Double(0.1)));
        assert_eq!(round_trip(Bson::Boolean(true), PLAIN), Some(Bson::Boolean(true)));
        assert_eq!(round_trip(Bson::Null, PLAIN), None);
        let dec = Decimal128::from_str("12345678901234567890.1234567890").unwrap();
        assert_eq!(cell(Bson::Decimal128(dec)), Cell::Decimal("12345678901234567890.1234567890".into()));
        assert_eq!(round_trip(Bson::Decimal128(dec), PLAIN), Some(Bson::Decimal128(dec)));
        let d = mongodb::bson::DateTime::from_millis(1_706_708_700_123);
        assert_eq!(cell(Bson::DateTime(d)), Cell::DateTimeTz("2024-01-31 13:45:00.123+00:00".into()));
        assert_eq!(round_trip(Bson::DateTime(d), PLAIN), Some(Bson::DateTime(d)));
        let bin = Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 1, 255] };
        assert_eq!(round_trip(Bson::Binary(bin.clone()), PLAIN), Some(Bson::Binary(bin)));
        let uuid = Binary { subtype: BinarySubtype::Uuid, bytes: (0..16).collect() };
        assert_eq!(cell(Bson::Binary(uuid.clone())), Cell::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into()));
        assert_eq!(round_trip(Bson::Binary(uuid.clone()), PLAIN), Some(Bson::Binary(uuid)));
    }

    #[test]
    fn nested_and_exotic_values_round_trip_as_extended_json() {
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let nested = Bson::Document(doc! {
            "a": [1_i32, 2_i64, 2.5, { "b": oid }],
            "d": mongodb::bson::DateTime::from_millis(5),
            "n": Decimal128::from_str("1.50").unwrap(),
            "bin": Binary { subtype: BinarySubtype::UserDefined(0x80), bytes: vec![1, 2] },
        });
        assert!(matches!(cell(nested.clone()), Cell::Json(ref j) if j.contains("$oid") && j.contains("$numberLong")));
        assert_eq!(round_trip(nested.clone(), PLAIN), Some(nested));
        for v in [
            Bson::Timestamp(Timestamp { time: 5, increment: 2 }),
            Bson::RegularExpression(Regex { pattern: "^a".into(), options: "i".into() }),
            Bson::MinKey,
            Bson::Binary(Binary { subtype: BinarySubtype::Md5, bytes: vec![9; 16] }),
            Bson::Decimal128(Decimal128::from_str("NaN").unwrap()),
        ] {
            assert!(matches!(cell(v.clone()), Cell::Json(_)), "{v:?}");
            assert_eq!(round_trip(v.clone(), PLAIN), Some(v));
        }
    }

    #[test]
    fn decimals_of_any_exponent_are_decimals() {
        for t in ["1E+100", "1E-6000", "-9.999999999999999999999999999999999E+6144", "1.5E-3", "0E-6176"] {
            let d = Decimal128::from_str(t).unwrap();
            assert!(matches!(cell(Bson::Decimal128(d)), Cell::Decimal(_)), "{t}");
            assert_eq!(round_trip(Bson::Decimal128(d), PLAIN), Some(Bson::Decimal128(d)), "{t}");
        }
        assert_eq!(cell(Bson::Decimal128(Decimal128::from_str("1E+3").unwrap())), Cell::Decimal("1000".into()));
    }

    #[test]
    fn decimals_past_34_digits_fail_instead_of_turning_into_text() {
        let e = bson(Cell::Decimal("99999999999999999999999999999999999999".into()), PLAIN).unwrap_err();
        assert!(e.contains("38 dígitos") && e.contains("34"), "{e}");
        assert!(bson(Cell::Decimal("0.3333333333333333333333333333333333333333".into()), PLAIN).is_err());
        assert!(bson(Cell::Decimal("1e400000".into()), PLAIN).is_err());
        // Trailing zeros past 34 digits are exact: they fit.
        assert!(bson(Cell::Decimal("1.000000000000000000000000000000000000000".into()), PLAIN).is_ok());
        assert_eq!(bson(Cell::UInt(u64::MAX), PLAIN), Ok(Some(Bson::Decimal128(Decimal128::from_str("18446744073709551615").unwrap()))));
    }

    #[test]
    fn object_ids_only_in_object_id_columns() {
        let hex = "65a1b2c3d4e5f60718293a4b";
        let oid = Bson::ObjectId(ObjectId::parse_str(hex).unwrap());
        let text = |t: &str, name: &str| {
            let h = hint(&TransferColumn { name: name.into(), type_name: t.into(), nullable: true }).unwrap();
            bson(Cell::Text(hex.into()), h).unwrap().unwrap()
        };
        assert_eq!(text("bson:objectId", "_id"), oid);
        assert_eq!(text("bson:long|bson:objectId", "_id"), oid);
        // A string `_id` stays a string, from MongoDB or any other engine
        // (an unmarked `objectId` type isn't this crate's).
        assert_eq!(text("bson:string", "_id"), Bson::String(hex.into()));
        assert_eq!(text("objectId", "_id"), Bson::String(hex.into()));
        assert_eq!(text("text", "_id"), Bson::String(hex.into()));
        assert_eq!(text("varchar(24)", "ref"), Bson::String(hex.into()));
        let e = hint(&col("bson:objectId|bson:string")).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("objectId y string")), "{e:?}");
    }

    #[test]
    fn integer_width_follows_the_column() {
        assert!(hint(&col("bson:long")).unwrap().long && !hint(&col("bson:int")).unwrap().long);
        assert!(hint(&col("long")).unwrap().long && hint(&col("bigint")).unwrap().long && hint(&col("Int64")).unwrap().long);
        assert!(hint(&col("bson:int|bson:long")).is_err() && hint(&col("bson:long|bson:int")).is_err());
        assert_eq!(bson(Cell::Int(5), hint(&col("bson:long|bson:double")).unwrap()), Ok(Some(Bson::Int64(5))));
        assert_eq!(bson(Cell::Int(5), hint(&col("integer")).unwrap()), Ok(Some(Bson::Int32(5))));
    }

    #[test]
    fn json_from_other_engines_is_plain_and_exact() {
        let h = hint(&col("jsonb")).unwrap();
        assert!(!h.extjson);
        let j = |s: &str| bson(Cell::Json(s.into()), h).unwrap().unwrap();
        assert_eq!(j("{\"a\":1,\"b\":[true,null,\"x\"]}"), Bson::Document(doc! { "a": 1, "b": [true, Bson::Null, "x"] }));
        assert_eq!(j("{\"a\":18446744073709551615}"), Bson::Document(doc! { "a": Decimal128::from_str("18446744073709551615").unwrap() }));
        assert_eq!(j("{\"y\":12345678901234567890123}"), Bson::Document(doc! { "y": Decimal128::from_str("12345678901234567890123").unwrap() }));
        assert_eq!(j("{\"z\":9007199254740993}"), Bson::Document(doc! { "z": 9_007_199_254_740_993_i64 }));
        assert_eq!(j("[0.1, 2.5e3, -0e0, 1.2345678901234567890123]"), {
            Bson::Array(vec![Bson::Double(0.1), Bson::Double(2500.0), Bson::Double(-0.0), Bson::Decimal128(Decimal128::from_str("1.2345678901234567890123").unwrap())])
        });
        // The scale is part of the number: `1.50` isn't the double 1.5.
        let dec = |t: &str| Bson::Decimal128(Decimal128::from_str(t).unwrap());
        assert_eq!(j("[1.50, 2500.0, -0.0, 1e400, 1.5, 100.25, 1E-3]"), {
            Bson::Array(vec![dec("1.50"), dec("2500.0"), dec("-0.0"), dec("1E+400"), Bson::Double(1.5), Bson::Double(100.25), Bson::Double(0.001)])
        });
        for bad in ["01", "1.", ".5", "-", "1e", "+1", "1e+", "--1", "0x1"] {
            let e = bson(Cell::Json(format!("[{bad}]")), h).unwrap_err();
            assert!(e.contains("no es JSON válido"), "{bad}: {e}");
        }
        let e = bson(Cell::Json("[1e400000]".into()), h).unwrap_err();
        assert!(e.contains("exponente fuera de rango"), "{e}");
        // `$`-keys are plain keys.
        assert_eq!(j("{\"when\":{\"$date\":\"2024-01-01T00:00:00Z\"}}"), Bson::Document(doc! { "when": { "$date": "2024-01-01T00:00:00Z" } }));
        assert_eq!(
            j("{\"price\":{\"$numberDecimal\":\"x\"},\"k\":1}"),
            Bson::Document(doc! { "price": { "$numberDecimal": "x" }, "k": 1 })
        );
        assert_eq!(j("\"a\\u00f1\\ud83d\\ude00\\\"\""), Bson::String("añ😀\"".into()));
        assert!(bson(Cell::Json("not json".into()), h).is_err());
        assert!(bson(Cell::Json("{\"a\":1} x".into()), h).is_err());
        assert!(bson(Cell::Json("[0.33333333333333333333333333333333333333]".into()), h).is_err());
        // PostgreSQL's `json` keeps repeated keys; a document can't.
        let dup = bson(Cell::Json("{\"a\":1,\"a\":2}".into()), h).unwrap_err();
        assert!(dup.contains("repite la clave «a»"), "{dup}");
        let nested = bson(Cell::Json("[{\"x\":{\"b\":1,\"c\":2,\"b\":3}}]".into()), h).unwrap_err();
        assert!(nested.contains("repite la clave «b»"), "{nested}");
        // The same key in sibling documents is fine.
        assert_eq!(j("[{\"a\":1},{\"a\":2}]"), Bson::Array(vec![Bson::Document(doc! { "a": 1 }), Bson::Document(doc! { "a": 2 })]));
        // From MongoDB (marked types), Extended JSON keeps its meaning.
        let m = hint(&col("bson:object|bson:null")).unwrap();
        assert_eq!(
            bson(Cell::Json("{\"d\":{\"$date\":{\"$numberLong\":\"5\"}}}".into()), m),
            Ok(Some(Bson::Document(doc! { "d": mongodb::bson::DateTime::from_millis(5) })))
        );
    }

    #[test]
    fn bson_named_types_of_other_engines_are_plain_json() {
        // Elasticsearch's `object`/`array`, Snowflake's `OBJECT`/`ARRAY`…
        for t in ["object", "array", "OBJECT", "ARRAY", "object|array", "date", "decimal", "objectId"] {
            let h = hint(&col(t)).unwrap();
            assert!(!h.extjson && !h.object_id, "{t}");
            let j = |s: &str| bson(Cell::Json(s.into()), h).unwrap().unwrap();
            assert_eq!(j(r#"{"when":{"$date":"2024-01-01T00:00:00Z"}}"#), Bson::Document(doc! { "when": { "$date": "2024-01-01T00:00:00Z" } }), "{t}");
            assert_eq!(j(r#"{"p":{"$numberDecimal":"x"}}"#), Bson::Document(doc! { "p": { "$numberDecimal": "x" } }), "{t}");
            assert_eq!(j(r#"{"y":12345678901234567890123}"#), Bson::Document(doc! { "y": Decimal128::from_str("12345678901234567890123").unwrap() }), "{t}");
            assert_eq!(j(r#"[9007199254740993]"#), Bson::Array(vec![Bson::Int64(9_007_199_254_740_993)]), "{t}");
        }
        // A mix of this crate's types and others' isn't MongoDB's.
        assert!(!hint(&col("bson:object|object")).unwrap().extjson);
        assert_eq!(marked("long|int"), "bson:long|bson:int");
        assert_eq!(marked(" object | null "), "bson:object|bson:null");
        assert_eq!(marked(""), "");
    }

    #[test]
    fn values_rows_cant_hold_are_refused() {
        let cell_of = |d: &RawDocumentBuf, k: &str| raw_cell(d.get(k).unwrap().unwrap()).unwrap();
        // Repeated keys (the server keeps them; a decoded document keeps the
        // last one), inside a subdocument, an array or a code's scope.
        let mut twice = RawDocumentBuf::new();
        twice.append("x", 1);
        twice.append("x", 2);
        let mut d = RawDocumentBuf::new();
        d.append("_id", 1);
        d.append("n", twice.clone());
        let mut arr = mongodb::bson::RawArrayBuf::new();
        arr.push(3);
        arr.push(twice.clone());
        d.append("a", arr);
        d.append("js", mongodb::bson::RawJavaScriptCodeWithScope { code: "x".into(), scope: twice });
        for k in ["n", "a", "js"] {
            let why = cell_of(&d, k).unwrap_err();
            assert!(why.contains("repite la clave «x»"), "{k}: {why}");
        }
        let e = refused(&d, "repite el campo «t»");
        assert!(matches!(&e, Error::Unsupported(m) if m.starts_with("El documento con _id 1 repite el campo «t»") && m.contains("copia directa")), "{e:?}");

        // Stored subdocuments whose `$`-keys Extended JSON would read as a type.
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let stored = doc! {
            "o": { "$date": "2024-01-01T00:00:00Z" },
            "p": { "$oid": "65a1b2c3d4e5f60718293a4b" },
            "q": { "$numberLong": "7" },
            "deep": [{ "k": { "$numberDecimal": "x" } }],
            "ref": { "$ref": "c", "$id": 1 },
            "real": { "d": mongodb::bson::DateTime::from_millis(5), "o": oid, "l": 7_i64 },
        };
        let raw = RawDocumentBuf::from_document(&stored).unwrap();
        for k in ["o", "p", "q", "deep"] {
            let why = cell_of(&raw, k).unwrap_err();
            assert!(why.contains("claves como «$") && why.contains("otro tipo"), "{k}: {why}");
        }
        // `$`-keys that read back the same, and real typed values, travel.
        for k in ["ref", "real"] {
            let c = cell_of(&raw, k).unwrap_or_else(|e| panic!("{k}: {e}"));
            assert_eq!(bson(c, PLAIN).unwrap().as_ref(), stored.get(k), "{k}");
        }
    }

    #[test]
    fn batches_follow_the_largest_document() {
        assert_eq!(batch_for(0), READ_BATCH);
        assert_eq!(batch_for(100), READ_BATCH);
        assert_eq!(batch_for(1024 * 1024), 2);
        assert_eq!(batch_for(16 * 1024 * 1024), 1);
    }

    #[test]
    fn dates_and_other_cells() {
        let ms = |s: &str| match bson(Cell::DateTimeTz(s.into()), PLAIN) {
            Ok(Some(Bson::DateTime(d))) => d.timestamp_millis(),
            other => panic!("{other:?}"),
        };
        assert_eq!(ms("2024-01-31 13:45:00+00:00"), 1_706_708_700_000);
        assert_eq!(ms("2024-01-31 10:45:00-03:00"), 1_706_708_700_000);
        assert_eq!(ms("2024-01-31T13:45:00Z"), 1_706_708_700_000);
        assert_eq!(ms("2024-01-31 13:45:00.5"), 1_706_708_700_500);
        // BSON dates are milliseconds: finer fractions are cut.
        assert_eq!(ms("2024-01-31 13:45:00.999999999"), 1_706_708_700_999);
        assert_eq!(ms("2024-01-31"), 1_706_659_200_000);
        assert!(bson(Cell::DateTime("infinity".into()), PLAIN).is_err());
        assert_eq!(bson(Cell::Text("65a1b2c3d4e5f60718293a4b".into()), PLAIN), Ok(Some(Bson::String("65a1b2c3d4e5f60718293a4b".into()))));
    }

    #[test]
    fn decimals_as_plain_digits() {
        assert_eq!(plain_decimal("1.5E+3").as_deref(), Some("1500"));
        assert_eq!(plain_decimal("1.50").as_deref(), Some("1.50"));
        assert_eq!(plain_decimal("-1.5E-3").as_deref(), Some("-0.0015"));
        assert_eq!(plain_decimal("0E-6176").map(|s| s.len() > 6000), Some(true));
        assert_eq!(plain_decimal("NaN"), None);
        assert_eq!(plain_decimal("Infinity"), None);
    }

    #[test]
    fn filters_that_match_does_not_take() {
        assert!(match_takes(&doc! { "n": { "$gt": 1 } }));
        assert!(!match_takes(&doc! { "$where": "this.n > 1" }));
        assert!(!match_takes(&doc! { "$or": [{ "a": 1 }, { "$where": "true" }] }));
        assert!(!match_takes(&doc! { "loc": { "$near": [0, 0] } }));
    }

    #[test]
    fn untyped_columns_are_not_mongodb() {
        // DynamoDB's columns and a copy's fallback ones have no type: their
        // JSON is plain, whatever keys it has.
        for t in ["", "  ", "|"] {
            let h = hint(&col(t)).unwrap();
            assert!(!h.extjson && !h.object_id && !h.long, "{t:?}");
            let j = |s: &str| bson(Cell::Json(s.into()), h).unwrap().unwrap();
            assert_eq!(j(r#"{"when":{"$date":"2024-01-01T00:00:00Z"}}"#), Bson::Document(doc! { "when": { "$date": "2024-01-01T00:00:00Z" } }));
            assert_eq!(j(r#"{"a":18446744073709551615}"#), Bson::Document(doc! { "a": Decimal128::from_str("18446744073709551615").unwrap() }));
            assert_eq!(j(r#"{"y":12345678901234567890123}"#), Bson::Document(doc! { "y": Decimal128::from_str("12345678901234567890123").unwrap() }));
            assert_eq!(
                j(r#"{"o":{"$oid":"65a1b2c3d4e5f60718293a4b"}}"#),
                Bson::Document(doc! { "o": { "$oid": "65a1b2c3d4e5f60718293a4b" } })
            );
            assert_eq!(j(r#"{"p":{"$numberDecimal":"x"},"k":1}"#), Bson::Document(doc! { "p": { "$numberDecimal": "x" }, "k": 1 }));
        }
        // `$type`'s spelling of code with scope is MongoDB's too.
        assert!(hint(&col("bson:javascriptWithScope")).unwrap().extjson);
    }

    #[test]
    fn column_types_from_aggregated_counts() {
        let t = |v: &[(&str, i64)]| joined(v.iter().map(|(t, n)| (t.to_string(), *n)).collect());
        assert_eq!(t(&[("int", 3), ("long", 5)]), "long|int");
        assert_eq!(t(&[("null", 9), ("string", 1)]), "string");
        assert_eq!(t(&[("null", 2)]), "null");
        assert!(hint(&col(&marked(&t(&[("objectId", 1), ("string", 1)])))).is_err());
    }

    #[test]
    fn sample_skeletons_keep_names_and_types_only() {
        let full = doc! {
            "_id": ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap(),
            "s": "x".repeat(1 << 20),
            "b": Binary { subtype: BinarySubtype::Generic, bytes: vec![7; 1 << 20] },
            "u": Binary { subtype: BinarySubtype::Uuid, bytes: vec![1; 16] },
            "o": { "big": "y".repeat(1 << 20) },
            "a": ["z".repeat(1 << 20)],
            "i": 1_i32, "l": 2_i64, "d": 0.5, "n": Bson::Null,
            "dec": Decimal128::from_str("1.5").unwrap(),
            "t": Timestamp { time: 1, increment: 2 },
            "re": Regex { pattern: "a".repeat(1000), options: "i".into() },
            "js": Bson::JavaScriptCode("f()".repeat(1000)),
            "mk": Bson::MinKey,
        };
        let raw = RawDocumentBuf::from_document(&full).unwrap();
        let sk = skeleton(&raw).unwrap();
        assert!(RawDocumentBuf::from_document(&sk).unwrap().as_bytes().len() < 1024);
        assert_eq!(sk.keys().collect::<Vec<_>>(), full.keys().collect::<Vec<_>>());
        let types = |d: &Document| super::super::convert::infer_columns(std::slice::from_ref(d)).into_iter().map(|c| c.data_type).collect::<Vec<_>>();
        assert_eq!(types(&sk), types(&full));
        assert_eq!(sk.get("u"), Some(&Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes: vec![] })));
    }

    #[test]
    fn server_described_skeletons() {
        // Every `$type` name gives back a value `convert::type_name` spells
        // the same way (code with scope counts as code).
        for t in MONGO_TYPES {
            let spelled = ["binData", "objectId", "minKey", "maxKey", "dbPointer", "javascriptWithScope"].into_iter().find(|s| s.eq_ignore_ascii_case(t)).unwrap_or(t);
            let v = empty_of(spelled).unwrap_or_else(|| panic!("{spelled}"));
            let want = if spelled == "javascriptWithScope" { "javascript" } else { spelled };
            assert_eq!(super::super::convert::type_name(&v), want);
        }
        assert_eq!(empty_of("somethingNew"), None);
        let d = RawDocumentBuf::from_document(&doc! { "size": 1_048_600_i32, "fields": [["_id", "long"], ["s", "string"], ["o", "object"]] }).unwrap();
        let (size, sk) = described_skeleton(&d).unwrap();
        assert_eq!(size, 1_048_600);
        assert_eq!(sk, doc! { "_id": 0_i64, "s": "", "o": {} });
        let unknown = RawDocumentBuf::from_document(&doc! { "size": 5_i32, "fields": [["x", "somethingNew"]] }).unwrap();
        assert!(described_skeleton(&unknown).is_none());
    }

    #[test]
    fn the_earliest_failure_is_reported() {
        let mut failed = None;
        keep_earliest(&mut failed, (20_001, Error::Query("fila 20001".into())));
        keep_earliest(&mut failed, (15_001, Error::Query("fila 15001".into())));
        keep_earliest(&mut failed, (30_001, Error::Query("fila 30001".into())));
        assert!(matches!(failed, Some((15_001, Error::Query(ref m))) if m == "fila 15001"));
        // Within one request, the lowest index, whatever order the server lists.
        let e = |i: i32| doc! { "index": i, "code": 11000, "errmsg": "dup" };
        let im: mongodb::error::InsertManyError = mongodb::bson::from_document(doc! { "writeErrors": [e(7), e(3), e(5)] }).unwrap();
        let (inserted, row, err) = load_error(ErrorKind::InsertMany(im).into(), 10, 101);
        assert_eq!((inserted, row), (7, 104));
        assert!(err.to_string().contains("fila 104"), "{err}");
    }

    #[test]
    fn insert_errors_are_short_and_count_what_went_in() {
        let dup = |i: i32| doc! { "index": i, "code": 11000, "errmsg": format!("E11000 duplicate key error collection: db.c index: _id_ dup key: {{ _id: {i}{} }}", "x".repeat(500)) };
        let write_errors: Vec<Document> = (0..10_000).map(|i| dup(i * 2)).collect();
        let im: mongodb::error::InsertManyError = mongodb::bson::from_document(doc! { "writeErrors": write_errors }).unwrap();
        let (inserted, row, e) = load_error(ErrorKind::InsertMany(im).into(), 20_000, 40_001);
        assert_eq!((inserted, row), (10_000, 40_001));
        let text = e.to_string();
        assert!(text.len() < ERROR_TEXT + 200, "{} bytes", text.len());
        assert!(text.contains("fila 40001") && text.contains("9999 documentos más") && text.contains("11000"), "{text}");
    }
}
