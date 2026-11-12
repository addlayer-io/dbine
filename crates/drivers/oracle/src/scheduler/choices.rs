//! The server's objects the forms' `Ref` fields pick from, by source.
//! Values are object ids ([`super::obj_id`]); what the login can't read
//! comes back empty (and, for the resource manager's lists and services,
//! the field becomes plain text).

use super::{obj_id, Ctx, Kind};
use crate::jobs::{self, Cells};
use oracledb::Connection;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default, Clone)]
pub(crate) struct Catalog {
    pub(crate) lists: BTreeMap<String, Vec<(String, String)>>,
    /// Sources the login couldn't read.
    pub(crate) missing: HashSet<String>,
}

impl Catalog {
    pub(crate) fn get(&self, source: &str) -> &[(String, String)] {
        self.lists.get(source).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The catalog's spelling of `value` (exact, else any case).
    pub(crate) fn pick(&self, source: &str, value: &str) -> Option<&str> {
        let list = self.get(source);
        list.iter()
            .find(|(v, _)| v == value)
            .or_else(|| list.iter().find(|(v, _)| v.eq_ignore_ascii_case(value)))
            .map(|(v, _)| v.as_str())
    }

    /// The source could be read.
    pub(crate) fn readable(&self, source: &str) -> bool {
        !self.missing.contains(source)
    }

    fn put(&mut self, source: &str, list: Vec<(String, String)>) {
        self.lists.insert(source.to_string(), list);
    }
}

fn read(c: &Connection, sql: &str) -> Result<Vec<Cells>, oracledb::Error> {
    jobs::texts(c, sql, &[], 10_000)
}

/// (owner, name) rows as ids, labelled `OWNER.NAME` (just `NAME` when
/// it's the login's own).
fn ids(rows: &[Cells], user: &str, prefix: &str) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|r| {
            let owner = jobs::col(r, 0)?;
            let name = jobs::col(r, 1)?;
            let label = if owner == user {
                format!("{prefix}{name}")
            } else {
                format!("{prefix}{owner}.{name}")
            };
            Some((obj_id(Some(&owner), &name), label))
        })
        .collect()
}

/// SYS's objects by name.
fn sys_ids(rows: &[Cells], prefix: &str) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|r| {
            let name = jobs::col(r, 0)?;
            Some((obj_id(Some("SYS"), &name), format!("{prefix}{name}")))
        })
        .collect()
}

fn plain(rows: &[Cells]) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|r| jobs::col(r, 0))
        .map(|n| (n.clone(), n))
        .collect()
}

pub(crate) fn load(c: &Connection, ctx: &Ctx, sources: &[&str]) -> Catalog {
    let mut cat = Catalog::default();
    let user = ctx.user().to_string();
    for &source in sources {
        let got: Result<Vec<(String, String)>, oracledb::Error> = (|| {
            Ok(match source {
                "schema" => {
                    let mut list = vec![(user.clone(), user.clone())];
                    if ctx.create_any(Kind::Job) || ctx.create_any(Kind::Credential) {
                        let sql = if ctx.major >= 12 {
                            "SELECT username FROM all_users WHERE oracle_maintained = 'N' OR username = 'SYSTEM' ORDER BY 1"
                        } else {
                            "SELECT username FROM all_users ORDER BY 1"
                        };
                        for (u, _) in plain(&read(c, sql)?) {
                            if u != user {
                                list.push((u.clone(), u));
                            }
                        }
                    }
                    list
                }
                "program" => ids(
                    &read(c, &ctx.sql("SELECT owner, program_name FROM {v}_scheduler_programs ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "schedule" => ids(
                    &read(c, &ctx.sql("SELECT owner, schedule_name FROM {v}_scheduler_schedules ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "event_schedule" => ids(
                    &read(c, &ctx.sql("SELECT owner, schedule_name FROM {v}_scheduler_schedules WHERE schedule_type = 'EVENT' ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "chain" => ids(
                    &read(c, &ctx.sql("SELECT owner, chain_name FROM {v}_scheduler_chains ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "job" => ids(
                    &read(c, &ctx.sql("SELECT owner, job_name FROM {v}_scheduler_jobs WHERE job_subname IS NULL ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "job_or_program" => {
                    let mut list = ids(
                        &read(c, &ctx.sql("SELECT owner, job_name FROM {v}_scheduler_jobs WHERE job_subname IS NULL ORDER BY 1, 2"))?,
                        &user,
                        "Job ",
                    );
                    list.extend(ids(
                        &read(c, &ctx.sql("SELECT owner, program_name FROM {v}_scheduler_programs ORDER BY 1, 2"))?,
                        &user,
                        "Programa ",
                    ));
                    list
                }
                "window" => sys_ids(
                    &read(c, &ctx.sql("SELECT window_name FROM {v}_scheduler_windows ORDER BY 1"))?,
                    "",
                ),
                "window_group" => sys_ids(
                    &read(c, &ctx.sql("SELECT window_group_name FROM {v}_scheduler_window_groups ORDER BY 1"))?,
                    "",
                ),
                "job_class" => sys_ids(
                    &read(c, &ctx.sql("SELECT job_class_name FROM {v}_scheduler_job_classes ORDER BY 1"))?,
                    "",
                ),
                "file_watcher" => ids(
                    &read(c, &ctx.sql("SELECT owner, file_watcher_name FROM {v}_scheduler_file_watchers ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "credential" => {
                    let sql = if ctx.credential_api() {
                        "SELECT owner, credential_name FROM {v}_credentials ORDER BY 1, 2"
                    } else {
                        "SELECT owner, credential_name FROM {v}_scheduler_credentials ORDER BY 1, 2"
                    };
                    ids(&read(c, &ctx.sql(sql))?, &user, "")
                }
                "external_destination" => sys_ids(
                    &read(c, &ctx.sql("SELECT destination_name FROM {v}_scheduler_external_dests ORDER BY 1"))?,
                    "",
                ),
                "db_destination" => ids(
                    &read(c, &ctx.sql("SELECT owner, destination_name FROM {v}_scheduler_db_dests ORDER BY 1, 2"))?,
                    &user,
                    "",
                ),
                "destination" => {
                    let mut list = Vec::new();
                    if ctx.has(Kind::DbDestination) {
                        list.extend(ids(
                            &read(c, &ctx.sql("SELECT owner, destination_name FROM {v}_scheduler_db_dests ORDER BY 1, 2"))?,
                            &user,
                            "",
                        ));
                    }
                    if ctx.has(Kind::ExternalDestination) {
                        list.extend(sys_ids(
                            &read(c, &ctx.sql("SELECT destination_name FROM {v}_scheduler_external_dests ORDER BY 1"))?,
                            "",
                        ));
                    }
                    if ctx.has(Kind::DestinationGroup) {
                        list.extend(ids(
                            &read(c, &ctx.sql("SELECT owner, group_name FROM {v}_scheduler_groups WHERE group_type IN ('DB_DEST', 'EXTERNAL_DEST') ORDER BY 1, 2"))?,
                            &user,
                            "",
                        ));
                    }
                    list
                }
                "member:db" | "member:external" | "member:any" => {
                    let mut list = vec![("LOCAL".to_string(), "LOCAL".to_string())];
                    if source != "member:external" && ctx.has(Kind::DbDestination) {
                        list.extend(ids(
                            &read(c, &ctx.sql("SELECT owner, destination_name FROM {v}_scheduler_db_dests ORDER BY 1, 2"))?,
                            &user,
                            "",
                        ));
                    }
                    if source != "member:db" && ctx.has(Kind::ExternalDestination) {
                        list.extend(sys_ids(
                            &read(c, &ctx.sql("SELECT destination_name FROM {v}_scheduler_external_dests ORDER BY 1"))?,
                            "",
                        ));
                    }
                    list
                }
                "resource_plan" => plain(&read(c, "SELECT plan FROM dba_rsrc_plans ORDER BY 1")?),
                "consumer_group" => plain(&read(
                    c,
                    "SELECT consumer_group FROM dba_rsrc_consumer_groups ORDER BY 1",
                )?),
                "service" => plain(&read(c, "SELECT name FROM all_services ORDER BY 1")?),
                _ => Vec::new(),
            })
        })();
        match got {
            Ok(list) => cat.put(source, list),
            Err(e) => {
                tracing::debug!("oracle scheduler: choices {source}: {e}");
                cat.missing.insert(source.to_string());
                cat.put(source, Vec::new());
            }
        }
    }
    cat
}
