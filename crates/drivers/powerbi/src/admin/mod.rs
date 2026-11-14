//! The model's administration from DBine (decision D4): users' permissions
//! on the model, its name and description, deleting it. Owned by part C.
//! Workspace administration stays out of scope.
//!
//! The Users and permissions tab and "Renombrar…" run scripts through
//! `execute`, so these are the driver's own commands (as Cosmos DB's
//! `GRANT … ON "container"`), one per line, names as DAX strings (`"…"`, a
//! quote doubled), `//` and `--` comments allowed:
//!
//! ```text
//! GRANT Read, Build TO "ana@contoso.com"            -- AS USER (default) | AS GROUP
//! REVOKE Reshare FROM "ana@contoso.com"
//! REVOKE ALL FROM "ana@contoso.com"
//! RENAME MODEL "Ventas" TO "Ventas 2026"
//! ALTER MODEL "Ventas" SET DESCRIPTION "Ventas por día"
//! ```
//!
//! `GRANT` / `REVOKE` act on the session's model (the tab's database);
//! `RENAME` / `ALTER` name the model as `list_databases` does. The new name
//! of `RENAME` is the model's own name: a leading `<workspace> › ` and the
//! ` [<id>]` of a repeated name, which only the listing has, are dropped,
//! and a name that still has ` › ` is refused ([`display_name`]). Rights:
//! `Read`, `Build` (Explore), `Reshare`, `Write`. Power BI keeps **one
//! combined right** per principal (`Read`, `ReadExplore`,
//! `ReadReshareExplore`, `ReadWrite`…, `None`), so [`execute`] reads the
//! principal's current right first (`GET /groups/{g}/datasets/{d}/users`),
//! computes the new combined value and sends it: `POST …/users` for a
//! principal not listed yet, `PUT …/users` otherwise (`None` when `Read`
//! goes or nothing is left). The API neither adds nor removes `Write`, and
//! takes no service principals (`AS APP`): both refused with the reason
//! before any request. Rename and description go to Fabric (`PATCH
//! /workspaces/{g}/semanticModels/{d}`); delete runs directly
//! ([`drop_database`]: `DELETE /groups/{g}/datasets/{d}`).
//!
//! Rules: every command is refused on read-only connections before any
//! request; a unit is either all commands or not one (the splitter gives
//! each command its own unit, `crate::dax::split`); principals and names
//! are validated (no line breaks, length); each id is checked against the
//! listing. The scheduled-task, headless-notebook, "varias bases" and MCP
//! guards count any unit that isn't `DEFINE` / `EVALUATE` as a write
//! (`dbine_driver::read_only::dax_first_write`).
//!
//! Endpoints and permissions (Microsoft Learn):
//!
//! | Command | Request | Scope | The caller needs |
//! |---|---|---|---|
//! | read the current right | `GET …/datasets/{d}/users` | `Dataset.Read.All` | ReadWriteReshare on the model |
//! | grant to someone new | `POST …/datasets/{d}/users` | `Dataset.ReadWrite.All` | ReadReshare (no Write, no apps) |
//! | change or remove | `PUT …/datasets/{d}/users` (`None` removes) | `Dataset.ReadWrite.All` | ReadWriteReshare (no Write, no apps, not what a workspace role gives) |
//! | rename, describe | Fabric `PATCH /workspaces/{g}/semanticModels/{d}` | `SemanticModel.ReadWrite.All` | read and write on the model |
//! | delete | `DELETE /groups/{g}/datasets/{d}` (`/datasets/{d}` in My workspace) | `Dataset.ReadWrite.All` | write on the model |

use crate::catalog::{Model, SEPARATOR};
use crate::http::{Method, Priority};
use crate::meta::service::{
    check_status, named_model, refuse_on_read_only, session_model, Service,
};
use crate::security::{combined, model_users, rights_of, rights_text, workspace_users};
use crate::Ctx;
use dbine_driver::sql::{ScriptStatement, StatementKind};
use dbine_driver::{Error, QueryOutcome, RenameSpec, Result, SecurityAction, SyncScript};
use serde_json::json;
use std::collections::BTreeMap;

/// The first words of the driver's commands. The DAX splitter makes a unit
/// of each line that starts with one (outside a `DEFINE` / `EVALUATE`
/// unit).
pub const ADMIN_VERBS: &[&str] = &["GRANT", "REVOKE", "RENAME", "ALTER"];

/// One of the driver's commands, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// The model's rights (`Read`, `Build`, `Reshare`) to a principal
    /// (`User` or `Group`), added to the right it already has.
    Grant {
        rights: Vec<String>,
        principal: String,
        principal_type: String,
    },
    /// Rights taken from the principal's current right (`ALL` parses to
    /// every right): without `Read`, or with nothing left, it's `None`.
    Revoke {
        rights: Vec<String>,
        principal: String,
    },
    Rename {
        model: String,
        new_name: String,
    },
    SetDescription {
        model: String,
        description: String,
    },
}

/// Whether a unit is one of the driver's commands: its first word, after
/// whitespace and comments, is one of [`ADMIN_VERBS`].
///
/// The word is read by `dbine_driver::read_only::dax_first_write`, the
/// same reading the app's guards (scheduled tasks, headless notebooks,
/// "varias bases", MCP) use to call a unit a query or a write: a unit that
/// runs as a command here is never a query there, by construction.
pub fn is_command(unit: &str) -> bool {
    let unit = ScriptStatement {
        text: unit.into(),
        start: 0,
        end: unit.len(),
        line: 1,
        kind: StatementKind::Sql,
        repeat: 1,
        error: None,
    };
    dbine_driver::read_only::dax_first_write(std::slice::from_ref(&unit))
        .is_some_and(|word| ADMIN_VERBS.contains(&word.as_str()))
}

/// The longest principal or model name taken.
const MAX_NAME: usize = 256;
/// The longest description taken.
const MAX_DESCRIPTION: usize = 4000;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Comma,
    Newline,
}

/// The unit's tokens: words, `"…"` strings (a quote doubled inside, no
/// line break), commas and line ends; comments dropped.
fn lex(unit: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut it = unit.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '\n' => out.push(Tok::Newline),
            c if c.is_whitespace() => {}
            ',' => out.push(Tok::Comma),
            '/' if unit[i..].starts_with("//") => {
                while it.peek().is_some_and(|(_, c)| *c != '\n') {
                    it.next();
                }
            }
            '-' if unit[i..].starts_with("--") => {
                while it.peek().is_some_and(|(_, c)| *c != '\n') {
                    it.next();
                }
            }
            '/' if unit[i..].starts_with("/*") => {
                let Some(end) = unit[i + 2..].find("*/").map(|n| i + 2 + n + 2) else {
                    return Err(Error::Query("Falta cerrar un comentario /* */.".into()));
                };
                while it.peek().is_some_and(|(j, _)| *j < end) {
                    it.next();
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match it.next() {
                        Some((_, '"')) if it.peek().is_some_and(|(_, c)| *c == '"') => {
                            it.next();
                            s.push('"');
                        }
                        Some((_, '"')) => break,
                        Some((_, '\n')) | None => {
                            return Err(Error::Query(
                                "Falta cerrar una cadena entre comillas (y no puede tener saltos de línea).".into(),
                            ))
                        }
                        Some((_, ch)) => s.push(ch),
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_alphanumeric() || c == '_' => {
                let mut w = String::from(c);
                while let Some((_, ch)) = it.peek().copied() {
                    if ch.is_alphanumeric() || ch == '_' {
                        w.push(ch);
                        it.next();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Word(w));
            }
            other => {
                return Err(Error::Query(format!(
                    "«{other}» no se espera en un comando de Power BI."
                )))
            }
        }
    }
    Ok(out)
}

/// A right as written (`read`, `BUILD`, `Explore`) → its name.
fn right(word: &str) -> Option<&'static str> {
    match word.to_ascii_lowercase().as_str() {
        "read" => Some("Read"),
        "build" | "explore" => Some("Build"),
        "reshare" => Some("Reshare"),
        "write" => Some("Write"),
        _ => None,
    }
}

/// Text a command carries: not empty (unless `may_be_empty`), no control
/// characters, not too long.
fn checked(what: &str, s: &str, max: usize, may_be_empty: bool) -> Result<String> {
    if !may_be_empty && s.trim().is_empty() {
        return Err(Error::Query(format!("Falta {what}.")));
    }
    if s.chars().any(char::is_control) {
        return Err(Error::Query(format!(
            "{what} no puede tener saltos de línea ni caracteres de control."
        )));
    }
    if s.chars().count() > max {
        return Err(Error::Query(format!("{what} pasa de {max} caracteres.")));
    }
    Ok(s.to_string())
}

fn usage() -> Error {
    Error::Query(
        "No es un comando de Power BI. Se escriben así:\nGRANT Read, Build TO \"ana@contoso.com\" [AS GROUP]\nREVOKE Reshare FROM \"ana@contoso.com\"   (o REVOKE ALL FROM …)\nRENAME MODEL \"Ventas\" TO \"Ventas 2026\"\nALTER MODEL \"Ventas\" SET DESCRIPTION \"…\"".into(),
    )
}

fn kw(t: Option<&Tok>, word: &str) -> bool {
    matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
}

fn string(t: Option<&Tok>) -> Result<String> {
    match t {
        Some(Tok::Str(s)) => Ok(s.clone()),
        _ => Err(usage()),
    }
}

fn parse_one(toks: &[Tok]) -> Result<Command> {
    let verb = match toks.first() {
        Some(Tok::Word(w)) => w.to_ascii_uppercase(),
        _ => return Err(usage()),
    };
    match verb.as_str() {
        "GRANT" | "REVOKE" => {
            let grant = verb == "GRANT";
            let mut i = 1;
            let mut rights: Vec<String> = Vec::new();
            loop {
                match toks.get(i) {
                    Some(Tok::Word(w))
                        if !grant && w.eq_ignore_ascii_case("ALL") && rights.is_empty() =>
                    {
                        rights = ["Read", "Build", "Reshare", "Write"]
                            .map(String::from)
                            .to_vec();
                        i += 1;
                        break;
                    }
                    Some(Tok::Word(w)) => {
                        let r = right(w).ok_or_else(|| {
                            Error::Query(format!(
                                "«{w}» no es un permiso de un modelo: Read, Build, Reshare o Write."
                            ))
                        })?;
                        if !rights.iter().any(|x| x == r) {
                            rights.push(r.into());
                        }
                        i += 1;
                    }
                    _ => return Err(usage()),
                }
                if matches!(toks.get(i), Some(Tok::Comma)) {
                    i += 1;
                    continue;
                }
                break;
            }
            if !kw(toks.get(i), if grant { "TO" } else { "FROM" }) {
                return Err(usage());
            }
            let principal = checked(
                "el usuario o grupo",
                &string(toks.get(i + 1))?,
                MAX_NAME,
                false,
            )?;
            i += 2;
            let mut principal_type = "User".to_string();
            if grant && kw(toks.get(i), "AS") {
                principal_type = match toks.get(i + 1) {
                    Some(Tok::Word(w)) if w.eq_ignore_ascii_case("USER") => "User".into(),
                    Some(Tok::Word(w)) if w.eq_ignore_ascii_case("GROUP") => "Group".into(),
                    Some(Tok::Word(w)) if w.eq_ignore_ascii_case("APP") => "App".into(),
                    _ => return Err(usage()),
                };
                i += 2;
            }
            if i != toks.len() {
                return Err(usage());
            }
            Ok(if grant {
                Command::Grant {
                    rights,
                    principal,
                    principal_type,
                }
            } else {
                Command::Revoke { rights, principal }
            })
        }
        "RENAME" => {
            if !kw(toks.get(1), "MODEL") || !kw(toks.get(3), "TO") || toks.len() != 5 {
                return Err(usage());
            }
            Ok(Command::Rename {
                model: checked("el modelo", &string(toks.get(2))?, MAX_NAME, false)?,
                new_name: checked("el nombre nuevo", &string(toks.get(4))?, MAX_NAME, false)?,
            })
        }
        "ALTER" => {
            if !kw(toks.get(1), "MODEL")
                || !kw(toks.get(3), "SET")
                || !kw(toks.get(4), "DESCRIPTION")
                || toks.len() != 6
            {
                return Err(usage());
            }
            Ok(Command::SetDescription {
                model: checked("el modelo", &string(toks.get(2))?, MAX_NAME, false)?,
                description: checked(
                    "La descripción",
                    &string(toks.get(5))?,
                    MAX_DESCRIPTION,
                    true,
                )?,
            })
        }
        _ => Err(usage()),
    }
}

/// The commands of a unit (every line a command or a comment).
pub fn parse(unit: &str) -> Result<Vec<Command>> {
    let toks = lex(unit)?;
    let mut out = Vec::new();
    for line in toks.split(|t| *t == Tok::Newline) {
        if line.is_empty() {
            continue;
        }
        out.push(parse_one(line)?);
    }
    if out.is_empty() {
        return Err(usage());
    }
    Ok(out)
}

/// What the API can't do, refused before any request.
fn precheck(c: &Command) -> Result<()> {
    match c {
        Command::Grant {
            rights,
            principal_type,
            ..
        } => {
            if rights.iter().any(|r| r == "Write") {
                return Err(Error::Query(
                    "La API de Power BI no agrega ni quita el permiso Write: se da con un rol del área de trabajo (Colaborador o superior).".into(),
                ));
            }
            if principal_type == "App" {
                return Err(app_refused());
            }
        }
        // REVOKE ALL is checked against the current right (Write).
        Command::Revoke { rights, .. }
            if rights.len() < 4 && rights.iter().any(|r| r == "Write") =>
        {
            return Err(Error::Query(
                "La API de Power BI no agrega ni quita el permiso Write: se quita cambiando el rol en el área de trabajo.".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

fn app_refused() -> Error {
    Error::Query("La API de Power BI no cambia los permisos de las entidades de servicio: se dan con su rol en el área de trabajo.".into())
}

/// Runs a unit of commands on the session (refused on read-only
/// connections before any request); each one says what it did with
/// `out.info`.
pub async fn execute(cx: &mut Ctx, unit: &str, out: &mut QueryOutcome) -> Result<()> {
    execute_in(cx, unit, out).await
}

pub(crate) async fn execute_in<S: Service>(
    s: &mut S,
    unit: &str,
    out: &mut QueryOutcome,
) -> Result<()> {
    refuse_on_read_only(s, "cambiar el modelo ni sus permisos")?;
    let commands = parse(unit)?;
    for c in &commands {
        precheck(c)?;
    }
    for c in commands {
        match c {
            Command::Grant {
                rights,
                principal,
                principal_type,
            } => {
                let r: Vec<&str> = rights.iter().map(String::as_str).collect();
                change_rights(s, true, &r, &principal, &principal_type, out).await?
            }
            Command::Revoke { rights, principal } => {
                let r: Vec<&str> = rights.iter().map(String::as_str).collect();
                change_rights(s, false, &r, &principal, "User", out).await?
            }
            Command::Rename { model, new_name } => {
                let m = named_model(s, &model).await?;
                let name = display_name(&new_name, Some(&m.workspace.name), m.id.get(..8))?;
                if name == m.name {
                    out.info(format!("El modelo ya se llama «{name}»: no se envió nada."));
                    continue;
                }
                patch_model(s, &m, json!({"displayName": name})).await?;
                out.info(format!("Se renombró el modelo «{}» a «{name}».", m.name));
            }
            Command::SetDescription { model, description } => {
                let m = named_model(s, &model).await?;
                patch_model(s, &m, json!({"description": description})).await?;
                out.info(format!("Se cambió la descripción del modelo «{}».", m.name));
            }
        }
    }
    Ok(())
}

/// GRANT / REVOKE on the session's model.
async fn change_rights<S: Service>(
    s: &mut S,
    grant: bool,
    rights: &[&str],
    principal: &str,
    principal_type: &str,
    out: &mut QueryOutcome,
) -> Result<()> {
    let m = session_model(s)?;
    let users = model_users(s, &m, Priority::Editor).await.map_err(|e| {
        if crate::meta::is_fatal(&e) {
            return e;
        }
        Error::Query(format!(
            "No se pudieron leer los permisos del modelo «{}»: hace falta el permiso ReadWriteReshare en él ({}).",
            m.name,
            crate::meta::short(&e)
        ))
    })?;
    let entry = users
        .iter()
        .find(|u| u.identifier.eq_ignore_ascii_case(principal));
    if entry.is_some_and(|e| e.principal_type == "App") {
        return Err(app_refused());
    }
    let current: Vec<&str> = entry.map(|e| rights_of(&e.right)).unwrap_or_default();
    let mut new: Vec<&str> = current.clone();
    if grant {
        for r in rights.iter().chain(["Read"].iter()) {
            if !new.contains(r) {
                new.push(r);
            }
        }
    } else {
        // What a workspace role gives can't be taken here.
        if let Ok(ws) = workspace_users(s, &m).await {
            if let Some(role) = ws
                .iter()
                .find(|u| u.identifier.eq_ignore_ascii_case(principal))
                .map(|u| u.right.as_str())
            {
                // Learn (Put Dataset User In Group): admins and members
                // inherit ReadWriteReshareExplore, contributors
                // ReadWriteExplore (no Reshare), viewers Read.
                let inherited: &[&str] = match role {
                    "Admin" | "Member" => &["Read", "Build", "Reshare", "Write"],
                    "Contributor" => &["Read", "Build", "Write"],
                    "Viewer" => &["Read"],
                    _ => &[],
                };
                if rights.iter().any(|r| inherited.contains(r)) {
                    return Err(Error::Query(format!(
                        "{principal} tiene el rol {} en el área de trabajo, que le da esos permisos: se quitan cambiando su rol, no desde el modelo.",
                        crate::security::role_text(role)
                    )));
                }
            }
        }
        new.retain(|r| !rights.contains(r));
        if !new.contains(&"Read") {
            new.clear();
        }
        if current.contains(&"Write") && !new.contains(&"Write") {
            return Err(Error::Query(format!(
                "{principal} tiene el permiso Write en el modelo, que la API de Power BI no quita: se quita cambiando su rol en el área de trabajo."
            )));
        }
    }
    let order =
        |v: &mut Vec<&str>| v.sort_by_key(|r| crate::security::RIGHTS.iter().position(|x| x == r));
    let mut current_sorted = current.clone();
    order(&mut current_sorted);
    order(&mut new);
    if new == current_sorted {
        out.info(if grant {
            format!(
                "{principal} ya tenía {} en «{}»: no se envió nada.",
                rights_text(&new),
                m.name
            )
        } else {
            format!(
                "{principal} no tenía esos permisos en «{}»: no se envió nada.",
                m.name
            )
        });
        return Ok(());
    }
    let principal_type = entry
        .map(|e| e.principal_type.as_str())
        .unwrap_or(principal_type);
    let body = json!({
        "identifier": principal,
        "principalType": principal_type,
        "datasetUserAccessRight": combined(&new),
    });
    // Listed: PUT replaces the right (None removes it). New: POST adds it.
    let method = if entry.is_some() {
        Method::Put
    } else {
        Method::Post
    };
    let r = s
        .send(
            method,
            &format!("{}/users", m.path()),
            Some(&body),
            Priority::Editor,
        )
        .await?;
    check_status(r, "Power BI no aceptó el cambio de permisos")?;
    out.info(format!(
        "{principal}: permisos en «{}» → {}.",
        m.name,
        rights_text(&new)
    ));
    Ok(())
}

/// Fabric's Update Semantic Model (`displayName`, `description`).
async fn patch_model<S: Service>(s: &mut S, m: &Model, body: serde_json::Value) -> Result<()> {
    let g = fabric_workspace(s, m)?;
    let r = s
        .fabric_send(
            Method::Patch,
            &format!("/workspaces/{g}/semanticModels/{}", m.id),
            Some(&body),
            Priority::Editor,
        )
        .await?;
    check_status(r, "Fabric no aceptó el cambio del modelo")?;
    crate::meta::forget(s, Some(m));
    s.refresh_listing().await?;
    Ok(())
}

fn fabric_workspace<S: Service>(s: &S, m: &Model) -> Result<String> {
    if s.settings().cloud.fabric_api().is_none() {
        return Err(Error::Unsupported(
            "Renombrar un modelo o cambiar su descripción usa la API de Fabric, que solo está en la nube pública.".into(),
        ));
    }
    match &m.workspace.id {
        Some(g) if crate::catalog::valid_guid(g) && crate::catalog::valid_guid(&m.id) => Ok(g.clone()),
        Some(_) => Err(Error::Query("Identificador de modelo no válido.".into())),
        None => Err(Error::Unsupported(
            "Los modelos de «Mi área de trabajo» no se renombran ni se describen desde DBine: la API de Fabric pide el área de trabajo.".into(),
        )),
    }
}

/// A DAX string literal (`"…"`, a quote doubled), refusing line breaks.
fn quoted(what: &str, s: &str, max: usize, may_be_empty: bool) -> Result<String> {
    let s = checked(what, s, max, may_be_empty)?;
    Ok(format!("\"{}\"", s.replace('"', "\"\"")))
}

/// Users and permissions: `Grant` / `Revoke` of the model's rights as
/// commands (other actions: `Unsupported` with the reason).
pub fn security_script(action: &SecurityAction) -> Result<String> {
    let whole_model = |o: &Option<dbine_driver::ObjectRef>| {
        o.as_ref()
            .is_none_or(|o| o.kind.is_empty() || o.name.is_empty())
    };
    match action {
        SecurityAction::Grant {
            privileges,
            object,
            to,
            grantable,
        } => {
            if !whole_model(object) {
                return Err(Error::Unsupported(
                    "En Power BI los permisos son del modelo entero.".into(),
                ));
            }
            let mut rights: Vec<&str> = vec!["Read"];
            for p in privileges {
                let r = right(p).ok_or_else(|| {
                    Error::Query(format!("«{p}» no es un permiso de un modelo: Read, Build o Reshare."))
                })?;
                if r == "Write" {
                    return Err(Error::Unsupported(
                        "La API de Power BI no agrega ni quita el permiso Write: se da con un rol del área de trabajo.".into(),
                    ));
                }
                if !rights.contains(&r) {
                    rights.push(r);
                }
            }
            if *grantable && !rights.contains(&"Reshare") {
                rights.push("Reshare");
            }
            rights.sort_by_key(|r| crate::security::RIGHTS.iter().position(|x| x == r));
            let as_group = if crate::catalog::valid_guid(to.trim()) {
                " AS GROUP"
            } else {
                ""
            };
            Ok(format!(
                "GRANT {} TO {}{as_group}",
                rights.join(", "),
                quoted("el usuario o grupo", to, MAX_NAME, false)?
            ))
        }
        SecurityAction::Revoke {
            privileges,
            object,
            from,
        } => {
            if !whole_model(object) {
                return Err(Error::Unsupported(
                    "En Power BI los permisos son del modelo entero.".into(),
                ));
            }
            let mut rights: Vec<&str> = Vec::new();
            for p in privileges {
                if p.eq_ignore_ascii_case("ALL") {
                    rights = vec!["ALL"];
                    break;
                }
                let r = right(p).ok_or_else(|| {
                    Error::Query(format!("«{p}» no es un permiso de un modelo: Read, Build o Reshare."))
                })?;
                if r == "Write" {
                    return Err(Error::Unsupported(
                        "La API de Power BI no agrega ni quita el permiso Write: se quita cambiando el rol en el área de trabajo.".into(),
                    ));
                }
                if !rights.contains(&r) {
                    rights.push(r);
                }
            }
            // Without Read nothing is left.
            if rights.is_empty() || rights.contains(&"Read") {
                rights = vec!["ALL"];
            }
            Ok(format!(
                "REVOKE {} FROM {}",
                rights.join(", "),
                quoted("el usuario o grupo", from, MAX_NAME, false)?
            ))
        }
        SecurityAction::Drop {
            name,
            kind: dbine_driver::PrincipalKind::User,
        } => Ok(format!(
            "REVOKE ALL FROM {}",
            quoted("el usuario", name, MAX_NAME, false)?
        )),
        _ => Err(Error::Unsupported(
            "Power BI no crea usuarios, roles ni contraseñas desde DBine: las identidades son de Microsoft Entra ID y los roles de seguridad (RLS) son parte del modelo.".into(),
        )),
    }
}

/// "Renombrar…" on a model: the model itself, nothing inside it.
pub fn rename_spec() -> RenameSpec {
    RenameSpec {
        databases: true,
        database_note: Some(
            "Se renombra el modelo semántico en Power BI. Los informes conectados lo siguen por su ID, pero las conexiones que lo nombran (XMLA, Excel) dejan de encontrarlo.".into(),
        ),
        ..Default::default()
    }
}

/// The model's own name in Power BI from the new name of a rename.
///
/// Databases are listing names: `<workspace> › <model>` without a workspace
/// on the connection, and a repeated name gets ` [<first 8 hex of its id>]`
/// (`catalog::name_models`). "Renombrar…" prefills that listing name, and
/// neither part belongs to the model's name, so this drops a leading
/// `<workspace> › ` and that ` [<id>]` (ASCII case ignored). A name that
/// still has the separator is refused: a rename doesn't move the model.
pub(crate) fn display_name(
    new_name: &str,
    workspace: Option<&str>,
    id8: Option<&str>,
) -> Result<String> {
    let mut n = new_name.trim().to_string();
    if let Some(w) = workspace.filter(|w| !w.is_empty()) {
        let prefix = format!("{w}{SEPARATOR}");
        if n.get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(&prefix))
        {
            n = n[prefix.len()..].trim_start().to_string();
        }
    }
    if let Some(id8) = id8.filter(|i| !i.is_empty()) {
        let suffix = format!(" [{id8}]").to_ascii_lowercase();
        // ASCII lowercasing keeps byte offsets.
        if let Some(i) = n.to_ascii_lowercase().find(&suffix) {
            n = format!("{}{}", &n[..i], &n[i + suffix.len()..])
                .trim()
                .to_string();
        }
    }
    if n.contains(SEPARATOR) {
        return Err(Error::Query(format!(
            "«{n}» lleva «{}», el separador entre área de trabajo y modelo de la lista de DBine: escribí solo el nombre del modelo (renombrarlo no lo cambia de área de trabajo).",
            SEPARATOR.trim()
        )));
    }
    if n.is_empty() {
        return Err(Error::Query("Falta el nombre nuevo del modelo.".into()));
    }
    Ok(n)
}

/// What a listing name says without the session: the workspace before the
/// first separator and a trailing ` [<8 hex>]` suffix, if there are.
fn listing_parts(database: &str) -> (Option<&str>, Option<&str>) {
    let workspace = database.split_once(SEPARATOR).map(|(w, _)| w);
    let id8 = database
        .strip_suffix(']')
        .and_then(|d| d.rsplit_once(" ["))
        .map(|(_, id)| id)
        .filter(|id| id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    (workspace, id8)
}

/// `RENAME MODEL "<database>" TO "<new_name>"`. The command takes the
/// listing names; [`execute`] derives the model's own name from the
/// resolved model ([`display_name`]), and the warning shows it.
pub fn rename_database_script(database: &str, new_name: &str) -> Result<SyncScript> {
    let statement = format!(
        "RENAME MODEL {} TO {}",
        quoted("el modelo", database, MAX_NAME, false)?,
        quoted("el nombre nuevo", new_name, MAX_NAME, false)?
    );
    // Read from the listing name alone (the exact check runs on the resolved
    // model): a refusal here is shown, and running refuses it again.
    let (workspace, id8) = listing_parts(database);
    let mut warnings = vec![match display_name(new_name, workspace, id8) {
        Ok(name) if name == new_name.trim() => {
            format!("En Power BI el modelo se va a llamar «{name}».")
        }
        Ok(name) => format!("En Power BI el modelo se va a llamar «{name}»: el área de trabajo y el sufijo [id] que DBine agrega en la lista no son parte del nombre."),
        Err(e) => e.to_string(),
    }];
    warnings.push(
        "Los informes conectados siguen al modelo por su ID; las conexiones XMLA o de Excel que lo nombran dejan de encontrarlo.".into(),
    );
    Ok(SyncScript {
        statements: vec![statement],
        warnings,
    })
}

/// `ALTER MODEL "<database>" SET DESCRIPTION "…"` for a `description`
/// change; other keys are refused.
pub fn alter_database_script(database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    let mut out = Vec::new();
    for (k, v) in changes {
        if k != "description" {
            return Err(Error::Unsupported(format!(
                "«{k}» no se cambia desde DBine: de un modelo solo se cambia la descripción."
            )));
        }
        out.push(format!(
            "ALTER MODEL {} SET DESCRIPTION {}",
            quoted("el modelo", database, MAX_NAME, false)?,
            quoted("La descripción", v, MAX_DESCRIPTION, true)?
        ));
    }
    Ok(out.join("\n"))
}

/// Applies a properties change (the description) directly.
pub async fn alter_database(
    cx: &mut Ctx,
    database: &str,
    changes: &BTreeMap<String, String>,
) -> Result<()> {
    alter_in(cx, database, changes).await
}

pub(crate) async fn alter_in<S: Service>(
    s: &mut S,
    database: &str,
    changes: &BTreeMap<String, String>,
) -> Result<()> {
    refuse_on_read_only(s, "cambiar el modelo")?;
    // Checked before any request: only the description, valid text.
    alter_database_script(database, changes)?;
    let Some(d) = changes.get("description") else {
        return Ok(());
    };
    let m = named_model(s, database).await?;
    patch_model(s, &m, json!({"description": d})).await
}

/// Deletes the model `name` (as listed): refused on read-only connections
/// and without the right to, before any request.
pub async fn drop_database(cx: &mut Ctx, name: &str) -> Result<()> {
    drop_in(cx, name).await
}

pub(crate) async fn drop_in<S: Service>(s: &mut S, name: &str) -> Result<()> {
    refuse_on_read_only(s, "borrar el modelo")?;
    let m = named_model(s, name).await?;
    if !m.write_permission {
        return Err(Error::Query(format!(
            "Para borrar «{}» hace falta permiso de escritura en el modelo.",
            m.name
        )));
    }
    if !crate::catalog::valid_guid(&m.id) {
        return Err(Error::Query("Identificador de modelo no válido.".into()));
    }
    let r = s
        .send(Method::Delete, &m.path(), None, Priority::Editor)
        .await?;
    check_status(r, "Power BI no borró el modelo")?;
    crate::meta::forget(s, Some(&m));
    s.refresh_listing().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::fake::{Fake, GROUP};
    use serde_json::Value;

    #[test]
    fn commands_are_told_by_their_first_word() {
        for c in [
            "GRANT Read TO \"ana@contoso.com\"",
            "revoke ALL FROM \"x\"",
            "// a comment\nRENAME MODEL \"a\" TO \"b\"",
            "/* x */ ALTER MODEL \"a\" SET DESCRIPTION \"d\"",
        ] {
            assert!(is_command(c), "{c}");
        }
        for q in [
            "EVALUATE 'Grant'",
            "DEFINE VAR Grant = 1 EVALUATE {Grant}",
            "GRANTS",
            "",
            "// GRANT",
        ] {
            assert!(!is_command(q), "{q}");
        }
    }

    /// What `Session::execute` runs as a command (`is_command`) and what
    /// the app's guards let through as a query (`dax_is_query`) never
    /// overlap, whatever whitespace or comment the unit starts with.
    #[test]
    fn a_command_is_never_a_query() {
        use dbine_driver::read_only::dax_is_query;
        let heads = [
            "",
            " ",
            "\t\r\n",
            "\u{a0}",   // NBSP
            "\u{feff}", // BOM
            "\u{2028}", // line separator
            "// c\n",
            "-- c\n",
            "-- x\r", // a bare CR doesn't end a line comment
            "-- x\r\n",
            "/* c */",
            "/* c */\n// d\n",
            "/*", // unclosed: runs to the end
            "/* a /* b */",
            "//",
            "*/",
            "'",
            "\"",
            "[",
        ];
        let bodies = [
            "GRANT Read TO \"a\"",
            "grant Read TO \"a\"",
            "REVOKE ALL FROM \"a\"",
            "RENAME MODEL \"a\" TO \"b\"",
            "ALTER MODEL \"a\" SET DESCRIPTION \"d\"",
            "EVALUATE {1}",
            "evaluate 'T'",
            "DEFINE VAR x = 1 EVALUATE {x}",
            "GRANTS",
            "EVALUATEX",
            "{1}",
            "",
        ];
        for h in heads {
            for b in bodies {
                let unit = format!("{h}{b}");
                assert!(
                    !(is_command(&unit) && dax_is_query(&unit)),
                    "both a command and a query: {unit:?}"
                );
            }
        }
        // Whitespace and comments the guards skip, this skips too.
        for c in [
            "\u{a0}GRANT Read TO \"a\"",
            "-- x\r\nREVOKE ALL FROM \"a\"",
            "/* a /* b */ RENAME MODEL \"a\" TO \"b\"",
        ] {
            assert!(is_command(c), "{c:?}");
        }
        // Hidden in a comment, or behind a character that isn't one.
        for c in [
            "-- x\rGRANT Read TO \"a\"",
            "/* GRANT Read TO \"a\"",
            "\u{feff}GRANT Read TO \"a\"",
            "'GRANT'",
        ] {
            assert!(!is_command(c), "{c:?}");
            assert!(!dax_is_query(c), "{c:?}");
        }
    }

    #[test]
    fn the_command_language_parses() {
        assert_eq!(
            parse("-- da acceso\nGRANT read,EXPLORE TO \"ana@contoso.com\" AS GROUP // fin")
                .unwrap(),
            [Command::Grant {
                rights: vec!["Read".into(), "Build".into()],
                principal: "ana@contoso.com".into(),
                principal_type: "Group".into(),
            }]
        );
        assert_eq!(
            parse("REVOKE ALL FROM \"x\"").unwrap(),
            [Command::Revoke {
                rights: vec![
                    "Read".into(),
                    "Build".into(),
                    "Reshare".into(),
                    "Write".into()
                ],
                principal: "x".into(),
            }]
        );
        assert_eq!(
            parse("RENAME MODEL \"Ventas \"\"AR\"\"\" TO \"Ventas 2026\"").unwrap(),
            [Command::Rename {
                model: "Ventas \"AR\"".into(),
                new_name: "Ventas 2026".into(),
            }]
        );
        assert_eq!(
            parse("ALTER MODEL \"V\" SET DESCRIPTION \"\"").unwrap(),
            [Command::SetDescription {
                model: "V".into(),
                description: String::new(),
            }]
        );
        for bad in [
            "GRANT TO \"x\"",
            "GRANT Read \"x\"",
            "GRANT Admin TO \"x\"",
            "GRANT Read TO x",
            "GRANT Read TO \"x\" AS ROBOT",
            "GRANT Read TO \"x\" extra",
            "REVOKE Read TO \"x\"",
            "RENAME \"a\" TO \"b\"",
            "RENAME MODEL \"a\" TO \"\"",
            "ALTER MODEL \"a\" SET NAME \"b\"",
            "GRANT Read TO \"a\nb\"",
            "GRANT Read TO \"open",
            "/* open",
            "GRANT Read TO \"x\"; DROP",
            "",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let long = format!("GRANT Read TO \"{}\"", "a".repeat(300));
        assert!(parse(&long).is_err());
    }

    #[test]
    fn scripts_the_app_runs_through_execute() {
        let grant = SecurityAction::Grant {
            privileges: vec!["Build".into()],
            object: None,
            to: "ana@contoso.com".into(),
            grantable: true,
        };
        let s = security_script(&grant).unwrap();
        assert_eq!(s, "GRANT Read, Build, Reshare TO \"ana@contoso.com\"");
        assert_eq!(parse(&s).unwrap().len(), 1);
        let to_group = SecurityAction::Grant {
            privileges: vec!["Read".into()],
            object: None,
            to: "154aef10-47b8-48c4-ab97-f0bf9d5f8fcf".into(),
            grantable: false,
        };
        assert!(security_script(&to_group).unwrap().ends_with("AS GROUP"));
        let write = SecurityAction::Grant {
            privileges: vec!["Write".into()],
            object: None,
            to: "x".into(),
            grantable: false,
        };
        assert!(security_script(&write).is_err());
        let revoke = SecurityAction::Revoke {
            privileges: vec!["Reshare".into()],
            object: None,
            from: "a\"b".into(),
        };
        assert_eq!(
            security_script(&revoke).unwrap(),
            "REVOKE Reshare FROM \"a\"\"b\""
        );
        let revoke_read = SecurityAction::Revoke {
            privileges: vec!["Read".into()],
            object: None,
            from: "x".into(),
        };
        assert_eq!(
            security_script(&revoke_read).unwrap(),
            "REVOKE ALL FROM \"x\""
        );
        assert!(security_script(&SecurityAction::CreateUser {
            name: "x".into(),
            password: None
        })
        .is_err());
        let r = rename_database_script("Ventas", "Ventas \"26\"").unwrap();
        assert_eq!(
            r.statements,
            ["RENAME MODEL \"Ventas\" TO \"Ventas \"\"26\"\"\""]
        );
        assert!(parse(&r.statements[0]).is_ok());
        assert!(rename_database_script("a", "x\ny").is_err());
        let changes = BTreeMap::from([("description".to_string(), "Ventas por día".to_string())]);
        let a = alter_database_script("Ventas", &changes).unwrap();
        assert_eq!(
            a,
            "ALTER MODEL \"Ventas\" SET DESCRIPTION \"Ventas por día\""
        );
        assert!(is_command(&a) && parse(&a).is_ok());
        let other = BTreeMap::from([("culture".to_string(), "es-AR".to_string())]);
        assert!(alter_database_script("Ventas", &other).is_err());
    }

    const DATASET_USERS: &str = include_str!("../../tests/fixtures/rest_dataset_users.json");
    const GROUP_USERS: &str = include_str!("../../tests/fixtures/rest_group_users.json");

    fn users(f: &Fake, dataset_users: Value) {
        let m = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("{}/users", m.path()),
            200,
            dataset_users,
        );
        f.route(
            Method::Get,
            &format!("/groups/{GROUP}/users"),
            200,
            serde_json::from_str(GROUP_USERS).unwrap(),
        );
        f.route(
            Method::Put,
            &format!("{}/users", m.path()),
            200,
            Value::Null,
        );
        f.route(
            Method::Post,
            &format!("{}/users", m.path()),
            200,
            Value::Null,
        );
    }

    fn run(f: &mut Fake, unit: &str) -> Result<QueryOutcome> {
        let mut out = QueryOutcome::default();
        futures::executor::block_on(execute_in(f, unit, &mut out)).map(|_| out)
    }

    #[test]
    fn grants_and_revokes_compute_the_combined_right() {
        let listed = serde_json::from_str::<Value>(DATASET_USERS).unwrap();
        // [current right, command, expected method and right]
        type Case<'a> = (&'a str, &'a str, Option<(Method, &'a str)>);
        let cases: &[Case] = &[
            // john@contoso.com has Read (listed).
            (
                "john@contoso.com",
                "GRANT Build TO \"john@contoso.com\"",
                Some((Method::Put, "ReadExplore")),
            ),
            (
                "john@contoso.com",
                "GRANT Read TO \"john@contoso.com\"",
                None,
            ),
            // The group has ReadReshare.
            (
                "group",
                "REVOKE Reshare FROM \"154aef10-47b8-48c4-ab97-f0bf9d5f8fcf\"",
                Some((Method::Put, "Read")),
            ),
            (
                "group",
                "REVOKE Read FROM \"154aef10-47b8-48c4-ab97-f0bf9d5f8fcf\"",
                Some((Method::Put, "None")),
            ),
            (
                "group",
                "REVOKE Build FROM \"154aef10-47b8-48c4-ab97-f0bf9d5f8fcf\"",
                None,
            ),
            // Someone not listed: POST, Read added.
            (
                "new",
                "GRANT Reshare, Build TO \"eva@contoso.com\"",
                Some((Method::Post, "ReadReshareExplore")),
            ),
            ("new", "REVOKE ALL FROM \"eva@contoso.com\"", None),
        ];
        for (who, unit, expect) in cases {
            let mut f = Fake::writer();
            users(&f, listed.clone());
            let out = run(&mut f, unit).unwrap_or_else(|e| panic!("{who}: {unit}: {e}"));
            let w = f.writes();
            match expect {
                Some((method, right)) => {
                    assert_eq!(w.len(), 1, "{unit}");
                    assert_eq!(w[0].method, *method, "{unit}");
                    assert_eq!(
                        w[0].body.as_ref().unwrap()["datasetUserAccessRight"],
                        *right,
                        "{unit}"
                    );
                }
                None => {
                    assert!(w.is_empty(), "{unit}: {w:?}");
                    assert!(
                        out.messages.iter().any(|m| m.contains("no se envió nada")),
                        "{unit}"
                    );
                }
            }
        }
        // The group keeps its type.
        let mut f = Fake::writer();
        users(&f, listed.clone());
        run(
            &mut f,
            "GRANT Build TO \"154aef10-47b8-48c4-ab97-f0bf9d5f8fcf\"",
        )
        .unwrap();
        assert_eq!(
            f.writes()[0].body.as_ref().unwrap()["principalType"],
            "Group"
        );
    }

    #[test]
    fn what_the_api_cant_do_is_refused_before_any_request() {
        let listed = serde_json::from_str::<Value>(DATASET_USERS).unwrap();
        for unit in [
            "GRANT Write TO \"x@contoso.com\"",
            "REVOKE Write FROM \"x@contoso.com\"",
            "GRANT Read TO \"x\" AS APP",
        ] {
            let mut f = Fake::writer();
            users(&f, listed.clone());
            assert!(run(&mut f, unit).is_err(), "{unit}");
            assert!(f.requests().is_empty(), "{unit}");
        }
        // A listed app, someone with Write, a workspace role: refused
        // after reading the right, with nothing written.
        for unit in [
            "GRANT Build TO \"3d9b93c6-7b6d-4801-a491-1738910904fd\"",
            "REVOKE ALL FROM \"Adam@contoso.com\"",
        ] {
            let mut f = Fake::writer();
            users(&f, listed.clone());
            assert!(run(&mut f, unit).is_err(), "{unit}");
            assert!(f.writes().is_empty(), "{unit}");
        }
        let mut f = Fake::writer();
        users(
            &f,
            serde_json::json!({"value": [{"identifier": "w@contoso.com", "principalType": "User", "datasetUserAccessRight": "ReadWrite"}]}),
        );
        let e = run(&mut f, "REVOKE ALL FROM \"w@contoso.com\"").unwrap_err();
        assert!(e.to_string().contains("Write"), "{e}");
        assert!(f.writes().is_empty());
        // A model whose users can't be read: no blind POST.
        let mut f = Fake::writer();
        assert!(run(&mut f, "GRANT Read TO \"eva@contoso.com\"").is_err());
        assert!(f.writes().is_empty());
    }

    /// Learn: contributors inherit ReadWriteExplore, not Reshare, so a
    /// Reshare given on the model can be taken; what the role gives can't.
    #[test]
    fn a_contributor_loses_a_direct_reshare_but_not_what_the_role_gives() {
        let contributor = || {
            let f = Fake::writer();
            let m = f.model.clone().unwrap();
            f.route(
                Method::Get,
                &format!("{}/users", m.path()),
                200,
                serde_json::json!({"value": [{"identifier": "c@contoso.com", "principalType": "User", "datasetUserAccessRight": "ReadReshareExplore"}]}),
            );
            f.route(
                Method::Get,
                &format!("/groups/{GROUP}/users"),
                200,
                serde_json::json!({"value": [{"identifier": "c@contoso.com", "principalType": "User", "groupUserAccessRight": "Contributor"}]}),
            );
            f.route(
                Method::Put,
                &format!("{}/users", m.path()),
                200,
                Value::Null,
            );
            f
        };
        let mut f = contributor();
        run(&mut f, "REVOKE Reshare FROM \"c@contoso.com\"").unwrap();
        let w = f.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(
            w[0].body.as_ref().unwrap()["datasetUserAccessRight"],
            "ReadExplore"
        );
        let mut f = contributor();
        let e = run(&mut f, "REVOKE Build FROM \"c@contoso.com\"").unwrap_err();
        assert!(e.to_string().contains("rol"), "{e}");
        assert!(f.writes().is_empty());
    }

    /// S1 for the administration: a read-only connection whose token has
    /// the write scopes sends nothing.
    #[test]
    fn read_only_connections_send_nothing() {
        let mut f = Fake::writer();
        f.set_read_only(true);
        users(&f, serde_json::from_str(DATASET_USERS).unwrap());
        for unit in [
            "GRANT Read TO \"x@contoso.com\"",
            "REVOKE ALL FROM \"john@contoso.com\"",
            "RENAME MODEL \"Ventas\" TO \"V2\"",
            "ALTER MODEL \"Ventas\" SET DESCRIPTION \"d\"",
        ] {
            assert!(run(&mut f, unit).is_err(), "{unit}");
        }
        let changes = BTreeMap::from([("description".to_string(), "d".to_string())]);
        assert!(futures::executor::block_on(alter_in(&mut f, "Ventas", &changes)).is_err());
        assert!(futures::executor::block_on(drop_in(&mut f, "Ventas")).is_err());
        assert!(f.requests().is_empty(), "{:?}", f.requests());
    }

    #[test]
    fn rename_describe_and_delete() {
        let mut f = Fake::writer();
        let m = f.model.clone().unwrap();
        let fabric = format!("/workspaces/{GROUP}/semanticModels/{}", m.id);
        f.push(crate::meta::fake::Route {
            method: Method::Patch,
            path: fabric.clone(),
            fabric: true,
            status: 200,
            body: serde_json::from_str(include_str!(
                "../../tests/fixtures/rest_fabric_semantic_model.json"
            ))
            .unwrap(),
            location: None,
            retry_after: None,
            times: None,
        });
        f.route(Method::Delete, &m.path(), 200, Value::Null);
        let out = run(&mut f, "RENAME MODEL \"Ventas\" TO \"Ventas 2026\"").unwrap();
        assert!(out.messages[0].contains("«Ventas 2026»"));
        let changes = BTreeMap::from([("description".to_string(), "Ventas por día".to_string())]);
        futures::executor::block_on(alter_in(&mut f, "Ventas", &changes)).unwrap();
        futures::executor::block_on(drop_in(&mut f, "Ventas")).unwrap();
        let w = f.writes();
        assert_eq!(w.len(), 3);
        assert!(
            w[0].fabric && w[0].body == Some(serde_json::json!({"displayName": "Ventas 2026"}))
        );
        assert_eq!(
            w[1].body,
            Some(serde_json::json!({"description": "Ventas por día"}))
        );
        assert_eq!(
            (w[2].method, w[2].path.as_str()),
            (Method::Delete, m.path().as_str())
        );
        assert_eq!(f.listings, 3);
        // Without write permission: no DELETE.
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().write_permission = false;
        assert!(futures::executor::block_on(drop_in(&mut f, "Ventas")).is_err());
        assert!(f.requests().is_empty());
        // A model of My workspace: Fabric can't name it.
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().workspace.id = None;
        assert!(run(&mut f, "RENAME MODEL \"Ventas\" TO \"x\"").is_err());
        assert!(f.requests().is_empty());
    }

    /// A Fake whose model is listed as `listed` (every workspace, maybe a
    /// repeated name), with Fabric's PATCH answered.
    fn listed_as(listed: impl Fn(&str) -> String) -> Fake {
        let mut f = Fake::writer();
        for m in f.models.iter_mut().chain(f.model.as_mut()) {
            m.listed_as = listed(&m.id);
        }
        let m = f.model.clone().unwrap();
        f.push(crate::meta::fake::Route {
            method: Method::Patch,
            path: format!("/workspaces/{GROUP}/semanticModels/{}", m.id),
            fabric: true,
            status: 200,
            body: Value::Null,
            location: None,
            retry_after: None,
            times: None,
        });
        f
    }

    #[test]
    fn a_rename_sends_the_models_own_name_not_the_listing_name() {
        // No workspace on the connection: "<workspace> › <model>".
        let mut f = listed_as(|_| "Finanzas › Ventas".into());
        let out = run(
            &mut f,
            "RENAME MODEL \"Finanzas › Ventas\" TO \"Finanzas › Ventas 2026\"",
        )
        .unwrap();
        assert_eq!(
            f.writes()[0].body,
            Some(json!({"displayName": "Ventas 2026"}))
        );
        assert!(
            out.messages[0].contains("«Ventas 2026»"),
            "{:?}",
            out.messages
        );
        // A repeated name: the id suffix goes too, wherever it was left.
        for new in [
            "Finanzas › Ventas [{id}] 2026",
            "Finanzas › Ventas 2026 [{id}]",
            "Ventas 2026",
        ] {
            let mut f = listed_as(|id| format!("Finanzas › Ventas [{}]", &id[..8]));
            let m = f.model.clone().unwrap();
            let unit = format!(
                "RENAME MODEL \"{}\" TO \"{}\"",
                m.listed_as,
                new.replace("{id}", &m.id[..8])
            );
            run(&mut f, &unit).unwrap_or_else(|e| panic!("{unit}: {e}"));
            assert_eq!(
                f.writes()[0].body,
                Some(json!({"displayName": "Ventas 2026"})),
                "{unit}"
            );
        }
        // Still a separator: refused before the PATCH, with the reason.
        let mut f = listed_as(|_| "Finanzas › Ventas".into());
        let e = run(
            &mut f,
            "RENAME MODEL \"Finanzas › Ventas\" TO \"Comercial › Ventas\"",
        )
        .unwrap_err();
        assert!(e.to_string().contains("área de trabajo"), "{e}");
        assert!(f.writes().is_empty());
        // The same name: nothing sent.
        let mut f = listed_as(|_| "Finanzas › Ventas".into());
        let out = run(
            &mut f,
            "RENAME MODEL \"Finanzas › Ventas\" TO \"Finanzas › Ventas\"",
        )
        .unwrap();
        assert!(f.writes().is_empty());
        assert!(out.messages[0].contains("no se envió nada"));
    }

    #[test]
    fn the_rename_script_shows_the_models_own_name() {
        let r = rename_database_script("Finanzas › Ventas", "Finanzas › Ventas 2026").unwrap();
        assert_eq!(
            r.statements,
            ["RENAME MODEL \"Finanzas › Ventas\" TO \"Finanzas › Ventas 2026\""]
        );
        assert!(r.warnings[0].contains("«Ventas 2026»"), "{:?}", r.warnings);
        let r = rename_database_script(
            "Finanzas › Ventas [1a2b3c4d]",
            "Finanzas › Ventas 2026 [1a2b3c4d]",
        )
        .unwrap();
        assert!(r.warnings[0].contains("«Ventas 2026»"), "{:?}", r.warnings);
        let r = rename_database_script("Ventas", "Ventas 2026").unwrap();
        assert_eq!(
            r.warnings[0],
            "En Power BI el modelo se va a llamar «Ventas 2026»."
        );
        let r = rename_database_script("Finanzas › Ventas", "Comercial › Ventas").unwrap();
        assert!(r.warnings[0].contains("separador"), "{:?}", r.warnings);
        assert_eq!(listing_parts("Ventas [zz]"), (None, None));
        assert_eq!(
            listing_parts("A › B [0f0f0f0f]"),
            (Some("A"), Some("0f0f0f0f"))
        );
    }
}
