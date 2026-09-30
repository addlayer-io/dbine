//! Identifier names across engines: case folding and length limits.
//!
//! Drivers quote every identifier in their DDL, so a name keeps its exact
//! spelling in the target. That is right for mixed-case names (they were
//! quoted on purpose) but wrong for names that are merely the source's
//! folded form: Oracle's `CLIENTES` copied to PostgreSQL as `"CLIENTES"`
//! would need quotes in every query forever. So a *regular* name spelled
//! entirely in the source's folding case is refolded to the target's.

use crate::dialect::IdentCase;
use std::collections::HashSet;

/// Letters, digits and `_`, starting with a letter or `_`.
pub fn is_regular(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The name as the target would store it unquoted, when the source did
/// the same for it; otherwise unchanged.
pub fn refold(name: &str, from: IdentCase, to: IdentCase) -> String {
    if !is_regular(name) || from == to {
        return name.to_string();
    }
    let folded_in_source = match from {
        IdentCase::Upper => !name.chars().any(|c| c.is_ascii_lowercase()),
        IdentCase::Lower => !name.chars().any(|c| c.is_ascii_uppercase()),
        // Engines that keep case: a regular all-lower or all-upper name is
        // the common convention, fold it too.
        IdentCase::Preserve => {
            !name.chars().any(|c| c.is_ascii_uppercase()) || !name.chars().any(|c| c.is_ascii_lowercase())
        }
    };
    if !folded_in_source {
        return name.to_string();
    }
    match to {
        IdentCase::Upper => name.to_ascii_uppercase(),
        IdentCase::Lower => name.to_ascii_lowercase(),
        IdentCase::Preserve => name.to_string(),
    }
}

/// Shorten `name` to `max` bytes, keeping it unique among `taken` with a
/// short hash suffix (`clientes_direccion_de_entrega_principal` →
/// `clientes_direccion_de_ent_3f2a`).
pub fn fit(name: &str, max: usize, taken: &mut HashSet<String>) -> String {
    let key = |s: &str| s.to_ascii_lowercase();
    if name.len() <= max && !taken.contains(&key(name)) {
        taken.insert(key(name));
        return name.to_string();
    }
    let hash = fnv(name);
    for salt in 0u32.. {
        let suffix = format!("_{:04x}", (hash.wrapping_add(salt)) & 0xffff);
        let mut cut = max.saturating_sub(suffix.len()).min(name.len());
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        let candidate = format!("{}{}", &name[..cut], suffix);
        if !taken.contains(&key(&candidate)) {
            taken.insert(key(&candidate));
            return candidate;
        }
    }
    unreachable!()
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refolds_only_folded_names() {
        assert_eq!(refold("CLIENTES", IdentCase::Upper, IdentCase::Lower), "clientes");
        assert_eq!(refold("MiTabla", IdentCase::Upper, IdentCase::Lower), "MiTabla");
        assert_eq!(refold("pedidos", IdentCase::Lower, IdentCase::Upper), "PEDIDOS");
        assert_eq!(refold("my table", IdentCase::Lower, IdentCase::Upper), "my table");
        assert_eq!(refold("Orders", IdentCase::Preserve, IdentCase::Lower), "Orders");
        assert_eq!(refold("orders", IdentCase::Preserve, IdentCase::Upper), "ORDERS");
    }

    #[test]
    fn fits_and_stays_unique() {
        let mut taken = HashSet::new();
        let a = fit("clientes_direccion_de_entrega_principal", 30, &mut taken);
        let b = fit("clientes_direccion_de_entrega_principal", 30, &mut taken);
        assert!(a.len() <= 30 && b.len() <= 30);
        assert_ne!(a, b);
        assert_eq!(fit("id", 30, &mut taken), "id");
    }
}
