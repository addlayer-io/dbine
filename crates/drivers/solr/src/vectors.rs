//! Dense vectors in Solr 9: `solr.DenseVectorField` field types
//! (`vectorDimension`, `vectorEncoding` FLOAT32 or BYTE,
//! `similarityFunction`, `knnAlgorithm`, `hnswMaxConnections`,
//! `hnswBeamWidth`), searched with `{!knn}` and `vectorSimilarity`.
//!
//! - **Type.** A vector field's type lives in a field type of its own, so
//!   DBine spells the column's type as the vector it holds:
//!   `knn_vector(768)` or `knn_vector(768, BYTE)`, and keeps the field
//!   type's name in the column's `fieldType` option. A create adds the
//!   field type (`add-field-type`) before the field; without a name of its
//!   own, the field type is named after what it defines
//!   (`knn_vector_768_cosine_m16_ef100`), so fields alike share one.
//! - **Vector index.** A `DenseVectorField` field is indexed with HNSW
//!   unless `indexed` is off: DBine shows it as a vector index named after
//!   the field, its distance (`similarityFunction`: `euclidean` is `l2`,
//!   `dot_product` and `maximum_inner_product` are `ip`) and parameters
//!   (`hnswMaxConnections` is `m`, `hnswBeamWidth` is `ef_construction`)
//!   from the field type. The field type's own spelling of the distance
//!   goes in the index's options.
//! - **Cells.** The stored vector (a JSON array) as its canonical text.

use dbine_driver::vector::{metric, param};
use dbine_driver::{
    ColumnDef, Error, IndexDef, Result, VectorIndex, VectorIndexSpec, VectorParamSpec,
};
use dbine_driver_elasticsearch::json::{Obj, J};
use dbine_driver_elasticsearch::vectors::{param_json, Elem, VecType};
use std::collections::BTreeMap;

/// The class DBine creates vector field types with.
pub const CLASS: &str = "solr.DenseVectorField";

/// Column option: the name of the vector field's field type.
pub const FIELD_TYPE: &str = "fieldType";

/// Field type keys that aren't part of the vector index.
const NOT_INDEX: &[&str] = &[
    "name",
    "class",
    "vectorDimension",
    "vectorEncoding",
    "similarityFunction",
    "knnAlgorithm",
    "indexed",
    "stored",
    "docValues",
    "multiValued",
    "required",
    "omitNorms",
    "omitTermFreqAndPositions",
    "omitPositions",
    "termVectors",
    "termPositions",
    "termOffsets",
    "termPayloads",
    "storeOffsetsWithPositions",
    "large",
    "uninvertible",
    "tokenized",
    "useDocValuesAsStored",
    "sortMissingFirst",
    "sortMissingLast",
];

/// Solr's names of the standard HNSW parameters.
const PARAMS: &[(&str, &str)] = &[
    ("hnswMaxConnections", param::M),
    ("hnswBeamWidth", param::EF_CONSTRUCTION),
];

/// A field type's class is a dense vector one.
pub fn is_vector_class(class: &str) -> bool {
    class
        .rsplit('.')
        .next()
        .is_some_and(|c| c.ends_with("DenseVectorField"))
}

/// What a vector field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorType {
    pub dims: Option<u32>,
    /// `vectorEncoding: BYTE` (signed bytes), else FLOAT32.
    pub byte: bool,
}

impl VectorType {
    /// DBine's spelling: `knn_vector(3)`, `knn_vector(4, BYTE)`.
    pub fn spelling(&self) -> String {
        match (self.dims, self.byte) {
            (Some(d), true) => format!("knn_vector({d}, BYTE)"),
            (Some(d), false) => format!("knn_vector({d})"),
            (None, true) => "knn_vector(, BYTE)".into(),
            (None, false) => "knn_vector".into(),
        }
    }

    /// The spelling read back: `None` when `ty` isn't one, `Err` when its
    /// arguments are wrong.
    pub fn parse(ty: &str) -> Option<Result<VectorType>> {
        let ty = ty.trim();
        let (base, args) = match ty.split_once('(') {
            Some((b, rest)) => (b.trim(), Some(rest.strip_suffix(')').unwrap_or(rest))),
            None => (ty, None),
        };
        if !base.eq_ignore_ascii_case("knn_vector") {
            return None;
        }
        let bad = || {
            Error::Query(format!(
                "«{ty}»: un vector de Solr se escribe knn_vector(dimensiones) o knn_vector(dimensiones, BYTE)."
            ))
        };
        let Some(args) = args else {
            return Some(Ok(VectorType {
                dims: None,
                byte: false,
            }));
        };
        let mut parts = args.split(',').map(str::trim);
        let dims = match parts.next().filter(|d| !d.is_empty()) {
            None => None,
            Some(d) => match d.parse::<u32>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Some(Err(bad())),
            },
        };
        let byte = match parts.next().map(str::to_ascii_uppercase).as_deref() {
            None | Some("FLOAT32") | Some("FLOAT") => false,
            Some("BYTE") => true,
            Some(_) => return Some(Err(bad())),
        };
        if parts.next().is_some() {
            return Some(Err(bad()));
        }
        Some(Ok(VectorType { dims, byte }))
    }

    /// The vector type of a field type's definition (`schema/fieldtypes`),
    /// if its class is a dense vector one.
    pub fn from_field_type(def: &J) -> Option<VectorType> {
        let class = def.get("class").and_then(J::as_str)?;
        if !is_vector_class(class) {
            return None;
        }
        Some(VectorType {
            dims: def
                .get("vectorDimension")
                .and_then(|d| d.text().parse().ok()),
            byte: def
                .get("vectorEncoding")
                .is_some_and(|e| e.text().eq_ignore_ascii_case("BYTE")),
        })
    }

    /// How results and transfers read its cells.
    pub fn cells(&self) -> VecType {
        let mut t = VecType::new(
            "knn_vector",
            self.dims,
            if self.byte { Elem::Byte } else { Elem::Float },
        );
        t.type_name = self.spelling();
        // Solr gives back the float32 it stores.
        t.single = true;
        t
    }
}

/// A column's vector type (`knn_vector(3)`), if it has one.
pub fn column_type(c: &ColumnDef) -> Result<Option<VectorType>> {
    VectorType::parse(&c.data_type).transpose()
}

fn metric_from(native: &str) -> String {
    match native.trim().to_ascii_lowercase().as_str() {
        "euclidean" => metric::L2.into(),
        "cosine" => metric::COSINE.into(),
        "dot_product" | "maximum_inner_product" => metric::IP.into(),
        other => other.into(),
    }
}

fn metric_to(m: &str) -> Option<&'static str> {
    Some(match m.trim().to_ascii_lowercase().as_str() {
        "l2" | "euclidean" => "euclidean",
        "cosine" => "cosine",
        "ip" | "dot_product" => "dot_product",
        "maximum_inner_product" | "max_inner_product" => "maximum_inner_product",
        _ => return None,
    })
}

/// The vector index of field `field` whose field type is `def`.
pub fn read_index(field: &str, def: &J) -> IndexDef {
    let mut vi = VectorIndex::default();
    let mut options = BTreeMap::new();
    if let Some(s) = def
        .get("similarityFunction")
        .map(J::text)
        .filter(|s| !s.is_empty())
    {
        vi.metric = Some(metric_from(&s));
        options.insert("similarityFunction".to_string(), s);
    }
    vi.algorithm = def
        .get("knnAlgorithm")
        .map(|a| a.text().to_ascii_lowercase())
        .filter(|a| !a.is_empty());
    for (k, v) in def.as_obj().into_iter().flatten() {
        if NOT_INDEX.contains(&k.as_str()) {
            continue;
        }
        let key = PARAMS
            .iter()
            .find(|(n, _)| n == k)
            .map_or(k.as_str(), |(_, s)| *s);
        vi.params.insert(key.to_string(), v.text());
    }
    IndexDef {
        name: field.to_string(),
        columns: vec![field.to_string()],
        options,
        vector: Some(vi),
        ..Default::default()
    }
}

/// The field type a vector column `c` of type `t` needs, with its vector
/// index `ix`: its name (the column's `fieldType`, or one after what it
/// defines) and its `add-field-type` definition.
pub fn field_type(c: &ColumnDef, t: &VectorType, ix: Option<&IndexDef>) -> Result<(String, Obj)> {
    let Some(dims) = t.dims else {
        return Err(Error::Query(format!(
            "Campo {}: Solr necesita la dimensión del vector (knn_vector(n)).",
            c.name.trim()
        )));
    };
    let mut def: Obj = vec![
        ("class".into(), J::Str(CLASS.into())),
        ("vectorDimension".into(), J::Num(dims.into())),
    ];
    let mut name = format!("knn_vector_{dims}");
    if t.byte {
        def.push(("vectorEncoding".into(), J::Str("BYTE".into())));
        name.push_str("_byte");
    }
    if let Some(ix) = ix {
        let vi = ix.vector.clone().unwrap_or_default();
        if let Some(m) = vi
            .metric
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            let standard = metric::ALL.iter().any(|s| s.eq_ignore_ascii_case(m));
            let own = ix
                .options
                .get("similarityFunction")
                .filter(|o| standard && metric_from(o) == metric_from(m) && metric_to(o).is_some());
            let native = match own {
                Some(o) => o.clone(),
                None => metric_to(m)
                    .ok_or_else(|| {
                        Error::Query(format!(
                            "El índice vectorial «{}»: Solr no tiene la distancia «{m}».",
                            ix.name.trim()
                        ))
                    })?
                    .to_string(),
            };
            name.push('_');
            name.push_str(&native.to_ascii_lowercase());
            def.push(("similarityFunction".into(), J::Str(native)));
        }
        if let Some(a) = vi
            .algorithm
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            if !a.eq_ignore_ascii_case("hnsw") {
                name.push('_');
                name.push_str(&a.to_ascii_lowercase());
            }
            def.push(("knnAlgorithm".into(), J::Str(a.to_ascii_lowercase())));
        }
        for (k, v) in vi.params.iter().filter(|(_, v)| !v.trim().is_empty()) {
            let native = PARAMS
                .iter()
                .find(|(_, s)| s == k)
                .map_or(k.as_str(), |(n, _)| *n);
            match k.as_str() {
                param::M => name.push_str(&format!("_m{}", v.trim())),
                param::EF_CONSTRUCTION => name.push_str(&format!("_ef{}", v.trim())),
                _ => {}
            }
            def.push((native.to_string(), param_json(v)));
        }
    }
    let name = match c
        .options
        .get(FIELD_TYPE)
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
    {
        Some(own) => own.to_string(),
        None => name,
    };
    def.insert(0, ("name".into(), J::Str(name.clone())));
    Ok((name, def))
}

/// The vector indexes the designer creates.
pub fn designer_spec() -> VectorIndexSpec {
    let p = |key: &'static str, default: &'static str| VectorParamSpec {
        key,
        algorithms: Vec::new(),
        default,
    };
    VectorIndexSpec {
        algorithms: vec!["hnsw"],
        metrics: vec![
            metric::L2,
            metric::COSINE,
            metric::IP,
            "maximum_inner_product",
        ],
        params: vec![p(param::M, "16"), p(param::EF_CONSTRUCTION, "100")],
        filter_columns: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn spellings() {
        for (s, dims, byte) in [
            ("knn_vector(3)", Some(3), false),
            ("knn_vector(4, BYTE)", Some(4), true),
            ("knn_vector", None, false),
        ] {
            let t = VectorType::parse(s).unwrap().unwrap();
            assert_eq!((t.dims, t.byte), (dims, byte));
            assert_eq!(t.spelling(), s);
        }
        assert_eq!(
            VectorType::parse("KNN_VECTOR(4, byte)")
                .unwrap()
                .unwrap()
                .spelling(),
            "knn_vector(4, BYTE)"
        );
        assert!(VectorType::parse("knn_vector(4, binary)").unwrap().is_err());
        assert!(VectorType::parse("pint").is_none());
        let t = VectorType::from_field_type(&j(
            r#"{"name":"knn_b4","class":"solr.DenseVectorField","vectorDimension":"4","vectorEncoding":"BYTE"}"#,
        ))
        .unwrap();
        assert_eq!(t.spelling(), "knn_vector(4, BYTE)");
        assert_eq!(t.cells().type_name, "knn_vector(4, BYTE)");
        assert!(VectorType::from_field_type(&j(r#"{"class":"solr.StrField"}"#)).is_none());
    }

    #[test]
    fn indexes_and_field_types() {
        let def = j(
            r#"{"name":"knn_b4","class":"solr.DenseVectorField","hnswMaxConnections":"8","knnAlgorithm":"hnsw","vectorDimension":"4","similarityFunction":"euclidean","vectorEncoding":"BYTE","hnswBeamWidth":"50"}"#,
        );
        let ix = read_index("b", &def);
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(vi.metric.as_deref(), Some("l2"));
        assert_eq!(
            vi.params,
            BTreeMap::from([
                ("ef_construction".to_string(), "50".to_string()),
                ("m".to_string(), "8".to_string())
            ])
        );
        let c = ColumnDef {
            name: "b".into(),
            data_type: "knn_vector(4, BYTE)".into(),
            ..Default::default()
        };
        let t = column_type(&c).unwrap().unwrap();
        let (name, d) = field_type(&c, &t, Some(&ix)).unwrap();
        assert_eq!(name, "knn_vector_4_byte_euclidean_ef50_m8");
        assert_eq!(
            J::Obj(d).compact(),
            r#"{"name":"knn_vector_4_byte_euclidean_ef50_m8","class":"solr.DenseVectorField","vectorDimension":4,"vectorEncoding":"BYTE","similarityFunction":"euclidean","knnAlgorithm":"hnsw","hnswBeamWidth":50,"hnswMaxConnections":8}"#
        );
        // The column's own field type name wins; no index: the type alone.
        let mut own = c.clone();
        own.options.insert(FIELD_TYPE.into(), "knn_b4".into());
        let (name, d) = field_type(&own, &t, None).unwrap();
        assert_eq!(name, "knn_b4");
        assert_eq!(
            J::Obj(d).compact(),
            r#"{"name":"knn_b4","class":"solr.DenseVectorField","vectorDimension":4,"vectorEncoding":"BYTE"}"#
        );
        // maximum_inner_product is `ip` and stays itself.
        let ix = read_index(
            "v",
            &j(
                r#"{"class":"solr.DenseVectorField","vectorDimension":"3","similarityFunction":"maximum_inner_product"}"#,
            ),
        );
        assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("ip"));
        let c = ColumnDef {
            name: "v".into(),
            data_type: "knn_vector(3)".into(),
            ..Default::default()
        };
        let t = column_type(&c).unwrap().unwrap();
        assert_eq!(
            field_type(&c, &t, Some(&ix)).unwrap().0,
            "knn_vector_3_maximum_inner_product"
        );
        let mut other = ix.clone();
        other.options.clear();
        assert_eq!(
            field_type(&c, &t, Some(&other)).unwrap().0,
            "knn_vector_3_dot_product"
        );
        other.vector.as_mut().unwrap().metric = Some("l1".into());
        assert!(field_type(&c, &t, Some(&other)).is_err());
        let bare = ColumnDef {
            name: "v".into(),
            data_type: "knn_vector".into(),
            ..Default::default()
        };
        assert!(field_type(&bare, &column_type(&bare).unwrap().unwrap(), None).is_err());
    }
}
