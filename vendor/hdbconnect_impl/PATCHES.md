# DBine patches over hdbconnect_impl 0.32.0

Upstream: https://crates.io/crates/hdbconnect_impl/0.32.0 (published
sources, all files kept; the license files come from the upstream
repository, https://github.com/emabee/rust-hdbconnect, since the published
crate doesn't ship them). Used by `hdbconnect_async` 0.32, the client of
DBine's SAP HANA driver. Every changed site is marked `PATCH(dbine)` in the
source.

## Applied

1. **`REAL_VECTOR` (SAP HANA Cloud)** —
   `src/protocol/parts/type_id.rs`, `src/protocol/parts/hdb_value.rs`,
   `src/protocol/parts/connect_options.rs`, `src/serde_db_impl/ser.rs`.
   - The client asks for data format version 9 instead of 8. Level 9 adds
     `REAL_VECTOR` (type code 96) and nothing else; a server without it
     answers with the highest level it has, so older servers behave as
     before. With level 8, HANA Cloud sends a vector as `VARBINARY`, and a
     computed vector or a view's can't be told from binary data.
   - `TypeId::REAL_VECTOR = 96`. Its values travel like `VARBINARY` (a
     length indicator, then the bytes) in the type's binary form: the
     dimension as a little-endian u32, then the little-endian float32
     values. They are read as `HdbValue::BINARY`; a `BINARY` value may be
     sent for a `REAL_VECTOR` parameter, as SAP's own Node.js client does.
   - The wire format follows SAP's node-hdb (`lib/protocol/Reader.js`
     `readVector`, `Writer.js` `writeVector`, `common/DataFormatVersion.js`
     `LEVEL9`, Apache-2.0). Tests: `type_id::test::real_vector_type_code`,
     `hdb_value::test::real_vector_is_read_as_its_binary_form` (one-, two- and
     four-byte lengths, the type's 65,000-dimension maximum, NULL).
   - Not tested against a HANA Cloud server (none available to the
     project).

## Upstream test that already fails

`protocol::part_attributes::test::test_part_attributes` fails on the
unmodified 0.32.0 sources too (it combines the flags with `&` instead of
`|`). It is left as is; vendored crates aren't workspace members, so
`cargo test --workspace` doesn't run it.
