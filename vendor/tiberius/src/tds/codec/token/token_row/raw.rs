//! PATCH(dbine): raw row passthrough.
//!
//! Reads a ROW / NBCROW token's column values as raw TDS bytes, without
//! decoding them into `ColumnData` (no UTF-16 <-> UTF-8 conversion, no
//! allocation per value). The output is a complete ROW token (token byte
//! included) — NBCROW null-bitmap rows are expanded, writing each null
//! column's NULL marker — so it can be re-sent as-is in a bulk load
//! (`BulkLoadRequest::send_raw_rows`) whose column metadata has the same types.

use crate::tds::codec::{FixedLenType, TypeInfo, VarLenType};
use crate::SqlReadBytes;
use bytes::{BufMut, BytesMut};
use futures_util::io::AsyncReadExt;

const PLP_NULL: u64 = 0xFFFF_FFFF_FFFF_FFFF;
const PLP_UNKNOWN: u64 = 0xFFFF_FFFF_FFFF_FFFE;

/// Decode one ROW (`nbc = false`) or NBCROW (`nbc = true`) token body into
/// a raw ROW token, using the current result set's column metadata.
pub(crate) async fn decode_raw_row<R>(src: &mut R, nbc: bool) -> crate::Result<BytesMut>
where
    R: SqlReadBytes + Unpin,
{
    let meta = src
        .context()
        .last_meta()
        .ok_or_else(|| crate::Error::Protocol("row before column metadata".into()))?;
    let n = meta.columns.len();

    let bitmap = if nbc {
        let mut b = vec![0u8; n.div_ceil(8)];
        src.read_exact(&mut b).await?;
        Some(b)
    } else {
        None
    };

    let mut out = BytesMut::with_capacity(64);
    out.put_u8(crate::TokenType::Row as u8);
    for (i, column) in meta.columns.iter().enumerate() {
        let is_null = bitmap
            .as_ref()
            .map(|b| b[i / 8] & (1 << (i % 8)) != 0)
            .unwrap_or(false);
        if is_null {
            put_null(&mut out, &column.base.ty)?;
        } else {
            copy_value(src, &column.base.ty, &mut out).await?;
        }
    }
    Ok(out)
}

async fn copy_exact<R>(src: &mut R, out: &mut BytesMut, len: usize) -> crate::Result<()>
where
    R: SqlReadBytes + Unpin,
{
    let start = out.len();
    out.resize(start + len, 0);
    src.read_exact(&mut out[start..]).await?;
    Ok(())
}

fn fixed_size(ty: FixedLenType) -> usize {
    match ty {
        FixedLenType::Null => 0,
        FixedLenType::Int1 | FixedLenType::Bit => 1,
        FixedLenType::Int2 => 2,
        FixedLenType::Int4
        | FixedLenType::Float4
        | FixedLenType::Money4
        | FixedLenType::Datetime4 => 4,
        FixedLenType::Int8 | FixedLenType::Float8 | FixedLenType::Money | FixedLenType::Datetime => {
            8
        }
    }
}

/// How a type's value is framed on the wire.
enum Framing {
    Fixed(usize),
    /// 1-byte length prefix (0 = NULL).
    ByteLen,
    /// 2-byte length prefix (0xFFFF = NULL).
    UShortLen,
    /// Partially length-prefixed (MAX types, XML): 8-byte total, then chunks.
    Plp,
    /// TEXT / NTEXT / IMAGE: text pointer + timestamp + 4-byte length.
    LongLen,
}

fn unsupported(what: impl std::fmt::Debug) -> crate::Error {
    crate::Error::Protocol(format!("raw rows: unsupported type {:?}", what).into())
}

fn framing(ty: &TypeInfo) -> crate::Result<Framing> {
    Ok(match ty {
        TypeInfo::FixedLen(f) => Framing::Fixed(fixed_size(*f)),
        TypeInfo::VarLenSizedPrecision { .. } => Framing::ByteLen,
        TypeInfo::Xml { .. } => Framing::Plp,
        TypeInfo::Udt(info) => return Err(unsupported(&info.type_name)),
        TypeInfo::VarLenSized(ctx) => match ctx.r#type() {
            VarLenType::BigVarChar
            | VarLenType::BigVarBin
            | VarLenType::BigChar
            | VarLenType::BigBinary
            | VarLenType::NVarchar
            | VarLenType::NChar => {
                if ctx.len() >= 0xFFFF {
                    Framing::Plp
                } else {
                    Framing::UShortLen
                }
            }
            VarLenType::Xml => Framing::Plp,
            VarLenType::Text | VarLenType::NText | VarLenType::Image => Framing::LongLen,
            VarLenType::Udt | VarLenType::SSVariant => return Err(unsupported(ctx.r#type())),
            // Guid, Intn, Bitn, Decimaln, Numericn, Floatn, Money, Datetimen,
            // Daten, Timen, Datetime2, DatetimeOffsetn
            _ => Framing::ByteLen,
        },
    })
}

async fn copy_value<R>(src: &mut R, ty: &TypeInfo, out: &mut BytesMut) -> crate::Result<()>
where
    R: SqlReadBytes + Unpin,
{
    match framing(ty)? {
        Framing::Fixed(n) => copy_exact(src, out, n).await,
        Framing::ByteLen => {
            let len = src.read_u8().await?;
            out.put_u8(len);
            copy_exact(src, out, len as usize).await
        }
        Framing::UShortLen => {
            let len = src.read_u16_le().await?;
            out.put_u16_le(len);
            if len == 0xFFFF {
                return Ok(());
            }
            copy_exact(src, out, len as usize).await
        }
        Framing::Plp => {
            let total = src.read_u64_le().await?;
            if total == PLP_NULL {
                out.put_u64_le(PLP_NULL);
                return Ok(());
            }
            // The total is re-sent as "unknown" (what the encoder itself
            // sends): with a known total, a bulk load into an xml column
            // parses UTF-16 data as single-byte and rejects it.
            out.put_u64_le(PLP_UNKNOWN);
            loop {
                let chunk = src.read_u32_le().await?;
                out.put_u32_le(chunk);
                if chunk == 0 {
                    return Ok(());
                }
                copy_exact(src, out, chunk as usize).await?;
            }
        }
        Framing::LongLen => {
            let ptr_len = src.read_u8().await?;
            out.put_u8(ptr_len);
            if ptr_len == 0 {
                return Ok(());
            }
            copy_exact(src, out, ptr_len as usize + 8).await?; // text pointer + timestamp
            let len = src.read_u32_le().await?;
            out.put_u32_le(len);
            copy_exact(src, out, len as usize).await
        }
    }
}

/// The NULL marker of a column in plain ROW layout (for NBCROW nulls).
fn put_null(out: &mut BytesMut, ty: &TypeInfo) -> crate::Result<()> {
    match framing(ty)? {
        Framing::Fixed(0) => {}
        Framing::Fixed(_) => {
            return Err(crate::Error::Protocol(
                "raw rows: NULL in a fixed-length (NOT NULL) column".into(),
            ))
        }
        Framing::ByteLen | Framing::LongLen => out.put_u8(0),
        Framing::UShortLen => out.put_u16_le(0xFFFF),
        Framing::Plp => out.put_u64_le(PLP_NULL),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_read_bytes::test_utils::IntoSqlReadBytes;
    use crate::tds::codec::{BaseMetaDataColumn, MetaDataColumn, TokenColMetaData, VarLenContext};
    use crate::tds::Collation;
    use crate::ColumnFlag;
    use std::sync::Arc;

    fn col(ty: TypeInfo) -> MetaDataColumn<'static> {
        MetaDataColumn {
            base: BaseMetaDataColumn {
                flags: ColumnFlag::Nullable.into(),
                ty,
                table_name: None,
            },
            col_name: "c".into(),
        }
    }

    fn intn() -> TypeInfo {
        TypeInfo::VarLenSized(VarLenContext::new(VarLenType::Intn, 4, None))
    }

    fn nvarchar(len: usize) -> TypeInfo {
        TypeInfo::VarLenSized(VarLenContext::new(
            VarLenType::NVarchar,
            len,
            Some(Collation::new(13632521, 52)),
        ))
    }

    async fn raw(types: Vec<TypeInfo>, body: BytesMut, nbc: bool) -> crate::Result<BytesMut> {
        let meta = TokenColMetaData {
            columns: types.into_iter().map(col).collect(),
        };
        let mut reader = body.into_sql_read_bytes();
        reader.context_mut().set_last_meta(Arc::new(meta));
        decode_raw_row(&mut reader, nbc).await
    }

    #[tokio::test]
    async fn row_is_copied_verbatim_with_token_byte() {
        let mut body = BytesMut::new();
        body.put_i32_le(42); // int NOT NULL
        body.put_u8(4); // intn
        body.put_i32_le(-1);
        body.put_u16_le(4); // nvarchar(10): "hi"
        body.put_slice(&[b'h', 0, b'i', 0]);
        body.put_u16_le(0xFFFF); // nvarchar NULL

        let out = raw(
            vec![
                TypeInfo::FixedLen(FixedLenType::Int4),
                intn(),
                nvarchar(20),
                nvarchar(20),
            ],
            body.clone(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(out[0], crate::TokenType::Row as u8);
        assert_eq!(&out[1..], &body[..]);
    }

    #[tokio::test]
    async fn nbcrow_nulls_are_expanded_to_row_markers() {
        let text = TypeInfo::VarLenSized(VarLenContext::new(VarLenType::Text, 0, None));
        let types = vec![
            intn(),                                  // 0: null
            nvarchar(20),                            // 1: "a"
            nvarchar(0xFFFF),                        // 2: null (PLP)
            text,                                    // 3: null
            nvarchar(20),                            // 4: null
            TypeInfo::FixedLen(FixedLenType::Null),  // 5: null, no bytes
            intn(),                                  // 6: 7
            intn(),                                  // 7: 8
            intn(),                                  // 8: null (second bitmap byte)
        ];
        let mut body = BytesMut::new();
        body.put_u8(0b0011_1101); // columns 0, 2, 3, 4, 5
        body.put_u8(0b0000_0001); // column 8
        body.put_u16_le(2);
        body.put_slice(&[b'a', 0]);
        body.put_u8(4);
        body.put_i32_le(7);
        body.put_u8(4);
        body.put_i32_le(8);

        let out = raw(types, body, true).await.unwrap();

        let mut expected = BytesMut::new();
        expected.put_u8(crate::TokenType::Row as u8);
        expected.put_u8(0); // intn null
        expected.put_u16_le(2); // "a"
        expected.put_slice(&[b'a', 0]);
        expected.put_u64_le(PLP_NULL); // nvarchar(max) null
        expected.put_u8(0); // text null
        expected.put_u16_le(0xFFFF); // nvarchar null; FixedLen(Null) writes nothing
        expected.put_u8(4); // 7
        expected.put_i32_le(7);
        expected.put_u8(4); // 8
        expected.put_i32_le(8);
        expected.put_u8(0); // column 8 null
        assert_eq!(out, expected);
    }

    #[tokio::test]
    async fn plp_total_is_resent_as_unknown() {
        let mut body = BytesMut::new();
        body.put_u64_le(4); // known total
        body.put_u32_le(4);
        body.put_slice(&[b'o', 0, b'k', 0]);
        body.put_u32_le(0); // terminator

        let out = raw(vec![nvarchar(0xFFFF)], body, false).await.unwrap();

        let mut expected = BytesMut::new();
        expected.put_u8(crate::TokenType::Row as u8);
        expected.put_u64_le(PLP_UNKNOWN);
        expected.put_u32_le(4);
        expected.put_slice(&[b'o', 0, b'k', 0]);
        expected.put_u32_le(0);
        assert_eq!(out, expected);
    }

    #[tokio::test]
    async fn text_value_keeps_pointer_and_timestamp() {
        let mut body = BytesMut::new();
        body.put_u8(16);
        body.put_slice(&[1u8; 16]); // text pointer
        body.put_slice(&[2u8; 8]); // timestamp
        body.put_u32_le(3);
        body.put_slice(b"abc");

        let ty = TypeInfo::VarLenSized(VarLenContext::new(
            VarLenType::Text,
            0x7FFF_FFFF,
            Some(Collation::new(13632521, 52)),
        ));
        let out = raw(vec![ty], body.clone(), false).await.unwrap();
        assert_eq!(&out[1..], &body[..]);
    }

    #[tokio::test]
    async fn null_in_fixed_column_is_an_error() {
        let mut body = BytesMut::new();
        body.put_u8(0b0000_0001);
        let err = raw(vec![TypeInfo::FixedLen(FixedLenType::Int4)], body, true)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::Error::Protocol(_)));
    }

    #[tokio::test]
    async fn sql_variant_is_refused() {
        let ty = TypeInfo::VarLenSized(VarLenContext::new(VarLenType::SSVariant, 8016, None));
        let mut body = BytesMut::new();
        body.put_u32_le(0);
        let err = raw(vec![ty], body, false).await.unwrap_err();
        assert!(matches!(err, crate::Error::Protocol(_)));
    }

    #[tokio::test]
    async fn row_before_metadata_is_an_error() {
        let mut reader = BytesMut::new().into_sql_read_bytes();
        let err = decode_raw_row(&mut reader, false).await.unwrap_err();
        assert!(matches!(err, crate::Error::Protocol(_)));
    }
}
