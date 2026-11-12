//! Pipe plugins: `CREATE PIPEPLUGIN <name> AS '<class>' USING URI '<jar>'`
//! and `DROP PIPEPLUGIN`. `SHOW PIPEPLUGINS` lists the built-in ones
//! (read-only) and the loaded ones with the JAR the server keeps, not the
//! URI it was loaded from: a change has to give the URI again, and drops the
//! plugin and loads it again.

use super::pipe::Clause;
use super::{bad, ident, lit, name_field, shown, Ctx, Plan, Stmt, Vals, NO_PIPE};
use crate::Table;
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
};
use serde_json::{json, Value};

const BUILTIN: &str = "Es un plugin incluido en IoTDB: no se puede cambiar ni borrar.";
const NEED_URI: &str = "El plugin necesita la URI de su JAR.";
const NO_URI: &str = "IoTDB no devuelve la URI del JAR de un plugin: para cambiarlo hay que indicarla de nuevo, y el cambio lo borra y lo vuelve a cargar.";

/// One plugin of `SHOW PIPEPLUGINS`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Plugin {
    /// As the server lists it (uppercase).
    pub(crate) name: String,
    pub(crate) builtin: bool,
    pub(crate) class: String,
    pub(crate) jar: Option<String>,
}

impl Plugin {
    /// The clause a built-in plugin serves, by its class's package; `None`
    /// for a loaded one (any).
    pub(crate) fn role(&self) -> Option<Clause> {
        if !self.builtin {
            return None;
        }
        let c = self.class.to_ascii_lowercase();
        if c.contains(".source.") || c.contains(".extractor.") {
            Some(Clause::Source)
        } else if c.contains(".processor.") {
            Some(Clause::Processor)
        } else if c.contains(".sink.") || c.contains(".connector.") {
            Some(Clause::Sink)
        } else {
            None
        }
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        v => Some(v.to_string()),
    }
}

pub(crate) fn plugins(t: &Table) -> Vec<Plugin> {
    let col = |name: &str| {
        t.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    };
    let Some(n) = col("PluginName") else {
        return Vec::new();
    };
    let (kind, class, jar) = (col("PluginType"), col("ClassName"), col("PluginJar"));
    t.rows
        .iter()
        .filter_map(|r| {
            let name = text(r.get(n))?;
            if name.chars().any(char::is_control) {
                return None;
            }
            Some(Plugin {
                name,
                builtin: kind
                    .and_then(|i| text(r.get(i)))
                    .is_some_and(|k| k.eq_ignore_ascii_case("builtin")),
                class: class.and_then(|i| text(r.get(i))).unwrap_or_default(),
                jar: jar.and_then(|i| text(r.get(i))),
            })
        })
        .collect()
}

pub(crate) fn item(p: &Plugin) -> SchedulerItem {
    SchedulerItem {
        id: format!("plugin:{}", p.name),
        name: p.name.clone(),
        description: Some(p.class.clone()).filter(|c| !c.is_empty()),
        enabled: None,
        system: p.builtin,
    }
}

/// A Java class's full name.
fn class_name(v: &str) -> Result<String> {
    let t = v.trim();
    let part = |p: &str| {
        let mut b = p.bytes();
        b.next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_' || c == b'$')
            && b.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
    };
    if t.is_empty() || t.len() > 500 || !t.split('.').all(part) {
        return Err(bad(format!(
            "«{}» no es el nombre completo de una clase Java (org.ejemplo.MiProcesador).",
            shown(v)
        )));
    }
    Ok(t.to_string())
}

fn plugin_name(v: Vals) -> Result<String> {
    let n = name_field(v, "El plugin")?;
    let ok = n
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        return Err(bad(format!(
            "«{}» no es un nombre de plugin válido (letras, números, guiones).",
            shown(&n)
        )));
    }
    Ok(n)
}

fn uri(v: Vals) -> Result<Option<String>> {
    let Some(u) = v.opt("uri") else {
        return Ok(None);
    };
    if u.chars().any(char::is_control) || u.len() > 2000 {
        return Err(bad(format!("«{}» no es una URI válida.", shown(&u))));
    }
    Ok(Some(u))
}

fn create(name: &str, class: &str, uri: &str) -> Result<Stmt> {
    Ok(Stmt::plain(format!(
        "CREATE PIPEPLUGIN {} AS {} USING URI {}",
        ident(name),
        lit(class, "Clase")?,
        lit(uri, "URI del JAR")?
    )))
}

fn drop(name: &str) -> Stmt {
    Stmt::plain(format!("DROP PIPEPLUGIN {}", ident(name)))
}

pub(crate) fn form(c: &Ctx, current: Option<&Plugin>) -> ObjectForm {
    let mut f = ObjectForm::default();
    let mut uri_field = FormField::new("uri", "URI del JAR", FormKind::Text)
        .placeholder("file:///opt/plugins/mi-plugin.jar")
        .help("Desde donde el servidor carga el JAR (file:// en el servidor, o http(s)://).");
    if current.is_none() {
        uri_field = uri_field.required();
    }
    let mut general = vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("Con este nombre lo usan los pipes ('processor' = 'nombre'). IoTDB no distingue mayúsculas."),
        FormField::new("class_name", "Clase", FormKind::Text)
            .required()
            .placeholder("org.ejemplo.MiProcesador"),
        uri_field,
    ];
    if current.is_some() {
        general.push(FormField::new("type", "Tipo", FormKind::Text).read_only(true));
        general.push(FormField::new("jar", "JAR en el servidor", FormKind::Text).read_only(true));
    }
    f.sections = vec![FormSection::new("general", "General").fields(general)];
    if let Some(p) = current {
        f.values.insert("name".into(), json!(p.name));
        f.values.insert("class_name".into(), json!(p.class));
        f.values.insert(
            "type".into(),
            json!(if p.builtin { "Builtin" } else { "External" }),
        );
        f.values.insert("jar".into(), json!(p.jar));
        if p.builtin {
            f.read_only = true;
            f.notes.push(BUILTIN.into());
            return f;
        }
        f.notes.push(NO_URI.into());
    }
    if !c.pipe {
        f.read_only = true;
        f.notes.insert(0, NO_PIPE.into());
    }
    f
}

pub(crate) fn plan(current: Option<&Plugin>, values: &FormValues) -> Result<Plan> {
    let v = Vals(&values.values);
    let name = plugin_name(v)?;
    let class = class_name(&v.raw("class_name"))?;
    let uri = uri(v)?;
    let id = format!("plugin:{}", name.to_ascii_uppercase());
    let Some(p) = current else {
        let mut plan = Plan::new(&name, id);
        plan.comment(format!("Plugin de pipe «{name}» de IoTDB"));
        match (uri, v.opt("jar")) {
            (Some(uri), _) => {
                plan.push(create(&name, &class, &uri)?);
            }
            // The script of a loaded plugin: IoTDB keeps only its JAR's
            // file name, which stands in for the URI (it can't be saved).
            (None, Some(jar)) => {
                plan.comment(format!(
                    "IoTDB guarda solo el nombre del JAR («{jar}»): reemplazalo por su URI antes de ejecutar el script."
                ));
                let st = create(&name, &class, &jar)?;
                plan.push(Stmt {
                    shown: st.shown,
                    real: Err(NEED_URI.into()),
                    secrets: Vec::new(),
                });
            }
            (None, None) => return Err(bad(NEED_URI.into())),
        }
        return Ok(plan);
    };
    if p.builtin {
        return Err(bad(BUILTIN.into()));
    }
    let mut plan = Plan::new(&p.name, id);
    let renamed = !name.eq_ignore_ascii_case(&p.name);
    if !renamed && class == p.class && uri.is_none() {
        return Ok(plan);
    }
    let uri = uri.ok_or_else(|| bad(NO_URI.into()))?;
    plan.comment(format!("Plugin de pipe «{}» de IoTDB", p.name));
    plan.comment("IoTDB no cambia un plugin cargado: se borra y se vuelve a cargar desde la URI.");
    if renamed {
        plan.comment("Los pipes que lo usan por su nombre anterior dejan de encontrarlo.");
        plan.push(create(&name, &class, &uri)?);
        plan.push(drop(&p.name));
    } else {
        plan.comment("Si la carga falla, el plugin queda borrado: IoTDB no guarda la URI para volver a cargarlo.");
        plan.push(drop(&p.name));
        plan.push(create(&name, &class, &uri)?);
    }
    Ok(plan)
}

pub(crate) fn drop_plan(p: &Plugin) -> Result<Plan> {
    if p.builtin {
        return Err(bad(BUILTIN.into()));
    }
    let mut plan = Plan::new(&p.name, format!("plugin:{}", p.name));
    plan.comment(format!("Borra el plugin de pipe «{}» de IoTDB", p.name));
    plan.push(drop(&p.name));
    Ok(plan)
}
