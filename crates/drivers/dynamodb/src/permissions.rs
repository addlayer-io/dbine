//! What the login may do (`Session::permissions`). DynamoDB's access is
//! IAM policies on the credentials, with conditions and resource patterns
//! the API can't evaluate for the caller (there's no dry run of
//! `CreateBackup` / `RestoreTableFromBackup`, and simulating a policy needs
//! IAM permissions of its own): backups and restores stay unknown. DBine's
//! read-only mode isn't looked at: this reports what the server grants the
//! user, and the read-only mode blocks the writes on its own. DynamoDB has
//! no profiler, sessions to end, databases or users.

use dbine_driver::Permissions;

pub(crate) fn decide() -> Permissions {
    Permissions::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_known() {
        assert_eq!(decide(), Permissions::default());
    }
}
