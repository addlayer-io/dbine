//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//! `CreateDatabase` takes the `CREATE DATABASE` and extra DDL statements
//! that run with it, atomically (a failing one leaves no database): the
//! options go as `ALTER DATABASE … SET OPTIONS` there. Offered: the version
//! retention period and the default leader. The PostgreSQL dialect isn't:
//! this driver speaks GoogleSQL, so it couldn't use the database it made.
//!
//! Every value is checked before it reaches the DDL.

use crate::{bq, SpannerSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("version_retention", "Retención de versiones (version_retention_period)", FieldKind::Text)
            .placeholder("1h, 3d, 7d")
            .help("Cuánto se puede leer o restaurar hacia atrás: de 1h a 7d. Vacía: 1h."),
        Field::new("default_leader", "Líder por defecto (default_leader)", FieldKind::Text)
            .placeholder("us-east1")
            .help("Solo en instancias multirregión: la región que recibe las escrituras. Vacío: la de la configuración."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// The `CREATE DATABASE` and the statements that run with it.
pub(crate) fn statements(name: &str, o: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut options = Vec::new();
    if let Some(r) = opt(o, "version_retention") {
        let digits = r.trim_end_matches(['s', 'm', 'h', 'd']);
        let ok = !digits.is_empty() && digits.len() + 1 == r.len() && digits.len() <= 6 && digits.chars().all(|c| c.is_ascii_digit());
        if !ok {
            return Err(Error::Query(format!("retención de versiones: «{r}» no es una duración (por ejemplo 1h, 3d, 7d)")));
        }
        options.push(format!("version_retention_period = '{r}'"));
    }
    if let Some(l) = opt(o, "default_leader") {
        if !l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(Error::Query(format!("líder por defecto: «{l}» no es una región")));
        }
        options.push(format!("default_leader = '{l}'"));
    }
    let mut out = vec![format!("CREATE DATABASE {}", bq(name))];
    if !options.is_empty() {
        out.push(format!("ALTER DATABASE {} SET OPTIONS ({})", bq(name), options.join(", ")));
    }
    Ok(out)
}

/// What "Ver script" shows: the statements, separated by `;`.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    Ok(statements(name, o)?.join(";\n"))
}

impl SpannerSession {
    /// The instance configuration's leader options (empty for a regional
    /// one) and Spanner's default retention.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let config = self.api.get(&self.instance).await.ok().and_then(|i| i.get("config").and_then(Json::as_str).map(str::to_string));
        let leaders = match config {
            Some(c) => self
                .api
                .get(&c)
                .await
                .ok()
                .and_then(|c| c.get("leaderOptions").and_then(Json::as_array).cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect(),
            None => Vec::new(),
        };
        Ok(vec![
            FieldChoices { key: "version_retention".into(), default: Some("1h".into()), values: vec!["1h".into(), "1d".into(), "3d".into(), "7d".into()] },
            FieldChoices { key: "default_leader".into(), default: None, values: leaders },
        ])
    }

    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let mut st = statements(name, o)?.into_iter();
        let mut body = json!({ "createStatement": st.next().unwrap_or_default() });
        let extra: Vec<String> = st.collect();
        if !extra.is_empty() {
            body["extraStatements"] = json!(extra);
        }
        let op = self.api.post(&format!("{}/databases", self.instance), &body).await?;
        self.wait(op).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(script("ventas", &o(&[])).unwrap(), "CREATE DATABASE `ventas`");
        assert_eq!(script("ventas", &o(&[("default_leader", " ")])).unwrap(), "CREATE DATABASE `ventas`");
    }

    #[test]
    fn every_option() {
        assert_eq!(
            script("ventas", &o(&[("version_retention", "3d"), ("default_leader", "us-east1")])).unwrap(),
            "CREATE DATABASE `ventas`;\nALTER DATABASE `ventas` SET OPTIONS (version_retention_period = '3d', default_leader = 'us-east1')"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("version_retention", "3"),
            ("version_retention", "d"),
            ("version_retention", "3dd"),
            ("version_retention", "3d'"),
            ("version_retention", "1 h"),
            ("default_leader", "US-EAST1"),
            ("default_leader", "us-east1'"),
        ] {
            assert!(script("v", &o(&[bad])).is_err(), "{bad:?}");
        }
    }
}
