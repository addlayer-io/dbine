//! What the user may do (`Session::permissions`), from `ACL DRYRUN <me>
//! <command>` (Redis 7+, Valkey, Dragonfly): the server's own answer for
//! the user's ACL rules, without running the command.
//!
//! - backup: BGSAVE or SAVE (the two commands the Backups tab offers).
//! - profiler: MONITOR.
//! - security: ACL SETUSER (creating users, passwords and rules).
//!
//! The user comes from `ACL WHOAMI` (Dragonfly answers "User is <name>").
//! Both commands are `@admin` / `@dangerous`: a user without them can't ask
//! and everything stays unknown, as it does on servers without ACL (Redis
//! before 6, where a command may still be renamed or disabled) or without
//! DRYRUN (Redis 6.x, KeyDB). Restoring, ending sessions and creating or
//! dropping databases aren't offered for Redis. Only a broken connection
//! is an error.

use crate::shape::text_of;
use crate::RedisSession;
use dbine_driver::{Access, Error, Permissions, Result};
use redis::Value;

/// `ACL WHOAMI`'s reply as the user name.
pub(crate) fn user_name(reply: &str) -> Option<String> {
    let name = reply.trim();
    let name = name.strip_prefix("User is ").unwrap_or(name).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// `ACL DRYRUN`'s reply: "OK" when allowed, a sentence ("User ana has no
/// permissions to run the 'monitor' command", Dragonfly: "This user has
/// no permissions…") when not; anything else isn't an answer.
pub(crate) fn dryrun_allows(reply: &Value) -> Option<bool> {
    if matches!(reply, Value::Okay) {
        return Some(true);
    }
    let text = text_of(reply);
    if text.trim().eq_ignore_ascii_case("OK") {
        Some(true)
    } else if text.contains("no permissions") {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn decide(backup: &[Option<bool>], monitor: Option<bool>, setuser: Option<bool>) -> Permissions {
    let access = |v: Option<bool>, missing: &str| v.map_or(Access::Unknown, |ok| Access::check(ok, missing));
    // Either command makes a backup.
    let backup = if backup.contains(&Some(true)) {
        Some(true)
    } else if !backup.is_empty() && backup.iter().all(Option::is_some) {
        Some(false)
    } else {
        None
    };
    Permissions {
        backup: access(backup, "BGSAVE (o SAVE)"),
        profiler: access(monitor, "MONITOR"),
        manage_security: access(setuser, "ACL SETUSER"),
        ..Default::default()
    }
}

impl RedisSession {
    /// `None` when the server refused the command; a broken connection is
    /// the error.
    async fn permission_reply(&mut self, args: &[&[u8]]) -> Result<Option<Value>> {
        match self.run(args).await {
            Ok(v) => Ok(Some(v)),
            Err(e @ Error::Connect(_)) => Err(e),
            Err(e) => {
                tracing::debug!("redis: permissions check refused: {e}");
                Ok(None)
            }
        }
    }

    async fn dryrun(&mut self, user: &str, command: &[&[u8]]) -> Result<Option<bool>> {
        let mut args: Vec<&[u8]> = vec![b"ACL", b"DRYRUN", user.as_bytes()];
        args.extend_from_slice(command);
        Ok(self.permission_reply(&args).await?.as_ref().and_then(dryrun_allows))
    }
}

pub(crate) async fn check(s: &mut RedisSession) -> Result<Permissions> {
    let Some(user) = s.permission_reply(&[b"ACL", b"WHOAMI"]).await?.map(|v| text_of(&v)).and_then(|t| user_name(&t)) else {
        return Ok(Permissions::default());
    };
    let bgsave = s.dryrun(&user, &[b"BGSAVE"]).await?;
    // Without DRYRUN (or the right to use it) the first answer says so.
    if bgsave.is_none() {
        return Ok(Permissions::default());
    }
    let save = s.dryrun(&user, &[b"SAVE"]).await?;
    let monitor = s.dryrun(&user, &[b"MONITOR"]).await?;
    let setuser = s.dryrun(&user, &[b"ACL", b"SETUSER", b"dbine_check"]).await?;
    Ok(decide(&[bgsave, save], monitor, setuser))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    #[test]
    fn whoami_as_each_server_answers() {
        assert_eq!(user_name("ana").as_deref(), Some("ana"));
        assert_eq!(user_name("User is default").as_deref(), Some("default"));
        assert_eq!(user_name("  "), None);
    }

    #[test]
    fn dryrun_replies() {
        assert_eq!(dryrun_allows(&Value::Okay), Some(true));
        assert_eq!(dryrun_allows(&Value::SimpleString("OK".into())), Some(true));
        let no = |t: &str| Value::BulkString(t.as_bytes().to_vec());
        assert_eq!(dryrun_allows(&no("User ana has no permissions to run the 'monitor' command")), Some(false));
        assert_eq!(dryrun_allows(&no("This user has no permissions to run the 'MONITOR' command")), Some(false));
        assert_eq!(dryrun_allows(&Value::Nil), None);
    }

    #[test]
    fn decides_from_the_answers() {
        let p = decide(&[Some(true), Some(true)], Some(true), Some(true));
        assert_eq!((&p.backup, &p.profiler, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.restore, &p.kill_session, &p.create_database, &p.drop_database), (&Access::Unknown, &Access::Unknown, &Access::Unknown, &Access::Unknown));

        let p = decide(&[Some(false), Some(false)], Some(false), Some(false));
        assert!(denied(&p.backup, "BGSAVE"));
        assert!(denied(&p.profiler, "MONITOR"));
        assert!(denied(&p.manage_security, "ACL SETUSER"));

        // SAVE alone still makes a backup; an unanswered one decides nothing.
        assert_eq!(decide(&[Some(false), Some(true)], None, None).backup, Access::Allowed);
        assert_eq!(decide(&[Some(false), None], None, None).backup, Access::Unknown);
        assert_eq!(decide(&[Some(false), None], None, None).profiler, Access::Unknown);
    }
}
