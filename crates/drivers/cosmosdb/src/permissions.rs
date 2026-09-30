//! What the login may do (`Session::permissions`). DBine signs in with an
//! account key: a read-write key may do everything, a read-only key no
//! change at all, and nothing the key may read tells one from the other
//! for certain (nor Entra ID roles, which live in the control plane). So
//! creating and dropping databases and managing the database's users stay
//! unknown. DBine's read-only mode isn't looked at: this reports what the
//! server grants the user, and the read-only mode blocks the writes on its
//! own. Cosmos DB has no backups DBine runs, no profiler and no sessions to
//! end.

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
