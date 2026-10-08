//! The tables a subset touches and the order they're written in: the
//! starting table, every parent its rows need (recursively), optionally its
//! children down to a depth, and an order with parents first. A cycle of
//! foreign keys is broken at one of its edges (a nullable one when there is
//! one): those columns are written NULL and set afterwards.

use dbine_driver::TableSchema;
use std::collections::{BTreeMap, BTreeSet};

/// A foreign key between two tables of the database (indexes into the
/// schema list).
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub child: usize,
    pub parent: usize,
    pub child_cols: Vec<String>,
    pub parent_cols: Vec<String>,
    /// Every child column takes NULL: the edge can be cut.
    pub nullable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Start,
    Child,
    Parent,
}

impl Role {
    pub fn id(self) -> &'static str {
        match self {
            Role::Start => "start",
            Role::Child => "child",
            Role::Parent => "parent",
        }
    }
}

/// The tables in the subset and how they were reached.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Table → (role, depth): children count their depth from the start,
    /// parents their distance up from the table that needs them.
    pub members: BTreeMap<usize, (Role, u32)>,
}

pub fn same(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}

/// The table `schema.name` in the list (names compared ignoring case; a
/// missing schema matches any).
pub fn find(tables: &[TableSchema], schema: &Option<String>, name: &str) -> Option<usize> {
    tables
        .iter()
        .position(|t| t.name == name && same(&t.schema, schema))
        .or_else(|| tables.iter().position(|t| t.name.eq_ignore_ascii_case(name) && same(&t.schema, schema)))
}

/// Every foreign key whose both ends are in `tables`.
pub fn edges(tables: &[TableSchema]) -> Vec<Edge> {
    let mut out = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        for fk in &t.foreign_keys {
            if fk.columns.is_empty() || fk.columns.len() != fk.ref_columns.len() {
                continue;
            }
            let schema = fk.ref_schema.clone().or_else(|| t.schema.clone());
            let Some(p) = find(tables, &schema, &fk.ref_table) else { continue };
            let nullable = fk.columns.iter().all(|c| t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)).is_none_or(|x| x.nullable));
            out.push(Edge { child: i, parent: p, child_cols: fk.columns.clone(), parent_cols: fk.ref_columns.clone(), nullable });
        }
    }
    out
}

/// The tables reached from `start`: children down to `children_depth`
/// levels (none when `None`), then the parents of everything, recursively.
pub fn scope(edges: &[Edge], start: usize, children_depth: Option<u32>) -> Scope {
    let mut members: BTreeMap<usize, (Role, u32)> = BTreeMap::new();
    members.insert(start, (Role::Start, 0));
    if let Some(max) = children_depth {
        let mut frontier = vec![start];
        for depth in 1..=max {
            let mut next = Vec::new();
            for &t in &frontier {
                for e in edges.iter().filter(|e| e.parent == t && e.child != t) {
                    if !members.contains_key(&e.child) {
                        members.insert(e.child, (Role::Child, depth));
                        next.push(e.child);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
    }
    let mut work: Vec<(usize, u32)> = members.keys().map(|&t| (t, 0)).collect();
    while let Some((t, up)) = work.pop() {
        for e in edges.iter().filter(|e| e.child == t && e.parent != t) {
            if !members.contains_key(&e.parent) {
                members.insert(e.parent, (Role::Parent, up + 1));
                work.push((e.parent, up + 1));
            }
        }
    }
    Scope { members }
}

/// The scope's tables in write order (parents first) and the edges cut to
/// get it (indexes into `edges`). A table's reference to itself is not
/// cut: its rows are ordered instead ([`rows_parents_first`]).
pub fn order(scope: &Scope, edges: &[Edge]) -> (Vec<usize>, Vec<usize>) {
    let inside: Vec<usize> = (0..edges.len())
        .filter(|&i| {
            let e = &edges[i];
            e.child != e.parent && scope.members.contains_key(&e.child) && scope.members.contains_key(&e.parent)
        })
        .collect();
    let mut left: BTreeSet<usize> = scope.members.keys().copied().collect();
    let mut broken: Vec<usize> = Vec::new();
    let mut out = Vec::new();
    while !left.is_empty() {
        let blocked = |t: usize, broken: &[usize]| inside.iter().any(|&i| edges[i].child == t && left.contains(&edges[i].parent) && !broken.contains(&i));
        if let Some(&t) = left.iter().find(|&&t| !blocked(t, &broken)) {
            left.remove(&t);
            out.push(t);
            continue;
        }
        // A cycle: cut one of the edges among what's left, a nullable one
        // if possible.
        let candidates: Vec<usize> = inside.iter().copied().filter(|&i| !broken.contains(&i) && left.contains(&edges[i].child) && left.contains(&edges[i].parent)).collect();
        let Some(cut) = candidates.iter().copied().find(|&i| edges[i].nullable).or_else(|| candidates.first().copied()) else { break };
        broken.push(cut);
    }
    (out, broken)
}

/// Row order for a table that references itself: each row after the row
/// it points to, when that row is in the set. `parent_of[i]` is the index
/// of row `i`'s parent row. Rows in a cycle keep their order at the end.
pub fn rows_parents_first(parent_of: &[Option<usize>]) -> Vec<usize> {
    let n = parent_of.len();
    let mut placed = vec![false; n];
    let mut out = Vec::with_capacity(n);
    for start in 0..n {
        // Walk up to the first ancestor not placed yet, then place the chain
        // top-down.
        let mut chain = Vec::new();
        let mut cur = Some(start);
        while let Some(i) = cur {
            if placed[i] || chain.contains(&i) {
                break;
            }
            chain.push(i);
            cur = parent_of[i];
        }
        for &i in chain.iter().rev() {
            if !placed[i] {
                placed[i] = true;
                out.push(i);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ForeignKeyDef};

    fn table(name: &str, fks: &[(&str, &str, bool)]) -> TableSchema {
        let mut t = TableSchema { name: name.into(), ..Default::default() };
        t.columns.push(ColumnDef { name: "id".into(), data_type: "int".into(), nullable: false, ..Default::default() });
        for (col, parent, nullable) in fks {
            t.columns.push(ColumnDef { name: (*col).into(), data_type: "int".into(), nullable: *nullable, ..Default::default() });
            t.foreign_keys.push(ForeignKeyDef { name: None, columns: vec![(*col).into()], ref_schema: None, ref_table: (*parent).into(), ref_columns: vec!["id".into()], on_delete: None, on_update: None });
        }
        t
    }

    fn names(tables: &[TableSchema], ids: &[usize]) -> Vec<String> {
        ids.iter().map(|&i| tables[i].name.clone()).collect()
    }

    #[test]
    fn parents_first_and_children_by_depth() {
        let tables = vec![
            table("order_items", &[("order_id", "orders", false), ("product_id", "products", false)]),
            table("orders", &[("customer_id", "customers", false)]),
            table("customers", &[("country_id", "countries", true)]),
            table("products", &[("category_id", "categories", false)]),
            table("categories", &[]),
            table("countries", &[]),
            table("shipments", &[("item_id", "order_items", false)]),
            table("unrelated", &[]),
        ];
        let e = edges(&tables);
        let start = find(&tables, &None, "ORDERS").unwrap();

        // Only parents.
        let s = scope(&e, start, None);
        let (o, broken) = order(&s, &e);
        assert!(broken.is_empty());
        assert_eq!(names(&tables, &o), ["countries", "customers", "orders"]);

        // Children one level down bring their own parents.
        let s = scope(&e, start, Some(1));
        assert_eq!(s.members[&0], (Role::Child, 1));
        assert!(!s.members.contains_key(&6), "shipments are two levels down");
        let (o, _) = order(&s, &e);
        let pos = |n: &str| o.iter().position(|&i| tables[i].name == n).unwrap();
        for (p, c) in [("customers", "orders"), ("orders", "order_items"), ("products", "order_items"), ("categories", "products"), ("countries", "customers")] {
            assert!(pos(p) < pos(c), "{p} before {c}");
        }
        assert_eq!(s.members.get(&3).map(|m| m.0), Some(Role::Parent));
        assert!(!s.members.contains_key(&7));

        let s = scope(&e, start, Some(5));
        assert!(s.members.contains_key(&6));
    }

    #[test]
    fn cycles_are_cut_at_a_nullable_edge() {
        // departments.manager_id → employees (nullable), employees.dept_id → departments (required),
        // employees.boss_id → employees (self).
        let tables = vec![
            table("employees", &[("dept_id", "departments", false), ("boss_id", "employees", true)]),
            table("departments", &[("manager_id", "employees", true)]),
        ];
        let e = edges(&tables);
        let s = scope(&e, 0, None);
        let (o, broken) = order(&s, &e);
        assert_eq!(broken.len(), 1);
        let cut = &e[broken[0]];
        assert_eq!((tables[cut.child].name.as_str(), cut.child_cols[0].as_str()), ("departments", "manager_id"));
        assert_eq!(names(&tables, &o), ["departments", "employees"]);

        // No nullable edge: one is cut anyway, and reported.
        let tables = vec![table("a", &[("b_id", "b", false)]), table("b", &[("a_id", "a", false)])];
        let e = edges(&tables);
        let (o, broken) = order(&scope(&e, 0, None), &e);
        assert_eq!((o.len(), broken.len()), (2, 1));
        assert!(!e[broken[0]].nullable);
    }

    #[test]
    fn self_references_order_rows() {
        // 0 → 2 → 1 (root), 3 → 3 (itself), 4 → 5 → 4 (cycle).
        let order = rows_parents_first(&[Some(2), None, Some(1), Some(3), Some(5), Some(4)]);
        let pos = |i: usize| order.iter().position(|&x| x == i).unwrap();
        assert_eq!(order.len(), 6);
        assert!(pos(1) < pos(2) && pos(2) < pos(0));
    }
}
