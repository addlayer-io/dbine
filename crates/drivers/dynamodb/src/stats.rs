//! What the service already knows, for documenting the tables
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `ItemCount` of `DescribeTable`, for the table and each of its
//!   global and local secondary indexes: DynamoDB refreshes it about every
//!   six hours, so it lags recent writes. Never a `Scan` with
//!   `Select=COUNT`, which reads (and bills) the whole table.
//! - Comments: DynamoDB has none (tags are key-value labels, not comments).

use crate::DynamoSession;
use aws_sdk_dynamodb::types::TableDescription;
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};

/// The table's and its indexes' item counts. Indexes ride with the table in
/// `schema`, as `list_objects` lists them.
pub(crate) fn estimates(table: &str, d: &TableDescription) -> Vec<RowEstimate> {
    let est = |kind: &str, schema: Option<&str>, name: &str, n: Option<i64>| {
        n.filter(|n| *n >= 0).map(|n| RowEstimate {
            object: ObjectRef { kind: kind.into(), schema: schema.map(str::to_string), name: name.into() },
            rows: n as u64,
        })
    };
    let mut out: Vec<RowEstimate> = est(kinds::TABLE, None, table, d.item_count()).into_iter().collect();
    for g in d.global_secondary_indexes() {
        out.extend(g.index_name().and_then(|n| est(kinds::INDEX, Some(table), n, g.item_count())));
    }
    for l in d.local_secondary_indexes() {
        out.extend(l.index_name().and_then(|n| est(kinds::INDEX, Some(table), n, l.item_count())));
    }
    out
}

pub(crate) async fn row_estimates(s: &DynamoSession) -> Result<Vec<RowEstimate>> {
    use futures::stream::{self, StreamExt};
    let tables = s.table_names().await?;
    let client = &s.client;
    let described: Vec<(String, Option<TableDescription>)> = stream::iter(tables)
        .map(|t| async move {
            let d = client.describe_table().table_name(&t).send().await.ok().and_then(|o| o.table);
            (t, d)
        })
        .buffered(8)
        .collect()
        .await;
    Ok(described.iter().filter_map(|(t, d)| Some(estimates(t, d.as_ref()?))).flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_dynamodb::types::{GlobalSecondaryIndexDescription, LocalSecondaryIndexDescription};

    #[test]
    fn table_and_index_counts() {
        let d = TableDescription::builder()
            .table_name("Music")
            .item_count(12)
            .global_secondary_indexes(GlobalSecondaryIndexDescription::builder().index_name("byArtist").item_count(10).build())
            .local_secondary_indexes(LocalSecondaryIndexDescription::builder().index_name("byYear").build())
            .build();
        let e = estimates("Music", &d);
        let v: Vec<(&str, Option<&str>, &str, u64)> =
            e.iter().map(|e| (e.object.kind.as_str(), e.object.schema.as_deref(), e.object.name.as_str(), e.rows)).collect();
        assert_eq!(v, vec![("table", None, "Music", 12), ("index", Some("Music"), "byArtist", 10)]);
    }
}
