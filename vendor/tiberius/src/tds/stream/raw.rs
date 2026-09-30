//! PATCH(dbine): a query result stream whose rows stay as raw TDS bytes.
//!
//! Used to pipe rows from a `SELECT` straight into a bulk load
//! (`BulkLoadRequest::send_raw_rows`) without decoding and re-encoding each
//! value. Only valid when both sides describe the columns the same way; see
//! [`RawMetadata::check_compatible`].

use crate::tds::codec::{MetaDataColumn, TokenColMetaData, TypeInfo, VarLenType};
use crate::tds::stream::ReceivedToken;
use bytes::BytesMut;
use futures_util::{
    ready,
    stream::{BoxStream, Stream, StreamExt},
};
use std::{
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

/// An item of a [`RawRowStream`].
#[derive(Debug)]
pub enum RawItem {
    /// Column metadata of the result set that follows.
    Metadata(RawMetadata),
    /// One row, as a complete ROW token (NBCROW rows are expanded).
    Row(BytesMut),
}

/// Column metadata of a raw result set, or of a bulk load's target columns
/// (`Client::bulk_metadata`).
#[derive(Debug, Clone)]
pub struct RawMetadata(Arc<TokenColMetaData<'static>>);

impl RawMetadata {
    /// The columns, as the server described them.
    pub fn columns(&self) -> &[MetaDataColumn<'static>] {
        &self.0.columns
    }

    /// The column names, in order.
    pub fn column_names(&self) -> Vec<&str> {
        self.0.columns.iter().map(|c| c.col_name.as_ref()).collect()
    }

    /// Whether each column may hold NULL, as the server described it.
    pub fn nullable(&self) -> Vec<bool> {
        self.0
            .columns
            .iter()
            .map(|c| c.base.flags.contains(crate::ColumnFlag::Nullable))
            .collect()
    }

    pub(crate) fn from_columns(columns: Vec<MetaDataColumn<'static>>) -> Self {
        Self(Arc::new(TokenColMetaData { columns }))
    }

    /// `Ok` when rows of this result set (the source) can be sent as-is into
    /// a bulk load declared as `bulk` (from `Client::bulk_metadata`): same
    /// column count and, per column, the same wire type — including lengths,
    /// precision, scale and, for non-Unicode text, the code page. `Err`
    /// explains the first difference.
    pub fn check_compatible(&self, bulk: &RawMetadata) -> Result<(), String> {
        check_columns(&self.0.columns, &bulk.0.columns)
    }
}

fn check_columns(src: &[MetaDataColumn<'_>], dst: &[MetaDataColumn<'_>]) -> Result<(), String> {
    if src.len() != dst.len() {
        return Err(format!(
            "{} columns in the source, {} in the destination",
            src.len(),
            dst.len()
        ));
    }
    for (s, d) in src.iter().zip(dst) {
        if !same_wire_type(&s.base.ty, &d.base.ty) {
            return Err(format!(
                "column {}: source {:?}, destination {:?}",
                d.col_name, s.base.ty, d.base.ty
            ));
        }
    }
    Ok(())
}

fn same_wire_type(a: &TypeInfo, b: &TypeInfo) -> bool {
    match (a, b) {
        (TypeInfo::VarLenSized(x), TypeInfo::VarLenSized(y)) => {
            if x.r#type() != y.r#type() || x.len() != y.len() {
                return false;
            }
            match x.r#type() {
                // Unicode travels as UTF-16 whatever the collation.
                VarLenType::NVarchar | VarLenType::NChar | VarLenType::NText => true,
                VarLenType::BigVarChar | VarLenType::BigChar | VarLenType::Text => {
                    match (x.collation(), y.collation()) {
                        (Some(cx), Some(cy)) => {
                            if cx.info() == cy.info() && cx.sort_id() == cy.sort_id() {
                                return true;
                            }
                            // Different collations are fine when the bytes mean
                            // the same: same single/double-byte code page.
                            match (cx.encoding(), cy.encoding()) {
                                (Ok(ex), Ok(ey)) => ex == ey,
                                _ => false,
                            }
                        }
                        (None, None) => true,
                        _ => false,
                    }
                }
                _ => x.collation() == y.collation(),
            }
        }
        // XML travels as UTF-16 whatever the schema collection.
        (TypeInfo::Xml { .. }, TypeInfo::Xml { .. }) => true,
        _ => a == b,
    }
}

/// A result stream yielding [`RawItem`]s. Must be polled to the end before
/// the connection is used again.
pub struct RawRowStream<'a> {
    token_stream: BoxStream<'a, crate::Result<ReceivedToken>>,
}

impl<'a> std::fmt::Debug for RawRowStream<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawRowStream").finish()
    }
}

impl<'a> RawRowStream<'a> {
    pub(crate) fn new(token_stream: BoxStream<'a, crate::Result<ReceivedToken>>) -> Self {
        Self { token_stream }
    }
}

impl<'a> Stream for RawRowStream<'a> {
    type Item = crate::Result<RawItem>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let token = match ready!(this.token_stream.poll_next_unpin(cx)) {
                Some(res) => res?,
                None => return Poll::Ready(None),
            };
            return match token {
                ReceivedToken::NewResultset(meta) => {
                    Poll::Ready(Some(Ok(RawItem::Metadata(RawMetadata(meta)))))
                }
                ReceivedToken::RawRow(bytes) => Poll::Ready(Some(Ok(RawItem::Row(bytes)))),
                _ => continue,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tds::codec::{BaseMetaDataColumn, FixedLenType, VarLenContext};
    use crate::tds::Collation;
    use crate::ColumnFlag;

    fn meta(types: Vec<TypeInfo>) -> RawMetadata {
        RawMetadata::from_columns(
            types
                .into_iter()
                .enumerate()
                .map(|(i, ty)| MetaDataColumn {
                    base: BaseMetaDataColumn {
                        flags: if i == 0 {
                            ColumnFlag::Nullable.into()
                        } else {
                            Default::default()
                        },
                        ty,
                        table_name: None,
                    },
                    col_name: format!("c{i}").into(),
                })
                .collect(),
        )
    }

    fn sized(ty: VarLenType, len: usize, collation: Option<Collation>) -> TypeInfo {
        TypeInfo::VarLenSized(VarLenContext::new(ty, len, collation))
    }

    // Latin1_General (code page 1252), two sort orders.
    fn latin1_ci_as() -> Collation {
        Collation::new(0x00D0_0409, 52)
    }
    fn latin1_cs_as() -> Collation {
        Collation::new(0x0000_0409, 51)
    }
    // Cyrillic_General (code page 1251).
    fn cyrillic() -> Collation {
        Collation::new(0x00D0_0419, 0)
    }

    #[test]
    fn identical_metadata_is_compatible() {
        let a = meta(vec![
            TypeInfo::FixedLen(FixedLenType::Int4),
            sized(VarLenType::NVarchar, 100, Some(latin1_ci_as())),
        ]);
        assert!(a.check_compatible(&a.clone()).is_ok());
        assert_eq!(a.column_names(), vec!["c0", "c1"]);
        assert_eq!(a.nullable(), vec![true, false]);
    }

    #[test]
    fn column_count_mismatch_is_reported() {
        let a = meta(vec![TypeInfo::FixedLen(FixedLenType::Int4)]);
        let b = meta(vec![
            TypeInfo::FixedLen(FixedLenType::Int4),
            TypeInfo::FixedLen(FixedLenType::Int4),
        ]);
        let err = a.check_compatible(&b).unwrap_err();
        assert!(err.contains("1 columns in the source"), "{err}");
    }

    #[test]
    fn lengths_types_and_scales_must_match() {
        let n = |len| meta(vec![sized(VarLenType::NVarchar, len, Some(latin1_ci_as()))]);
        assert!(n(100).check_compatible(&n(200)).is_err());

        let fixed = meta(vec![TypeInfo::FixedLen(FixedLenType::Int4)]);
        let intn = meta(vec![sized(VarLenType::Intn, 4, None)]);
        assert!(fixed.check_compatible(&intn).is_err());

        let dec = |scale| {
            meta(vec![TypeInfo::VarLenSizedPrecision {
                ty: VarLenType::Decimaln,
                size: 9,
                precision: 18,
                scale,
            }])
        };
        assert!(dec(2).check_compatible(&dec(2)).is_ok());
        assert!(dec(2).check_compatible(&dec(4)).is_err());

        let err = dec(2).check_compatible(&dec(4)).unwrap_err();
        assert!(err.starts_with("column c0:"), "{err}");
    }

    #[test]
    fn unicode_ignores_collation() {
        let a = meta(vec![sized(VarLenType::NVarchar, 100, Some(latin1_ci_as()))]);
        let b = meta(vec![sized(VarLenType::NVarchar, 100, Some(cyrillic()))]);
        assert!(a.check_compatible(&b).is_ok());
    }

    #[test]
    fn non_unicode_needs_the_same_code_page() {
        let v = |c: Collation| meta(vec![sized(VarLenType::BigVarChar, 50, Some(c))]);
        // Same code page, different sort order: same bytes.
        assert!(v(latin1_ci_as()).check_compatible(&v(latin1_cs_as())).is_ok());
        // Different code page: the bytes mean different characters.
        assert!(v(latin1_ci_as()).check_compatible(&v(cyrillic())).is_err());
    }
}
