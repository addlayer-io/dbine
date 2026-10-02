//! "Comparar esquemas" against real servers: two indices, one with its own
//! analysis (analyzer, filter, normalizer) and fields that use it, the other
//! plain. The sync script takes the plain one to the first and both then
//! read the same (analysis, fields and their parameters).
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.map_err(|e| format!("{text}: {e}"))?;
    out.error.map_or(Ok(()), |e| Err(format!("{text}: {e}")))
}

const SOURCE: &str = r#"
PUT /dbine_cmp_src
{
  "settings": {
    "analysis": {
      "filter": { "es_stop": { "type": "stop", "stopwords": "_spanish_" } },
      "analyzer": { "es": { "type": "custom", "tokenizer": "standard", "filter": ["lowercase", "es_stop"] } },
      "normalizer": { "low": { "type": "custom", "filter": ["lowercase"] } }
    }
  },
  "mappings": { "properties": { "titulo": { "type": "text", "analyzer": "es" }, "codigo": { "type": "keyword", "normalizer": "low" } } }
}
"#;

const TARGET: &str = r#"
PUT /dbine_cmp_dst
{ "mappings": { "properties": { "otro": { "type": "keyword" } } } }
"#;

fn by_name(schema: &[TableSchema], name: &str) -> TableSchema {
    schema.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("{name}")).clone()
}

async fn compare_and_sync(id: &str, url: &str) {
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let _ = run(&mut s, "DELETE /dbine_cmp_src,dbine_cmp_dst?ignore_unavailable=true").await;
    run(&mut s, SOURCE).await.unwrap();
    run(&mut s, TARGET).await.unwrap();
    let schema = s.database_schema().await.unwrap();
    let src = by_name(&schema, "dbine_cmp_src");
    let dst = by_name(&schema, "dbine_cmp_dst");
    println!("{:?}", src.options);
    assert!(src.options.get("analysis").is_some_and(|a| a.contains("es_stop")));
    // The target takes the source's analysis and fields (as the compare carries them).
    let mut new = dst.clone();
    new.options.insert("analysis".into(), src.options["analysis"].clone());
    new.columns.extend(src.columns.iter().cloned());
    let script = d.sync_script(&[TableChange::Alter { old: dst, new }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st).await.unwrap();
    }
    let after = by_name(&s.database_schema().await.unwrap(), "dbine_cmp_dst");
    assert_eq!(after.options.get("analysis"), src.options.get("analysis"));
    for c in &src.columns {
        assert_eq!(after.columns.iter().find(|x| x.name == c.name), Some(c));
    }
    run(&mut s, "DELETE /dbine_cmp_src,dbine_cmp_dst").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn elasticsearch_compare() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else { return };
    compare_and_sync("elasticsearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opensearch_compare() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else { return };
    compare_and_sync("opensearch", &url).await;
}

// --- "Eliminar" in the compare ---------------------------------------------
//
// The UI drops an element by taking it out of a side's model and sending the
// difference (`changesOf` in CompareView.vue): an index that's gone is a
// `Drop`, one that lost a field or its description an `Alter { old, new }`.
// Each side here is a name prefix on the same server (the compare pairs
// indices by name, so the prefix is taken off to compare the sides).

use std::collections::BTreeMap;

type Model = BTreeMap<String, TableSchema>;

async fn model(s: &mut Box<dyn Session>, prefix: &str) -> Model {
    s.database_schema()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|t| {
            let name = t.name.strip_prefix(prefix)?.to_string();
            Some((name.clone(), TableSchema { name, ..t }))
        })
        .collect()
}

/// Back to the side's real names, to send.
fn real(t: &TableSchema, prefix: &str) -> TableSchema {
    TableSchema { name: format!("{prefix}{}", t.name), ..t.clone() }
}

/// Drops on one side what `edit` takes out of its model; returns the
/// script's warnings and the model read back.
async fn drop_on(d: &dyn dbine_driver::Driver, s: &mut Box<dyn Session>, prefix: &str, edit: impl Fn(&mut Model)) -> (Vec<String>, Model) {
    let orig = model(s, prefix).await;
    let mut work = orig.clone();
    edit(&mut work);
    assert_ne!(work, orig, "the edit drops something");
    let mut tables = Vec::new();
    for (n, t) in &orig {
        match work.get(n) {
            None => tables.push(TableChange::Drop { table: real(t, prefix) }),
            Some(w) if w != t => tables.push(TableChange::Alter { old: real(t, prefix), new: real(w, prefix) }),
            Some(_) => {}
        }
    }
    let script = d.sync_script(&tables).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(s, st).await.unwrap();
    }
    let after = model(s, prefix).await;
    assert_eq!(after, work, "the side reads as its edited model");
    (script.warnings, after)
}

const SIDE: &str = r#"
PUT /{p}pedidos
{
  "mappings": {
    "_meta": { "description": "Pedidos", "owner": "ventas" },
    "properties": { "cliente": { "type": "keyword" }, "extra": { "type": "text" } }
  }
}

PUT /{p}clientes
{ "mappings": { "properties": { "nombre": { "type": "keyword" } } } }

PUT /{p}viejo
{ "mappings": { "properties": { "x": { "type": "long" } } } }
"#;

async fn drop_from_compare(id: &str, url: &str) {
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let d = d.as_ref();
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let (a, b) = ("dbine_drop_a_", "dbine_drop_b_");
    // By name: wildcard deletes are off by default.
    let names: Vec<String> = [a, b].iter().flat_map(|p| ["pedidos", "clientes", "viejo"].map(|t| format!("{p}{t}"))).collect();
    let clean = format!("DELETE /{}?ignore_unavailable=true", names.join(","));
    let clean = clean.as_str();
    let _ = run(&mut s, clean).await;
    for p in [a, b] {
        for req in SIDE.replace("{p}", p).split("\n\n") {
            run(&mut s, req.trim()).await.unwrap();
        }
    }
    assert_eq!(model(&mut s, a).await, model(&mut s, b).await);

    // An index on one side: the sides differ only there.
    let (warnings, ma) = drop_on(d, &mut s, a, |m| {
        m.remove("viejo");
    })
    .await;
    assert!(warnings.iter().any(|w| w.contains("dbine_drop_a_viejo")), "{warnings:?}");
    let mb = model(&mut s, b).await;
    assert!(!ma.contains_key("viejo") && mb.contains_key("viejo"));
    // The same index on the other side: equal again.
    let (_, mb) = drop_on(d, &mut s, b, |m| {
        m.remove("viejo");
    })
    .await;
    assert_eq!(ma, mb);

    // A whole index on both sides.
    for p in [a, b] {
        let (_, m) = drop_on(d, &mut s, p, |m| {
            m.remove("clientes");
        })
        .await;
        assert!(!m.contains_key("clientes"));
    }

    // The description (the props row's "Quitar comentario"), on both sides.
    for p in [a, b] {
        let (_, m) = drop_on(d, &mut s, p, |m| m.get_mut("pedidos").unwrap().comment = None).await;
        assert_eq!(m["pedidos"].comment, None, "{:?}", m["pedidos"]);
        // The rest of `_meta` stays.
        assert!(m["pedidos"].options.get("mappings_extra").is_some_and(|x| x.contains(r#""owner":"ventas""#)), "{:?}", m["pedidos"].options);
    }
    let (ma, mb) = (model(&mut s, a).await, model(&mut s, b).await);
    assert_eq!(ma, mb);

    // A field: a mapping only grows, so it's only a warning.
    let mut work = ma.clone();
    work.get_mut("pedidos").unwrap().columns.retain(|c| c.name != "extra");
    assert_ne!(work, ma, "the field is in the model");
    let script = d.sync_script(&[TableChange::Alter { old: real(&ma["pedidos"], a), new: real(&work["pedidos"], a) }]).unwrap();
    assert!(script.statements.is_empty(), "{script:#?}");
    assert!(script.warnings.iter().any(|w| w.contains("extra")), "{script:#?}");

    run(&mut s, clean).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn elasticsearch_drop() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else { return };
    drop_from_compare("elasticsearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opensearch_drop() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else { return };
    drop_from_compare("opensearch", &url).await;
}
