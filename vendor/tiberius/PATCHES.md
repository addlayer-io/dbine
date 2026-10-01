# DBine patches over tiberius 0.13.0

Upstream: https://crates.io/crates/tiberius/0.13.0 (published sources, all
files kept). Every changed site is marked `PATCH(dbine)` in the source.

## Applied

1. **Packet length in its own header** — `src/tds/codec/packet.rs`.
   `Packet::encode` wrote the packet length at `dst[2..4]` of the whole output
   buffer instead of at this packet's header. Harmless while each packet is
   flushed alone (the buffer is empty), but with several packets in one buffer
   it corrupted the first packet's length. Latent fix; test
   `encode_second_packet_keeps_first_length`.

2. **No length limit for `(max)` columns** — `src/tds/codec/column_data.rs`
   (varchar, nvarchar, varbinary encode arms). The length check compared the
   value with the MAX marker (0xFFFF), so bulk-encoding a value longer than
   65535 bytes into `varchar(max)` / `nvarchar(max)` / `varbinary(max)` failed
   with "exceed column limit 65535". It now applies only to sized columns. The
   upstream test that asserted the old behaviour
   (`nvarchar_too_long_unknown_size_errors`) now asserts the value is accepted
   (`nvarchar_max_accepts_over_64k`); new tests `over_64k_values_into_max_columns`
   and `sized_column_limit_still_enforced`.

3. **`time(n)` / `datetimeoffset(n)` in `INSERT BULK`** —
   `src/tds/codec/token/token_col_metadata.rs` (`Display for MetaDataColumn`).
   They were declared without a scale (so as scale 7), and columns of any
   other scale failed with "Invalid column attribute from bcp client". Now
   declared with their scale, as `datetime2(n)` already was.

4. **`nvarchar(n)` / `nchar(n)` declared in characters** — same file. The
   metadata length is in bytes and `INSERT BULK` declares characters:
   `nvarchar(4000)` (8000 bytes) came out as `nvarchar(max)` and the server
   rejected it ("Invalid column type from bcp client"); smaller ones were
   declared twice as long. `nvarchar(max)` is now used only above 8000 bytes.
   Upstream tests that used byte lengths as character counts were adjusted.

5. **Bulk load with a select list** — `src/client.rs`:
   `Client::bulk_insert_with_select(table, select_list, options, order_hints)`
   and `Client::bulk_metadata_select(table, select_list)`. The target columns
   come from `SELECT TOP 0 {select_list} FROM {table}` instead of plain column
   names, so a column can be declared to `INSERT BULK` as a type the encoder
   supports and the server converts on insert (`xml` -> `nvarchar(max)`,
   `text` -> `varchar(max)`, `image` / spatial / `hierarchyid` ->
   `varbinary(max)`), the way other bulk clients load them. Every item of the
   list is a target: columns are not filtered by the `Updateable` flag, which
   expressions never carry and which SQL Server reports inconsistently for
   identity columns under `IDENTITY_INSERT ON`. Upstream
   `bulk_insert_with_options` and `column_metadata` were split into private
   helpers (`start_bulk_insert`, `metadata_query`) shared with the new
   methods; their behaviour is unchanged.

6. **Raw row passthrough** — rows piped from a `SELECT` into a bulk load as TDS
   bytes, without decoding them into `ColumnData` and re-encoding them
   (measured: **~58% less CPU, ~30% faster on a local 1M-row table**).
   * `src/tds/codec/token/token_row/raw.rs` — `decode_raw_row` copies a ROW /
     NBCROW token's values as bytes into a complete ROW token. NBCROW nulls
     are expanded to each type's NULL marker. PLP totals are re-sent as
     "unknown length" (0xFFFFFFFFFFFFFFFE): with a known total, a bulk load
     into an `xml` column parses the UTF-16 data as single-byte and rejects
     it. UDT and `sql_variant` columns are refused.
   * `src/tds/stream/token.rs` — `TokenStream::new_raw` yields
     `ReceivedToken::RawRow` instead of decoded rows.
   * `src/tds/stream/raw.rs` — public `RawRowStream` / `RawItem` /
     `RawMetadata`; `RawMetadata::check_compatible` compares the wire types
     column by column (lengths, precision, scale; Unicode text ignores the
     collation; non-Unicode text needs the same code page).
   * `src/client.rs` — `Client::query_raw_rows(sql)`, and
     `Client::bulk_metadata(table, columns, options)`: the columns
     `bulk_insert_with_options` would declare, without starting a load (the
     up-front compatibility check).
   * `src/tds/codec/bulk_load.rs` — `BulkLoadRequest::send_raw_rows(bytes)`.
   * `src/lib.rs` — exports `RawItem`, `RawMetadata`, `RawRowStream`.

7. **Cancel from another task (TDS attention)** — `Client::cancel_handle()`
   returns a cloneable `CancelHandle` whose `cancel()` stops the request in
   flight while the client is busy reading its results, as SSMS does: an
   Attention packet on the same connection, so the session survives (open
   transaction, `#temp` tables, `SET` options). Upstream `cancel_query`
   needs `&mut Client`, which the reading task holds.
   * `src/client/connection.rs` — `CancelHandle` (a flag plus an
     `AtomicWaker` shared with the connection). `Connection::poll_next`, the
     single point every read goes through, sends the Attention packet
     (`poll_attention`) when a cancel was asked and a response is being read
     (`flushed` false), then keeps reading. The acknowledgement is spotted at
     packet level (`track_attention_ack`: the message ends with a DONE /
     DONEPROC / DONEINPROC carrying `DONE_ATTN`, 0x20). `send` forgets a
     cancel asked while nothing ran, and `flush_stream`
     (`drain_attention_ack`) reads an acknowledgement still due before the
     next request.
   * `src/tds/stream/token.rs` — SQL Server ends the stopped request with its
     own message (a DONE with the error bit) and acknowledges in the next
     one: with an Attention out, the token stream reads on into it instead of
     ending.
   * `src/tds/stream/query.rs`, `src/tds/stream/command.rs`, `src/result.rs`
     — the acknowledging DONE ends the stream with the new
     `Error::Cancelled` (`src/error.rs`); `forward_to_metadata` / `columns`
     leave it for `poll_next`. If the request had already finished, its
     results come complete and the stream still ends with `Cancelled`.
   * `src/client.rs`, `src/lib.rs` — `Client::cancel_handle`, export of
     `CancelHandle`.
   * Tests: `attention_tests` in `connection.rs` (acknowledgement split over
     packets); live ones in DBine's `crates/drivers/sqlserver/src/cancel_live.rs`
     (SQL Server 2022: the session, `#temp` table, `SET` and transaction
     survive; a late Attention's lone acknowledgement isn't taken as the next
     answer; cancel after a reconnect reaches the new connection only).
     Babelfish 5.4 reads the Attention only once the batch is over, so the
     driver doesn't rely on it there.

8. **A batch's whole answer in order (messages, counts, every error)** —
   `Client::simple_query_messages(sql)` returns a `MessageStream` whose
   `MessageItem`s are, in server order: result-set metadata and rows, INFO
   tokens (`PRINT`, `RAISERROR` up to severity 10, `SET STATISTICS IO/TIME`,
   warnings), every ERROR token, each DONE / DONEPROC / DONEINPROC with its
   row count (`None` under `SET NOCOUNT ON`), and the ENVCHANGEs for the
   database (`USE`) and transactions. Upstream `QueryStream` yields only
   metadata and rows, drops INFO, DONE and ENVCHANGE, and ends at the first
   ERROR; DBine's SQL Server driver needs them all to print what SSMS prints.
   * `src/tds/stream/message.rs` (new) — `MessageStream`, `MessageItem`,
     `ServerMessage` (number, state, class, message, server, procedure,
     line), `DoneInfo` / `DoneKind`. The attention acknowledgement (patch 7)
     ends it with `Error::Cancelled`.
   * `src/tds/stream/token.rs` — `TokenStream::new_messages`: with
     `errors_as_items`, an ERROR token is only an item, so the stream doesn't
     end with the first one as its error.
   * `src/tds/codec/token/token_done.rs` — crate-private accessors `count()`
     (the row count when `DONE_COUNT` is set), `is_more()`, `is_error()`,
     `cur_cmd()`, and `from_parts()` for tests.
   * `src/tds/stream.rs`, `src/lib.rs` — module and exports (`DoneInfo`,
     `DoneKind`, `MessageItem`, `MessageStream`, `ServerMessage`).
   * `src/client.rs` — `Client::simple_query_messages`.
   * Tests: `messages_counts_and_errors_come_in_order` and
     `the_attention_ack_ends_it_cancelled` in `message.rs` (run from a copy
     of this folder with an empty `[workspace]` table, `--no-default-features
     --features tds73,chrono,rust_decimal,rustls,sql-browser-tokio`: tiberius
     isn't a workspace member). Live ones in DBine's
     `crates/drivers/sqlserver/src/script_live.rs`.

## Not needed on 0.13 (already upstream)

* **Requested packet size** — `Config::packet_size(n)` exists upstream and is
  sent in LOGIN7; the server's ENVCHANGE answer is what the connection then
  uses. Requesting 32767 instead of the default 4096 means **~8x fewer
  packets / TLS records** for bulk loads. (Upstream does not clamp the value:
  callers pass 512..=32767.)
* **`INSERT BULK ... WITH (TABLOCK)` and an explicit column list** —
  `Client::bulk_insert_with_options(table, columns, options, order_hints)`
  with `SqlBulkCopyOption::TableLock` (also `CheckConstraints`, `KeepNulls`,
  `FireTriggers`, `ORDER(...)`). Identity columns: pass
  `SqlBulkCopyOption::KeepIdentity`, which keeps them by the identity flag
  instead of relying on the inconsistent `Updateable` flag.
* **Nullable `money` / `smallmoney` in `INSERT BULK`** — the `Moneyn`
  `Display` arm exists upstream.
* **Exact `money` / `smallmoney` bulk encoding** — upstream encodes `F64` and,
  exactly, `Numeric` values into money columns (fixed and nullable). Send a
  `decimal(19,4)` value (`ColumnData::Numeric`) for exact results; the
  scaled-`I64` convention is therefore not ported. (Money is still decoded as
  `f64`; the raw passthrough copies it exactly.)
* **`date` TYPE_INFO without a length byte** — fixed upstream in
  `VarLenContext::encode`.
* **Bracketed column names in `INSERT BULK`** (`[name]`, `]` -> `]]`) — done
  upstream in `Display for MetaDataColumn`; kept a test for it.

## Not vendored: TLS test fixtures

`docker/` (the upstream Dockerfiles and the self-signed test CA, server
certificate and private keys) and `tests/custom-cert.rs` are left out of this
copy: DBine doesn't run tiberius's own TLS tests, and a private key in the
repository sets off secret scanners even when it's a public test fixture.
The unit tests that read those files are marked `#[ignore]` (`PATCH(dbine)`):
the ones in `src/client/tls_stream/certs.rs`, ten in
`src/client/tls_stream/rustls_tls_stream.rs` (the `build_trust_store_*` ones
that load a CA file, `load_client_auth_reads_pem_cert_and_key`,
`read_private_key_reads_{pem,der}`, `read_private_key_unsupported_extension_errors`,
`no_cert_verifier_accepts_any_certificate_when_opted_in` and
`default_verifier_rejects_untrusted_certificate`) and the three
`multi_cert_*` ones in `src/client/tls_stream/native_tls_stream.rs`. To run
the crate's tests, copy it out of the workspace (cargo refuses to test it in
place) and run `cargo test --lib`, with the default features (native-tls)
and with `--no-default-features --features
tds73,chrono,rust_decimal,rustls,rustls-webpki-roots,sql-browser-tokio`.
