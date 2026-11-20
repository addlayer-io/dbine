# Vectors (embeddings)

DBine reads, edits, indexes, compares, moves and searches vectors in every
engine that stores them: `vector(n)` in PostgreSQL with pgvector,
`VECTOR(n)` in MySQL, Oracle, SQL Server and Snowflake, `dense_vector` in
Elasticsearch, vector fields in Redis, MongoDB Atlas and many more. The same
screens work in all of them; each engine keeps its own types, index
algorithms and distances.

What each engine supports, and what is missing and why, is in
[`engine-support.md`](engine-support.md#vectors).

## What you see in the app

### Results and the grid

- A vector cell shows its first numbers, as many as fit the column
  (`[0.12, -0.03, 0.88, …]`), and a badge with its dimensions. The badge's
  tooltip gives the dimensions and the element type (`float32`, `float16`,
  `int8`, `bit`…). **Copy cell** copies the whole vector.
- Sparse vectors show the engine's own text (`{1:0.5,4:2}/5` in pgvector,
  `[5,[1,4],[0.5,2]]` in Oracle).
- Where the driver doesn't mark a result column, the app recognises the
  column's type name (`vector(3)`, `F32_BLOB(3)`, `knn_vector(3)`,
  `vector<float, 3>`…). Arrays of floats that an engine uses for embeddings
  (`FLOAT[n]`, `Array(Float32)`, `ARRAY<FLOAT64>`, `LIST<FLOAT>`) are shown as
  vectors too, but they are not edited as vectors unless the driver marks
  them.
- **Filters:** a vector column filters by NULL / NOT NULL or by an SQL
  condition from the filter menu (for example `embedding <-> '[…]' < 0.5`).
  Other text is refused with the reason, because no engine compares vectors as
  text.
- **Sort:** sorting by a vector column sorts the **loaded rows** number by
  number, and a note says so. It is never sent to the server and exports don't
  apply it.

### Cell viewer

Opening a vector cell shows its element type, its dimensions (and the
column's declared ones when they differ), its L2 norm, minimum and maximum,
and every number in rows of 8 with their positions. **Copy as JSON** copies
it. Sparse vectors show their text as it is.

### Editing

- An edited or new vector must be a JSON array of finite numbers, not empty,
  with the column's dimensions. `int8`, `uint8` and bit vectors must stay
  within their element's range; `float16`, `bfloat16` and `float32` vectors
  must not overflow it. The same check applies in the grid, in the JSON tree
  of document results and in Add document.
- DBine sends the driver an array of numbers and the driver writes its
  engine's literal: `'[1,2]'`, `STRING_TO_VECTOR(…)`, `VEC_FromText(…)`,
  `N'[…]'`, `vector32('[…]')`, `TO_REAL_VECTOR(…)`, an array literal, a packed
  blob… Where the column's type matters (the element, the form it is stored
  in), the script is written from the types of the result or the describe.
- Vector columns are never part of the `WHERE` of an `UPDATE` or `DELETE`.
  Without a primary key they are left out of the all-columns `WHERE`, with a
  note. A primary key that includes a vector column can't identify the row.
- Sparse vectors are edited as the engine's text, unchecked.
- **Copy as SQL INSERTs / UPDATEs** uses the table's driver, so each vector is
  the engine's literal. JSON, JSON Lines, YAML and Mongo copies write arrays.

### Table designer

Where the engine has vector indexes, each index in the designer has a
**Vector** switch. The vector column comes first (any vector type, or an array
of floats such as `FLOAT[n]`, `Array(Float32)` or `ARRAY<FLOAT64>`, or
pgvector's `bit(n)`), then the algorithm, the distance, the build parameters
(empty means the engine's default) and the filter columns where the engine
takes them. The designer offers each engine's vector type (for example
`vector(1536)`). It refuses, with the reason, what the engine would refuse: a
distance the algorithm doesn't have for that type, a column without a
dimension where the engine needs one, a parameter out of the engine's range.

Editing a table loads its vector indexes. Changing an index's algorithm,
distance, parameters or column drops it and creates it again, with a note.
Engines whose designer has no vector indexes keep an existing one as it is,
read-only.

### Explorer and the Indexes tab

- Index nodes in the explorer carry a short tag, for example
  `HNSW · Cosine`, `IVFFLAT · L2` or `DISKANN · Cosine`. Hovering shows the
  whole line with the build parameters.
- The **Indexes** tab has a **Vector index** column with the algorithm,
  distance and settings, and **Drop index…** and **Disable index…** show them.
- On Redis, Valkey and Dragonfly the explorer has a **Search indexes** folder
  after the key search (see [Redis, Valkey and Dragonfly](#redis-valkey-and-dragonfly)).

### Schema compare and sync

Each side shows a vector index's algorithm, distance and parameters. A
difference in them alone is listed and highlighted, and the sync drops and
creates the index. Two indexes that differ only in their distance or settings
are told apart in every engine. See [`schema-compare.md`](schema-compare.md).

### Data compare

- Vector columns compare by value: `[1e-07, 2]` equals `[0.0000001,2.0]`. Two
  columns compare at the lower precision of their sides (float32 unless both
  sides keep 64-bit numbers), so a float64 vector equals the float32 one a
  sync writes from it. Against a text, JSON or schemaless field, both sides
  are compared by value. Sparse vectors compare as the engine's text.
- A vector column is never the default key.
- Vector cells show their first numbers and dimensions. When two vectors
  differ, both sides start at the first number that differs (`from #10`), with
  enough digits to tell them apart. Double-click opens a cell whole.
- The sync script writes vectors as the target engine's literal, and as arrays
  into schemaless or JSON fields. Exported lists write JSON arrays.
- BigQuery `ARRAY<FLOAT64>`, Doris and StarRocks `ARRAY<FLOAT>`, Spanner
  `ARRAY<FLOAT32>` without `vector_length`, and Neo4j `LIST<FLOAT>` show as
  vectors in table and query compares. They still compare, export and sync
  as the arrays they are.

See [`data-compare.md`](data-compare.md).

### Transfers, migration and clone

Vectors travel between engines as a JSON array of numbers, whole; each target
encodes it for its column's type. Reads from engines that send binary
(MySQL, SQL Server, SAP HANA, libSQL, MongoDB BinData…) are decoded to that
array first. This holds for [bulk transfer](bulk-transfer.md),
[migration](migration.md), **Clone table**, copy a subset, test data and
imports. Moves between pgvector and MySQL 9, MariaDB, GreptimeDB, Manticore,
SQL Server, Oracle, Elasticsearch, OpenSearch, Open Distro, Solr, Redis,
Cassandra, ScyllaDB, libSQL and MongoDB were run live and kept the values
exact (see the table below for what was tested where).

- **Binary vectors.** pgvector reads `bit(n)` as its bit string (`1011…`).
  When the target column holds vectors (a migration's libSQL `F1BIT_BLOB`,
  MongoDB `vector(n, bit)` or Oracle `VECTOR(n, BINARY)`), the transfer hands
  the target its bits, one 0 or 1 per dimension, so they arrive as that
  engine's binary vectors and its vector index finds them. Into a column that
  isn't a vector, a bit string stays a bit string. The other way, libSQL's
  1-bit vectors and Oracle's `BINARY` ones load into `bit(n)`.
- **Sparse vectors.** Oracle's `SPARSE` vectors (`[n,[i…],[v…]]`, indices from
  0) load into pgvector's `sparsevec` as `{i+1:v}/n`, in transfers, clones and
  insert scripts.

### Schema conversion between engines

[`schema-conversion.md`](schema-conversion.md) converts the type and the
vector index. The converter uses the **target driver's real abilities** (its
designer): a target whose designer has no vector indexes gets the table
without them, and the conversion report says why.

**Types**

- A vector keeps its dimension and element where the target has an exact type
  (`vector(n)`, `halfvec(n)`, `VECTOR(n, FLOAT32)`, `F32_BLOB(n)`,
  `FLOAT_VECTOR(n)`, `dense_vector(n)`, `VECTOR(FLOAT, n)`…). Where the target
  has a different precision, the report says the number was approximated (for
  example `VECTOR(INT, n)` into a float target).
- A vector into an engine of the same family that has **no** vector type
  (pgvector into Materialize, H2, Yellowbrick, Redshift, Denodo or Aurora
  DSQL; libSQL into SQLite; SQL Server 2025 into Fabric or Babelfish) becomes
  an array of numbers (or JSON or text where needed), reported as
  approximated.
- Into an engine with no vectors at all (see
  [the list](#engines-without-vectors)), a vector becomes a list of numbers or
  JSON text with a warning.
- A sparse vector becomes text in the source's notation where the target has
  no sparse type (`VARCHAR` in DuckDB, `String` in ClickHouse, text in
  libSQL, Cassandra and Neo4j, JSON in BigQuery).
- A double vector moved into DuckDB keeps `DOUBLE[n]`, so no precision is lost,
  and loses its index (DuckDB indexes only `FLOAT[n]`).
- A `bit(n)` column searched by a Hamming or Jaccard index moves to other
  engines as a binary vector with its index: MongoDB `vector(n, bit)` searched
  by Euclidean, Oracle `VECTOR(n, BINARY)` with the same distance, libSQL
  `F1BIT_BLOB(n)` searched by cosine (see the indexes below). Its rows arrive
  as those binary vectors (see [Transfers](#transfers-migration-and-clone)).
  A `bit(n)` column without such an index is a plain bit string and stays one.
- Half precision (pgvector `halfvec`, SQL Server `float16`) becomes SAP HANA's
  `HALF_VECTOR`, with a note that older HANA Cloud releases need
  `REAL_VECTOR`.
- Doris and StarRocks `ARRAY<FLOAT>` reaching other engines still converts as
  a float array: the type alone doesn't say it holds embeddings.
- The dimension a source index holds (Manticore's `knn_dims`, Doris and
  StarRocks' `dim`, ClickHouse's `vector_similarity` dimension) becomes the
  target column's where the target declares it in the column.

**Indexes**

- A vector index goes only to a column that can take it. One whose column has
  no dimension (where the target needs one), is sparse, or is outside the
  target's limits is **left out**, and the report says why. The table is still
  created.
- The algorithm and the distance are mapped to the closest one the target has,
  and the report lists each change as an "index changed" note. For example:
  pgvector `hnsw` becomes `ybhnsw` on YugabyteDB, a C-SPANN `CREATE VECTOR
  INDEX` on CockroachDB, `HNSW_FLAT` on SingleStore, MHNSW on MariaDB; `ivfflat`
  becomes `ivf_flat` on OceanBase; a squared Euclidean distance becomes
  `euclidean` in MongoDB and Cosmos DB; Hamming over bit vectors becomes Euclidean in MongoDB, DocumentDB, FerretDB
  and Cosmos DB, and `L2_SQUARED` or `l2_norm` in Couchbase; an Oracle
  `BINARY` index without a distance is created with `HAMMING`. In libSQL,
  Hamming over 1-bit vectors becomes cosine: libSQL has no Hamming distance,
  and its cosine distance on 1-bit vectors counts the bits that differ, which
  is the Hamming distance (checked on `sqld` 0.24.33). A Jaccard index has no
  equal there and is left out. L2 or L1 over bits becomes Hamming in pgvector.
- Into Elasticsearch, OpenSearch, Open Distro and Solr a vector index is mapped
  by the vector field its column becomes there; see
  [their section](#elasticsearch-opensearch-open-distro-and-solr).
- Limits of the target are applied: HNSW parameters outside Atlas's ranges
  (`maxEdges` 16–64, `numEdgeCandidates` 100–3200) go to the nearest allowed
  value; Cosmos DB's `flat` index over more than 505 dimensions becomes
  `quantizedFlat`; Neo4j takes dimensions 1–4096, M 1–512 and ef_construction
  1–3200 and cosine or Euclidean only; Memgraph's quantization becomes its
  number type (`int8`, `f16`, `bf16`, `f64`, `b1x8`) and any other quantization
  leaves the index out; MongoDB (Atlas 8192), DocumentDB and FerretDB (2000),
  Cosmos DB (4096) and Couchbase (4096) don't create an index past their
  maximum number of dimensions; Cassandra doesn't create a cosine index on a
  vector of one dimension; ScyllaDB's vectors have at most 16,000 dimensions;
  Elasticsearch takes 4,096, OpenSearch 16,000 and Open Distro 10,000.
- On PostgreSQL with pgvector (PostgreSQL, TimescaleDB, AlloyDB, Cloud SQL,
  Aurora, EDB, Fujitsu, openGauss and the Greenplum family) HNSW and IVFFlat
  index up to 2,000 dimensions of `vector`, 4,000 of `halfvec` and 64,000 of
  `bit`. Past that, the index is left out with the reason and the migration
  goes on. YugabyteDB (`ybhnsw`) and CockroachDB (C-SPANN) keep it. Checked
  live through the migration: a libSQL `F32_BLOB(3072)` with its vector index
  arrives in PostgreSQL without the index (the plan says why) and in
  CockroachDB with it. (The
  conversion into the family keeps an index whatever its dimension, because it
  doesn't know which engine of the family it writes for; the table DDL then
  refuses it on an engine with a limit.)
- Targets whose designer has no indexes (Snowflake, Redshift, Materialize,
  Fabric, Aurora DSQL) or no vector indexes (MySQL Community, SQLite,
  DynamoDB, Timeplus, H2, Yellowbrick, Babelfish) get the table without the
  vector index, and the report says why.

### Test data

**Generate test data** offers a **Vector (random numbers)** generator wherever
the table has a vector column, and **Automatic** picks it. A column whose type
fixes the dimensions (`vector(1536)`, `VECTOR(3, FLOAT32)`) gets vectors of
that length; otherwise the generator has a dimensions setting (empty: the
column's dimensions, or 8). The values are random unit vectors; `int8` and
`uint8` columns get whole numbers in range. Sparse columns (pgvector
`sparsevec`, Oracle `SPARSE`) get sparse vectors in their engine's text. This
includes the vector fields of Redis, Valkey and Dragonfly search indexes. See
[`test-data.md`](test-data.md).

### Copy a subset

Vector cells go to the target as the engine's literal. Masking keeps a vector
a vector: a hash or made-up value is a random vector of the same dimension,
and noise moves each number. A subset that the driver had to cut short
(Valkey's 10,000-document KNN maximum, for example) says it is partial in the
plan and in the run report (checked live on an index of 10,050 documents with
no `NUMERIC` field). Copying a subset of a Redis, Valkey or Dragonfly
search index keeps its definition: its `PREFIX` and its vector index (a
definition with no prefix gets `PREFIX 1 <name>:` instead of covering every
hash). See [`data-subset.md`](data-subset.md).

### Notebooks (DataFrame SQL)

In DataFrame SQL a result's vector column is a DuckDB `FLOAT[n]` array
(`DOUBLE[n]` for 64-bit vectors), so `array_cosine_similarity` and its kin work
over it. Result cells use the grid. A write cell into a vector column doesn't
warn about text length, and its preview shows vector cells as their first
numbers and dimensions. A notebook file keeps only each column's name and
type, so after reopening it a result is recognised as a vector only if its
type name says so (`FLOAT[n]` does; a MySQL `VARBINARY` or a Redis hash field
shows as text). See [`notebooks.md`](notebooks.md).

### Export

JSON and JSON Lines write vector columns as arrays of numbers. CSV, XML and
SQL keep the whole text. Excel stops with a message when a vector doesn't fit
in a cell (32,767 characters) instead of cutting it. There is no Parquet
export.

### Database docs

The generated Markdown and HTML show a vector index's settings in a column of
the table's indexes, where the table has one. See
[`database-docs.md`](database-docs.md).

### AI assistant and MCP

- `describe_object`, `index_usage` and the database-wide index overview give
  each vector index's algorithm, distance (or "engine default") and build
  parameters, for example
  `[vector index: algorithm hnsw, metric cosine, ef_construction=64, m=16]`,
  so the assistant can write the similarity query with the operator or function
  the index serves. They show it for engines whose driver reports vector
  settings with the index usage.
- **Similarity reads.** A similarity search runs without approval where the
  server enforces the read as read-only and every function the query calls is
  a side-effect-free built-in. The functions of each engine are listed per
  variant (pgvector's and the variants that ship it, CockroachDB, CrateDB,
  RisingWave, MySQL 9, MariaDB, TiDB, SingleStore, OceanBase, Doris / VeloDB,
  StarRocks, Databend, GreptimeDB, Manticore, Oracle 23ai, SQL Server
  2025 / Azure SQL, libSQL, ClickHouse / Timeplus, DuckDB, BigQuery, Spanner,
  Snowflake, Databricks, Couchbase, Cosmos DB and SAP HANA).
- pgvector's operators (`<->`, `<#>`, `<=>`, `<+>`, `<~>`, `<%>`) aren't
  function calls, so they never need approval on their own. pgvector's
  functions belong to an extension, so before such a read skips the approval
  DBine asks the server whether every function of that name, in any schema,
  is pgvector's. If a user's function has the same name, the user approves the
  read.
- Functions that call a model or another service (`VECTOR_EMBEDDING`,
  `AI_GENERATE_EMBEDDINGS`, Databricks' `vector_search`) always ask.
- **Neo4j** reads run as one Cypher statement in the READ access mode the
  server enforces. A read that calls only Neo4j's own functions (Cypher 25's
  `vector()`, `vector_distance`, `vector_norm` and `vector_dimension_count`,
  and `vector.similarity.*`) or searches a vector or full-text index
  (`db.index.vector.queryNodes`…) runs without approval. APOC, GDS, any other
  procedure and `LOAD CSV` ask. A read-only Neo4j or Memgraph connection runs
  vector and full-text index searches too.
- The other engines whose reads aren't SQL (Cassandra, ScyllaDB, Memgraph,
  Redis, MongoDB) ask for every read.

See [`mcp.md`](mcp.md) and [`ai-assistant.md`](ai-assistant.md).

### Editor completion

Each engine's vector functions are offered in the editor: pgvector's operators
and functions, `VECTOR_DISTANCE` and friends in SQL Server and Oracle,
`COSINE_SIMILARITY` and `L2DISTANCE` in SAP HANA, Db2's `VECTOR_*` functions,
Painless's `cosineSimilarity` and the k-NN functions in Elasticsearch and
OpenSearch, Couchbase's `APPROX_VECTOR_DISTANCE`, MongoDB's `$vectorSearch`
and (8.3+) `$similarityCosine`, `$similarityDotProduct` and
`$similarityEuclidean`, and so on.

## Engines

"Live" means it ran against a real server or emulator (`dbine-test-*`
containers, or a local server). "Unit" means unit tests only, with the
behaviour taken from the vendor's documentation.

### PostgreSQL family

| Engine | Vector types | Vector indexes | Tested |
|---|---|---|---|
| PostgreSQL with pgvector (also Supabase, Neon, RDS and Aurora, Azure, Cloud SQL, EDB, Fujitsu) | `vector(n)`, `halfvec(n)`, `sparsevec(n)`; `bit(n)` for binary quantization | `hnsw` (`m`, `ef_construction`): L2, inner product, cosine and L1 on `vector`, `halfvec`, `sparsevec`; Hamming and Jaccard on `bit`. `ivfflat` (`lists`): L2, inner product, cosine on `vector` and `halfvec`; Hamming on `bit`. `diskann` where pgvectorscale or Azure's `pg_diskann` is installed (`vector` columns only) | Live on PostgreSQL 17; the managed services unit |
| TimescaleDB | as pgvector | as pgvector, plus `diskann` (`num_neighbors`, `search_list_size`, `max_alpha`, `storage_layout`) | Unit |
| AlloyDB | as pgvector | as pgvector, plus `scann` (cosine, L2, dot product) and `ivf` (`lists`, `quantizer`); neither indexes `sparsevec` or `bit` | Unit |
| YugabyteDB | `vector(n)` | `ybhnsw` (L2, cosine, inner product); an `hnsw` from PostgreSQL becomes it. No IVFFlat | Live |
| openGauss 7.0 (DataVec, built in) | as pgvector | `hnsw`, `ivfflat`, `diskann` | Live |
| Greenplum 7, Apache Cloudberry, Greengage | as pgvector, where the server has the extension | as pgvector | Unit (no image with pgvector) |
| KingbaseES | as pgvector | as pgvector | Unit (follows the vendor's announcements) |
| CockroachDB 25.2+ | `VECTOR(n)` | `CREATE VECTOR INDEX` (C-SPANN) with prefix columns; L2, cosine, inner product | Live in an earlier round; re-run blocked by disk, unit for the latest changes |
| CrateDB 5.5+ | `FLOAT_VECTOR(n)` | none to create: a `FLOAT_VECTOR` column is indexed (HNSW) by being declared | Live |
| RisingWave 2.6+ | `vector(n)` | `HNSW` (`m`, `ef_construction`) and `FLAT`; L2, cosine, inner product, L1; only on append-only tables | Live |

Particularities:

- Results show the dimension (`vector(3)`) and the whole JSON array, in the
  grid and also in exports, the results store and compares.
- The designer refuses a distance the algorithm doesn't have for the column's
  type, a type it doesn't index, and a parameter that isn't a number. The
  operator class follows the column's type (`vector_cosine_ops`,
  `halfvec_l2_ops`…).
- CockroachDB's `pg_am` reports its vector indexes as `prefix`, so DBine reads
  them from their definition (`USING cspann`). A `vector` or `halfvec` type or
  an HNSW index from PostgreSQL is written as `VECTOR(n)` / `CREATE VECTOR
  INDEX`, also when a schema sync compares a PostgreSQL table with a
  CockroachDB one. `sparsevec` and the distances C-SPANN lacks (L1, Hamming,
  Jaccard) are refused with the reason. Bulk load goes through `INSERT`s.
- CrateDB values arrive as `real[]` and are shown as JSON arrays; they are
  written as array literals because CrateDB doesn't cast text to a vector.
  `sparsevec` and a vector without a dimension are refused.
- CockroachDB and RisingWave index only their `VECTOR` / `vector(n)` columns:
  an index on a bit string or a sparse vector is refused with the reason.
- RisingWave can't describe a result with a vector column, so DBine marks the
  vector columns of queries over tables (single table, joins, FROM lists,
  aliases) from the tables' types. A computed or renamed vector column, and
  results streamed to an export, are not marked.
- Index size limits are per engine (see Schema conversion above). The
  designer, the table DDL and the sync refuse an index past its engine's limit
  and give the reason; for `vector` the reason says that an index on the
  column cast to `halfvec` reaches 4,000. DiskANN, AlloyDB's ScaNN and IVF,
  and KingbaseES are left to their servers.
- Reading or copying a table from openGauss works: openGauss ends its binary
  `COPY` without the trailer, and DBine accepts that.

### MySQL family

| Engine | Vector type | Vector index | Tested |
|---|---|---|---|
| MySQL 9.0+ | `VECTOR(n)` (float32) | None in MySQL Community (HeatWave only): a script or conversion writes a `--` note and the table is still created | Live (9.7.4) |
| MariaDB 11.7+ | `VECTOR(n)` (float32) | `CREATE VECTOR INDEX … M=… DISTANCE=euclidean\|cosine` (MHNSW). Needs a NOT NULL column. A sync rebuilds the index when its distance or `M` changes | Live (11.8.9) |
| TiDB 8.4+ | `VECTOR(n)`, `VECTOR` | HNSW on cosine or L2 distance; it lives in TiFlash, so the table needs a TiFlash replica (the script says so; without one the server answers error 8200) | Live (8.5.3) for the type; the index unit |
| OceanBase 4.3.3+ | `VECTOR(n)` | `hnsw`, `hnsw_sq`, `hnsw_bq`, `ivf_flat`, `ivf_sq8`, `ivf_pq`; L2, inner product, cosine | Unit |
| SingleStore 8.5+ | `VECTOR(n[, F32\|F16\|I8…])` | AUTO, FLAT, HNSW_FLAT, HNSW_PQ, IVF_FLAT, IVF_PQ, IVF_PQFS; dot product and Euclidean (no cosine) | Unit |
| Apache Doris 4.0+ / VeloDB | `ARRAY<FLOAT> NOT NULL` | ANN index `hnsw` or `ivf`; `l2_distance`, `inner_product`; the dimension is required | Unit |
| StarRocks 3.4+ | `ARRAY<FLOAT> NOT NULL` | `hnsw` or `ivfpq`; `l2_distance`, `cosine_similarity`; needs `enable_experimental_vector` on the FE (the script says so) | Unit |
| GreptimeDB | `VECTOR(n)` | The column's `VECTOR INDEX` in `CREATE TABLE` (`l2sq`, `cosine`, `ip`); a sync change is a note, because `ALTER` can't add one | Live (1.2.1) |
| Manticore Search | `float_vector` | The column's KNN settings (`knn_type='hnsw'`, `hnsw_similarity='L2'\|'IP'\|'COSINE'`), shown as an index named after the column | Live (29.9.0), transfer included |
| Databend | `VECTOR(n)` | `CREATE VECTOR INDEX … m=… ef_construct=… distance='cosine,l2,l1'` | Unit |

Particularities:

- MySQL 9 results are decoded from `MYSQL_TYPE_VECTOR`'s float32 bytes.
  MariaDB sends a binary string, so its vector columns are looked up in the
  catalog on a separate connection (the user's `FOUND_ROWS()` and warnings stay
  theirs); aliased `VEC_FromText(…) AS x`, derived tables and CTEs are marked
  too, a `UNION`'s columns only when they are `VEC_FromText` aliases.
- Literals: `STRING_TO_VECTOR('[…]')` (MySQL 9), `VEC_FromText('[…]')`
  (MariaDB), `'[…]'` (TiDB, OceanBase, SingleStore), array literals (Doris,
  StarRocks), `(…)` (Manticore, which edits with a partial `REPLACE … WHERE
  id=…`). GreptimeDB has no `UPDATE`: a row is replaced by inserting it again.
- Doris and StarRocks describe an existing AGGREGATE KEY table as such and
  don't write a vector index on it (the engine builds them only on DUPLICATE
  KEY, or PRIMARY KEY too in StarRocks). A conversion into them changes the key
  model to DUPLICATE KEY only when the vector index is actually created. A
  nullable column gets a note instead of the index.
- A vector index whose column can't be a vector on the target is dropped and
  reported, never written on JSON or an array.
- Manticore as a source: `float_vector` columns move to other engines as
  vectors with the dimension of their KNN settings, and their HNSW index moves
  too.

### SQL Server, Oracle, SAP HANA and Db2

| Engine | Vector type | Vector indexes | Tested |
|---|---|---|---|
| SQL Server 2025, Azure SQL | `vector(n)` (float32) and `vector(n, float16)` (preview) | DiskANN with cosine, Euclidean or dot distance (`CREATE VECTOR INDEX … WITH (METRIC = '…', TYPE = 'DiskANN')`) | Live on SQL Server (2019 for the TDS handshake; the vector features on 2025); Azure SQL unit |
| Oracle 23ai / 26ai | `VECTOR(n\|*, FLOAT32\|FLOAT64\|INT8\|BINARY\|*, DENSE\|SPARSE)` | HNSW (in memory) and IVF; cosine, Euclidean, dot, Manhattan, Hamming, Jaccard | Live (23.26.3) |
| SAP HANA Cloud | `REAL_VECTOR(n)`, `HALF_VECTOR(n)` | HNSW; cosine or Euclidean; `M`, `efConstruction`, `efSearch` | Unit |
| IBM Db2 LUW 12.1.2+ | `VECTOR(n, FLOAT32\|INT8)` | none (Db2 12.1.2 has no vector indexes) | Unit |

- **SQL Server:** DBine asks for the server's vector TDS type, so float32
  vector columns are marked with their dimension in every result and each cell
  is the whole JSON array. Table browsing (filtered and sorted too) also marks
  `float16` columns; in an ad-hoc query a `float16` vector arrives as JSON text
  and isn't marked, because the TDS feature carries float32 only. A vector is
  written as `N'[1,2,3]'`. Vectors can't be compared with `=`, so they are
  never part of a row key; sync by rows hashes them as their JSON text. Loads
  send the vector's binary form through `INSERT BULK` and refuse one of the
  wrong size. `CREATE VECTOR INDEX` needs `ALTER DATABASE SCOPED CONFIGURATION
  SET PREVIEW_FEATURES = ON`, and while the index exists the table takes no
  `INSERT`, `UPDATE` or `DELETE`; the script says both. The designer offers
  vectors on every version, and a server before 2025 refuses them at `CREATE`
  with a message that says which release has them. SQL Server can't disable or
  rebuild a vector index (Msg 42250), and **Disable index…** says so. Vector
  indexes are refused on vectors SQL Server keeps as JSON text (sparse, no
  dimension, too many dimensions).
- **Oracle:** results mark dimensions, format and sparseness. A `BINARY`
  vector is one 0 or 1 per dimension (the first dimension is the high bit of
  the first byte), like other engines' bit vectors. A `SPARSE` vector keeps
  Oracle's text `[dims,[indices],[values]]`. Index settings come from Data
  Pump's view (readable by the owner and by `SELECT_CATALOG_ROLE`), else from
  `VECSYS.VECTOR$INDEX` or `V$VECTOR_INDEX`; when none can be read, the script
  says so instead of silently using `COSINE`. The tables Oracle makes for each
  index are not shown, compared, synced or cloned. HNSW needs the vector
  memory pool (`VECTOR_MEMORY_SIZE` above 0). `BINARY` vectors accept only
  `HAMMING` and `JACCARD`; an index without a distance is created with
  `HAMMING`. Vector indexes can be made invisible and visible again; an
  unusable one can't be rebuilt (ORA-51926). Dense values and pgvector's
  `{i:v}/n` go into `SPARSE` columns as Oracle's sparse text, and sparse values
  into dense columns are spread out, since Oracle converts neither. A `SPARSE`
  vector loads into pgvector's `sparsevec`, and pgvector's binary vectors into
  `BINARY` columns. Values past 4,000 bytes are written as a
  `TO_CLOB('…') || '…'` chain.
- **SAP HANA Cloud:** DBine's HANA client predates the vector type. A server
  that sends vectors as binary has them decoded and marked, with the dimension
  from the catalog. One that sends the vector type itself makes table browsing
  read them as text (`TO_NVARCHAR`), still marked. In that case an ad-hoc query
  with a vector column fails with an explanation (read it with
  `TO_NVARCHAR(column)`), and a computed vector isn't marked. Edits write
  `TO_REAL_VECTOR('[…]')`, or `TO_HALF_VECTOR('[…]')` for a `HALF_VECTOR`
  column when the column types are known. Outside HANA Cloud the designer's
  error says vector indexes are HANA Cloud's.
- **Db2 LUW:** edits write `VECTOR('[…]', n, FLOAT32|INT8)` with the column's
  coordinate type when it is known. An edit from a caller that doesn't know the
  types writes `FLOAT32`, which Db2 refuses for an `INT8` column. Db2 for i and
  z/OS take vectors as a list of numbers with a warning.

### Elasticsearch, OpenSearch, Open Distro and Solr

| Engine | Vector types | Vector index | Tested |
|---|---|---|---|
| Elasticsearch 7.x, 8.x, 9.x | `dense_vector(n)`, `(n, byte)`, `(n, bit)`, `(n, bfloat16)`, `sparse_vector`, `rank_vectors(n)` (8.18+), `semantic_text` | The field's own ANN index (`index: true`): `similarity` cosine, `l2_norm`, `dot_product`, `max_inner_product`; `index_options` `hnsw` or `flat` with `int8`, `int4` or `bbq` quantization, `m`, `ef_construction`, `confidence_interval` | Live on 7.17.25, 8.15.3, 9.2.0 and 9.5.5 (with a trial license for `rank_vectors`) |
| OpenSearch 2.x, 3.x | `knn_vector(n)`, `(n, byte)`, `(n, binary)` | `method` `hnsw` with `space_type` l2, cosinesimil, innerproduct, l1, linf, hamming; `engine`, `m`, `ef_construction`; `mode`, `compression_level` or a trained `model_id` | Live on 2.17.1, 2.18.0, 2.19.0 and 3.10.0 |
| Open Distro 1.13 | `knn_vector(n)` (float only) | The index's k-NN settings (`index.knn`, space type l2, cosinesimil, l1, `m`, `ef_construction`), shared by all its vector fields | Live on 1.13.3 |
| Solr 9 | `knn_vector(n)`, `(n, BYTE)` (`DenseVectorField`) | The field type's HNSW graph: `similarityFunction` euclidean, cosine, dot_product, maximum_inner_product; `hnswMaxConnections`, `hnswBeamWidth` | Live on 9.10.1, standalone and SolrCloud |

- Vector columns are marked from the mapping in searches, `_mget` and
  `GET _doc` / `_source`; byte vectors given as hex decode to numbers; bit and
  binary vectors read one 0/1 per dimension.
- **Scripts that know the target's types.** Insert, edit and data-sync scripts
  built with the target's column types (from a result or the table's
  description) write each vector the way its field takes it, so the script
  also runs outside DBine (Kibana, curl): a bit vector (Elasticsearch
  `dense_vector(n, bit)`, OpenSearch `knn_vector(n, binary)`) given one 0/1 per
  dimension becomes its packed signed bytes (`n / 8` of them), and byte
  vectors are written as integers. A script built without the types writes
  bit vectors one 0/1 per dimension: DBine's console packs them by the
  target's mapping when it runs it, and anywhere else it needs the packed
  bytes.
- A vector index of an OpenSearch field that names no engine gets the one its
  vectors need: Lucene for bytes, Faiss with Hamming for binary, NMSLIB for L1
  and L∞.
- **Schema conversion** maps a vector index by the vector field its column
  becomes in the target. Bit vectors (Elasticsearch `bit`, OpenSearch
  `binary`) are searched only by Hamming: a source index by L2 becomes Hamming,
  which ranks bits the same, and one by cosine or inner product is left out.
  Hamming exists only for bit vectors, so on a float column (on Open Distro,
  or bits whose dimension isn't a multiple of 8) the index is left out.
  OpenSearch searches byte vectors with Lucene, which has no L1, so such an
  index is left out. Elasticsearch quantizes float vectors only, `int4` needs
  an even dimension and BBQ 64 dimensions or more; otherwise the quantization
  is left to the engine. A column that doesn't become a vector field (no
  dimension on OpenSearch, Open Distro or Solr, a sparse vector, more
  dimensions than the engine takes) gets no vector index, and the report says
  why. A float array that another engine searches with a vector index stating
  its dimension (ClickHouse, BigQuery, Doris…) arrives as a vector field with
  its index.
- **OpenSearch to Open Distro.** Both write `knn_vector`, but Open Distro
  takes less: byte and binary vectors become float vectors (a note says so)
  and a binary vector's Hamming index is left out; a vector of more than
  10,000 dimensions isn't a vector field there; an index keeps only HNSW with
  L2, cosine or L1 and `m` / `ef_construction` (OpenSearch's engine, `mode`
  and `compression_level` are dropped, and an index by inner product or L∞ is
  left out). Open Distro keeps the distance and HNSW parameters for the whole
  index, so when two vector fields are searched differently the second one's
  index is left out and the report names the first. OpenSearch's own index
  settings (`index.replication.type`, `index.remote_store.*`) are left out on
  Elasticsearch and Open Distro, which refuse them, and the report lists each
  one.
- **Sync:** a new field gets its vector index in the same update. Changing or
  dropping the vector index of an existing Elasticsearch or OpenSearch field
  needs a reindex, so sync only warns. Settings left empty in the designer are
  the engine's choice: sync takes what the server reads back as the same
  index. On Solr, dropping a field's vector index makes the field
  `indexed: false` (with a reindex warning) and leaves its field type, since
  other fields may share it; changing one replaces the field type or gives the
  field a new one.
- **Transfers** are JSON-array text; the load packs 0/1 bit vectors into
  bytes. Solr's load takes a number only if it is a float32 (a byte for
  `BYTE`) and the length is the dimension, else it stops: vectors kept as
  float64 text (an Elasticsearch `_source` with more digits, MongoDB, JSON
  files) stop the load at the first such value.
- **Elasticsearch 7.x** has `dense_vector` without an ANN index: a mapping that
  asks for one gets a warning naming its fields (7.17 takes it without
  effect, older 7.x refuse it). Similarity search there is `script_score`.
- **Elasticsearch 8.0–8.10** index vectors only when asked: DBine always writes
  the distance with `index: true`. HNSW parameters given without a
  quantization are written `int8_hnsw`; on 8.0–8.11 choose quantization
  `none`. `bbq_*`, `bbq_disk`, `rescore_vector` and `bfloat16` (9.x) are
  written as given, without a check of the server's version. On 9.x, BBQ HNSW
  indexes with `rescore_vector`, int8 `flat` indexes and `bfloat16` vectors
  (9.3+) are created from the designer and read back as written, and a schema
  sync against the designer's model finds nothing to do (checked live on
  9.5.5).
- **Elasticsearch 9.2+** leaves vectors out of `_source`. DBine's browse and
  transfers ask for them, and get them rebuilt from the index. A search,
  `GET _doc` or `_mget` typed in the console that doesn't ask, and doesn't
  filter `_source` itself, gets a message saying how
  (`"_source": {"exclude_vectors": false}` or
  `?_source_exclude_vectors=false`). A `bfloat16` vector under a cosine index
  is stored normalized with its length, so it comes back with bfloat16's
  error (`[1, 1, 0.5, -2]` reads `[1.0009766, 1.0009766, 0.5004883,
  -2.0019531]`); under an L2 index it comes back as written when its numbers
  fit bfloat16. Float vectors under cosine come back exact.
- `rank_vectors` (several vectors per document) has no ANN index and isn't a
  vector column: its cells are the JSON array of arrays. It is an Enterprise
  feature. `semantic_text` is text whose embeddings Elasticsearch computes with
  an inference endpoint; it isn't a vector column and has no vector index of
  DBine's.
- **OpenSearch** builds an ANN index for every `knn_vector` of a k-NN index,
  so the description lists one for a field the designer left without. A float
  vector index that names no engine gets the server's default engine: NMSLIB
  up to 2.17, Faiss from 2.18. L2 works on every version. Cosine too: on 2.18
  Faiss has no `cosinesimil` (it has it from 2.19), so DBine creates a float
  or byte cosine index that names no engine with Lucene there, and says so
  (also for a mapping typed in the console, the designer's default and
  conversions such as pgvector's `vector_cosine_ops`). L1 and L∞ get NMSLIB,
  the only engine with them; OpenSearch 3.0 and later don't create new NMSLIB
  indexes, so on 3.x DBine refuses such an index before sending it and says to
  choose Faiss or Lucene with L2, cosine or inner product. Checked live on
  2.17.1, 2.18.0, 2.19.0 and 3.10.0. IVF needs a trained model and isn't
  offered by the designer; a `model_id` index is read and written as it is.
- **Open Distro** has float vectors only and fixes the distance and HNSW
  parameters in the index's settings at creation.
- **Solr:** more than 1024 dimensions are taken from Solr 9.5; 9.4 needs
  `SchemaCodecFactory` in the collection's `solrconfig.xml`, and 9.0–9.3 can't.
  The designer suggests 1024 and the DDL notes it above that. On a standalone
  server cores share their configset's managed schema, so a vector field type
  added for one core is seen by the others on that configset. Scripts that
  know the types write `BYTE` vectors as integers and refuse a value Solr would
  change without a word: it truncates `1.5` to `1` and wraps `200` to `-56`.

### Redis, Valkey and Dragonfly

| Engine | Where vectors live | Vector index | Tested |
|---|---|---|---|
| Redis 8 and Redis Stack | Hash fields as packed blobs (`FLOAT32`, `FLOAT64`, `FLOAT16`, `BFLOAT16`, `INT8`, `UINT8`), arrays in JSON documents, Redis 8 vector sets (`vectorset`) | `FT.CREATE … VECTOR HNSW\|FLAT\|SVS-VAMANA`; `COSINE`, `L2`, `IP` | Live on Redis 8.10.3 |
| Valkey (valkey-search) | Hash fields (`FLOAT32`) and JSON arrays | `HNSW`, `FLAT`; `FLOAT32` only | Live on 9.1.3 |
| Dragonfly | Hash fields (all six types) and JSON arrays | `HNSW`, `FLAT`; database 0 only | Live on 2.0.0 |

- **Search indexes are objects.** They are listed in the **Search indexes**
  folder of the explorer (read without walking the keys; the folder shows
  before the first index exists, and its menu offers vector index templates),
  described (the document key `id` and each field, a vector field as
  `VECTOR(3, FLOAT32)`), scripted as their `FT.CREATE` and dropped with
  `FT.DROPINDEX` (the documents stay). The definition keeps the index-wide
  options (`LANGUAGE`, `SCORE`, `STOPWORDS`, valkey-search's `PUNCTUATION`…),
  so compare and sync make it again the same.
- **Schema compare and sync** on key databases work on search indexes
  (`FT.DROPINDEX` + `FT.CREATE`), not on keys. Each server reports some
  settings at their defaults (Redis 8 adds `M`, `EF_CONSTRUCTION`,
  `EF_RUNTIME`…), and DBine reads an index without those, so a Redis 8, a
  Valkey and a Dragonfly index with the same definition compare equal, also
  after a sync from one to another.
- **Results** show vectors as text (`[0.1,0.2,0.3]`), decoded by the type of
  the index that covers the key, and the column says it holds vectors: in
  `FT.SEARCH` (also under `RETURN … AS`), `FT.AGGREGATE … LOAD`, `HGET`,
  `HGETALL`, `HMGET`, `HSCAN`, and in `FT.HYBRID` (Redis 8.4+), which shows as
  a table of key, score and the `LOAD` fields. Vector sets show each element
  with its vector and attributes.
- **Designer:** vector types and, on an index row, algorithm, distance and
  parameters. `FLOAT16` and `BFLOAT16` need Redis Stack 7.4 or Redis 8, `INT8`
  and `UINT8` Redis 8.0, and `SVS-VAMANA` Redis 8.2 (`FLOAT32` or `FLOAT16`
  only); a refusal says which feature needs a newer server. An uncompressed
  `SVS-VAMANA` index is scripted without a `COMPRESSION` setting, since Redis
  refuses `COMPRESSION NO_COMPRESSION`. There are
  templates for a vector index over hashes, over JSON documents, and a vector
  set.
- **Editing:** a vector typed as `[0.1, 0.2, 0.3]` is written as its index's
  blob (the script says the type and values in a comment) or as a JSON array.
  One of another dimension than the index's is refused, because the server
  would keep it without indexing it. When the column types are known, the blob
  is the column's own type. New rows go where the grid is: a new document of a
  search index (under its prefix), a new element of a vector set (`VADD`), a
  new field of a hash.
- **Transfers:** a search index is read and loaded as its documents (the
  index's `FILTER` isn't applied when reading). A vector set can be a target:
  the element is its `element` column (or the first), the vector its `vector`
  column, and the other columns become its attributes; a new set is made
  without quantization, so the values copy whole. Copies to and from pgvector
  were run live with Redis 8.
- **Limits:** if two open connections have search indexes with the same name
  and vector types but a different key prefix or other fields, a script that
  only knows column types can't tell them apart and is refused: open the index
  again on the connection you are writing to. RediSearch before 2.8 doesn't
  report a vector field's settings: the field stays as `VECTOR` and its
  `FT.CREATE` needs them typed in. A vector set keeps its vectors normalized
  (`VEMB` gives them back within float rounding) and keeps the quantization
  DBine read from it: Redis refuses a mismatch with an error. Valkey and
  Dragonfly have no vector sets. Redis 8 and Dragonfly create search indexes
  only in database 0. Browsing a Valkey search index with no `NUMERIC` field
  asks for the nearest vectors, capped at 10,000 (valkey-search's maximum).
  Redis is not offered by the app's migration, which needs a schema-conversion
  dialect.

### Cassandra family

| Engine | Vector type | Vector index | Tested |
|---|---|---|---|
| Apache Cassandra 5 | `vector<float, n>` (other element types for storage) | SAI: cosine, Euclidean, dot product | Live on 5.0.9 |
| ScyllaDB 2025.4+ | `vector<float, n>` | `vector_index`: cosine, Euclidean, dot product; `m`, `ef_construction`, `search_beam_width`, `quantization` (f32, f16, bf16, i8, b1), `oversampling`, `rescoring`; filter columns and local indexes | Live on 2026.3.1 (index creation and options; ANN queries not run) |
| DataStax Astra DB | as Cassandra | as Cassandra; `source_model` is kept | Unit |
| Amazon Keyspaces | none | none | not applicable |

- Results and table data mark vector columns with their dimension and show
  vectors as text; describe, compare, sync and the DDL keep
  `vector<float, n>`. The designer offers `vector<float, 3>` and, on an index
  row, the distance (and on ScyllaDB the graph settings, quantization and
  filter columns). There are templates for a vector index on both.
- Similarity search is `ORDER BY v ANN OF […] LIMIT k` (Cassandra) and the
  `similarity_*` functions. ScyllaDB's ANN needs its separate Vector Store
  service.
- **ScyllaDB creates vector indexes only in keyspaces that use tablets**, and a
  keyspace can't switch to tablets after it is created. The conversion says so
  for each vector index, and a refused index explains it: create the table in a
  new keyspace with tablets (`NetworkTopologyStrategy`).
- Cassandra's SAI graph settings (`maximum_node_connections`,
  `construction_beam_width`) need the server option
  `cassandra.sai.vector.allow_custom_parameters`: DBine keeps them for an index
  read from such a server, doesn't offer them, and leaves them out (saying so)
  when converting from another engine.
- Conversion checks the column a vector index searches: a vector without a
  dimension becomes a list, a sparse one becomes text, and neither keeps its
  index; an indexed `vector<double, n>` becomes `vector<float, n>` (CQL indexes
  only float vectors). Keyspaces has no vectors: a vector becomes `list<…>`
  and its index is left out.
- A `text` column keeps text that looks like JSON as text, and receives
  another engine's JSON value as its JSON text.

### MongoDB and document engines

| Engine | Vector support | Tested |
|---|---|---|
| MongoDB (Atlas Vector Search; Community and Enterprise 8.2+ with mongot) | Arrays of numbers and BinData subtype 9 vectors (float32, int8, packed bits). Types `vector(N)`, `vector(N, float32)`, `vector(N, int8)`, `vector(N, bit)`. `vectorSearch` indexes: `hnsw`; cosine, Euclidean, dot product; `quantization` (`scalar`, `binary`), `hnswOptions` (`maxEdges`, `numEdgeCandidates`), filter fields. Also Cosmos DB for MongoDB vCore's `cosmosSearch` (`vector-hnsw`, `vector-ivf`, `vector-diskann`) | Live on Atlas Local; vCore unit |
| Amazon DocumentDB | `vector` indexes: HNSW (`m`, `efConstruction`), IVFFlat (`lists`); Euclidean, cosine, dot product; one field, no filter fields | Unit |
| FerretDB 2 | `cosmosSearch` indexes: `vector-hnsw` (`m`, `efConstruction`, `compression: half`), `vector-ivf` (`numLists`); COS, L2, IP; filter fields. `vector-diskann` isn't available on 2.7 | Live |
| Azure Cosmos DB for NoSQL | Fields of the container's vector embedding policy: `vector(N)`, `vector(N, int8)`, `vector(N, uint8)`. `vectorIndexes` of type `diskANN`, `quantizedFlat`, `flat`; cosine, Euclidean, dot product | Emulator for `diskANN`; the rest unit |
| Couchbase 8.0 | GSI vector indexes (hyperscale `CREATE VECTOR INDEX`, composite `CREATE INDEX … (cat, emb VECTOR)`) with `COSINE`, `L2`, `L2_SQUARED`, `DOT`; Search vector indexes (7.6+) through the Search service's REST API. Types `vector(N)` and `vector(N, base64)` | Live on 8.0 |

- **MongoDB:** a field is a vector when it holds BinData vectors or a vector
  index declares it (DBine looks up the collection's indexes, cached for 30 s).
  A field whose vectors are all BinData of one element is typed with it
  (`vector(N, int8)`); `vector(N)` is an array, or a field holding both forms.
  Atlas Vector Search indexes are listed from `$listSearchIndexes` with their
  status (READY, BUILDING, PENDING, FAILED), created with
  `createSearchIndex`, changed in place with `updateSearchIndex`, dropped with
  `dropSearchIndex`, and renaming a field updates them. A definition DBine
  can't say whole is kept, and changes from the designer or a compare are
  written over it. Atlas Search indexes with `knnVector` fields are listed,
  compared and copied too. A vector index on a `vector(N, bit)` field uses the
  Euclidean distance, the only one Atlas searches bit vectors by. The shell
  has `createSearchIndex`, `getSearchIndexes`, `updateSearchIndex`,
  `dropSearchIndex`; the editor completes `$vectorSearch` and, from 8.3, the
  `$similarity*` operators.
- **Editing in MongoDB:** an edit keeps the stored form. When the column's
  type is known, each vector is written as BinData of that element for
  `vector(N, float32|int8|bit)` and as an array for `vector(N)`. Without types
  it follows what recent results of the collection showed (30 minutes, per
  collection name, process-wide); values are never changed, but two
  connections with the same collection name and different forms could swap the
  form. DocumentDB always gets arrays. Copies keep each document's form.
- **Search indexes build asynchronously** in mongot, which pauses builds when
  its disk is over 85% full. MongoDB 7 and other servers without mongot have
  none, and creating one fails with the server's message. When the search
  indexes can't be read for another reason (permission, mongot starting), the
  schema says so. Vector search indexes can't be hidden.
- **Cosmos DB for NoSQL:** the policy is written when the container is
  created (it can't change later, and sync says so). Vector paths stay out of
  the range index. A `flat` index over more than 505 dimensions is created as
  `quantizedFlat`. When the server doesn't return the embedding policy (the
  emulator), DBine takes the distance as cosine, the element as float32 and
  the dimension from the items it reads, and the schema says so; a field
  without items has no dimension and must get one before the container can be
  copied.
- **Couchbase:** hyperscale indexes are read with their dimension, similarity
  and description (`IVF<lists>,<quantization>`), scripted, compared and
  rebuilt; the designer makes them (hyperscale by default, composite by kind)
  with filter fields. On a collection without documents a vector index can't
  train: DBine creates it deferred and says to run `BUILD INDEX` once the
  documents are in. Search vector indexes are made through the Search
  service's REST API (`CREATE SEARCH INDEX … WITH {definition}`); the designer
  doesn't create them. Fields that keep vectors as base64 text of
  little-endian float32 (`vector_base64`, or a GSI index with
  `DECODE_VECTOR(field, false)`) are typed `vector(N, base64)`: results and
  copies show numbers, edits that know the types write base64, and into other
  engines it moves as float32. Big-endian base64 is shown as the index's
  expression, not as a vector field. Queries with `APPROX_VECTOR_DISTANCE`,
  `VECTOR_DISTANCE`, `SEARCH()` and the other vector functions run from DBine.
- **Transfers** read vectors as JSON arrays; loads from other engines write
  arrays of numbers (into DocumentDB and FerretDB, `int8` and bit vectors
  become arrays of numbers, and FerretDB leaves BinData vectors out of its
  searches). pgvector's binary vectors arrive in a `vector(n, bit)` field as
  arrays of 0 and 1, which its Euclidean `vectorSearch` index finds, not as
  packed BinData. A migration into a new collection knows its vector fields
  from the converted structure, since MongoDB describes an empty collection by
  no field. MongoDB to MongoDB keeps each value's form; the direct copy keeps
  the raw BSON.
- CouchDB (Mango and Nouveau are full-text), DynamoDB (vector search goes
  through OpenSearch zero-ETL or S3 Vectors, separate services) and etcd have
  no vectors.

### Embedded and analytical engines

| Engine | Vector types | Vector index | Tested |
|---|---|---|---|
| libSQL / Turso | `F32_BLOB(n)` (`FLOAT32(n)`), `F64_BLOB`, `F16_BLOB`, `FB16_BLOB`, `F8_BLOB`, `F1BIT_BLOB` | `libsql_vector_idx` (DiskANN): cosine (on `F1BIT_BLOB`, the Hamming distance), L2; `max_neighbors`, `compress_neighbors`, `insert_l`, `search_l`, `alpha`; a partial index keeps its `WHERE` | Live on a local `sqld` 0.24.33; Turso cloud not tested |
| SQLite | A file libSQL wrote: its vector columns are decoded, edited and moved like libSQL's | SQLite can't create or use libSQL's index. A libSQL file's index is listed (as DiskANN) and compared; DDL, sync, clone and rename leave it out and say why | Live |
| DuckDB | `FLOAT[n]`, `DOUBLE[n]` | `vss` `HNSW` (`l2sq`, `cosine`, `ip`; `M`, `M0`, `ef_construction`, `ef_search`) on `FLOAT[n]` columns only | Live |
| ClickHouse | `Array(Float32)`, `Array(BFloat16)` | `vector_similarity('hnsw', 'cosineDistance'\|'L2Distance'\|'dotProduct', dimension[, quantization, m, ef_construction])` | Live on 25.3 LTS and 26.9; older and other versions unit |
| Timeplus Proton | `array(float32)` | none: Proton has no vector index | Unit; live edits and transfers not run |

- **libSQL:** DBine decodes the blobs itself, so results and transfers show
  whole vectors. 8-bit values come out as libSQL scales them, and 1-bit values
  as 0 and 1. Each vector is written in its column's format (`vector32('[…]')`,
  `vector64('[…]')`…). An index needs a vector column with a dimension; the
  designer refuses, with the reason, an index on a column without one, L2 on
  1-bit vectors (libSQL accepts it but ranks wrong) and 1-bit compression with
  L2. An L2 index on 1-bit vectors already on the server is scripted as it is
  by clone, script export, same-engine migration and compare, with a warning.
  On 1-bit vectors libSQL's cosine distance (`vector_distance_cos`, and the
  index's ranking) counts the bits that differ: it is the Hamming distance, so
  pgvector's binary vectors migrate into `F1BIT_BLOB(n)` with their Hamming
  index as a cosine one (checked live on `sqld` 0.24.33).
  The indexes' shadow tables (`<index>_shadow`, `libsql_vector_meta_shadow`)
  belong to libSQL and are hidden. The **Indexes** tab shows the shadow tables'
  size.
- **SQLite:** sqlite-vec (`vec0` tables, `vec_*` functions) needs a loadable
  extension, and DBine never loads SQLite extensions: `vec0` tables show as
  virtual tables that can't be read. Their shadow tables are hidden from the
  explorer, schema, compare, clone and migrations.
- **DuckDB:** the vss index DDL runs `INSTALL vss; LOAD vss;` and turns on
  `hnsw_enable_experimental_persistence` first. Opening a file with HNSW
  indexes never downloads anything: a writable connection loads vss if it is
  installed on the computer, a read-only one loads nothing (reads work without
  it), and without vss a write to such a table fails with an error that says
  how to install it. An HNSW index's distance and settings are read from its
  statement. DuckDB 1.5 lists the statement without its `WITH (…)` clause, so
  on a 1.5 file the distance comes from vss's index report
  (`pragma_hnsw_index_info()`, when vss is loaded), and `M` and
  `ef_construction`, which that report doesn't have, are lost: the index is
  read without them, so a script or a clone creates it with vss's defaults.
  In the CSV / Parquet / JSON
  folder connection, vector
  columns are marked in results only: those connections have no tables of
  their own, and Parquet has no fixed-size arrays. An index on a `DOUBLE[n]`,
  sparse or dimensionless vector is left out in a conversion, with the reason.
- **ClickHouse:** a ClickHouse array has no fixed length, so the dimension
  belongs to the index. DBine writes the form of ClickHouse 25.8 and later;
  24.8–25.7 indexes (experimental, no dimension) are read and written back in
  their own form after `SET allow_experimental_vector_similarity_index = 1`.
  `dotProduct` needs 26.4 and `BFloat16` 25.1; an older server's error says
  which version is needed. On 26.4 to 26.9 a `dotProduct` index search ranks
  wrong unless rescoring is on (a ClickHouse bug, fixed in 26.10).
  `Array(Float64)` isn't marked as a vector in arbitrary results, because a
  result has no index context; browsing its table shows it as a vector when a
  vector index searches it, and its index still moves.

### Cloud warehouses and graph engines

| Engine | Vector type | Vector index | Tested |
|---|---|---|---|
| BigQuery | `ARRAY<FLOAT64>` | `CREATE VECTOR INDEX` `IVF` or `TREE_AH`; cosine, Euclidean, dot product; filter columns as `STORING` | Emulator for types; indexes unit (the emulator has none) |
| Cloud Spanner | `ARRAY<FLOAT32\|FLOAT64>(vector_length=>n)` | `CREATE VECTOR INDEX` with `distance_type`, `tree_depth`, `num_leaves`, `num_branches`, `leaf_scatter_factor`; filter columns as `STORING` | Emulator |
| Snowflake | `VECTOR(FLOAT\|INT, n)`, up to 4096 dimensions | none: the similarity functions scan the table | Unit (no account) |
| Neo4j | Float-list properties; Neo4j 2025.10+ native `VECTOR<FLOAT32>(n)` (Bolt 6.0) | `vector.dimensions`, `vector.similarity_function` (cosine, Euclidean), `vector.hnsw.m`, `vector.hnsw.ef_construction`, `vector.quantization.enabled` | Live on 5.26 and 2025.10.1 Enterprise (reads; write tests blocked by disk in round 1) |
| Memgraph | Float lists | `CREATE VECTOR INDEX … WITH CONFIG {dimension, metric, capacity, scalar_kind, resize_coefficient}` | Live |
| Arrow Flight SQL | Fixed-size float lists, named `FLOAT[n]` / `DOUBLE[n]` | Those of the engine behind it (over DuckDB with `vss`, HNSW is shown) | Live |

- **BigQuery:** arrays can't hold or be NULL; an empty float array, how
  BigQuery stores a NULL array, reads as NULL. An array holding NaN or ±∞
  keeps its plain form. BigQuery has no other secondary indexes: a plain index
  row is commented out in the DDL and reported as a warning. Another engine's
  vector becomes `ARRAY<FLOAT64>` (no dimension is enforced), and its index
  becomes IVF (`TREE_AH` from ScaNN); an index on a sparse vector is left out.
  As a source, `ARRAY<FLOAT64>` stays a list of floats.
- **Spanner:** results carry no `vector_length`, so result columns are vectors
  of unknown dimension. Scripts write `ARRAY<FLOAT32>[…]`, or
  `ARRAY<FLOAT64>[…]` when the script knows the column's type, because Spanner
  doesn't coerce between the two. The `IS NOT NULL` filter Spanner requires on
  nullable columns is added, and settings Spanner refuses (`tree_depth` other
  than 2 or 3, sizes of 0, `leaf_scatter_factor` above 32) are reported before
  anything runs. A vector index needs a column with `vector_length`; with no
  known dimension, or on a sparse vector, it is left out and reported.
- **Snowflake:** scripts cast `[…]::VECTOR(FLOAT, n)` (and
  `VECTOR(INT, n)` when the type is known). An `INT` vector, or one whose
  element type is unknown, becomes `VECTOR(FLOAT, n)` with a warning. A vector
  index from another engine is left out with the reason; a graph engine's index
  still gives its float list the type `VECTOR(FLOAT, n)`.
- **Neo4j:** results mark a column as a vector when all of its lists have the
  same length; table browsing shows whole nodes, so a vector inside a node isn't
  marked in the grid's column. With Neo4j 2025.10+, DBine negotiates Bolt 6.0
  and reads the native `VECTOR` type; edits and copies write `vector(…)`
  (those statements run in Cypher 25). Older protocol versions return a
  placeholder instead of the vector. Vectors from other engines load as
  `LIST<FLOAT>`, which vector indexes take. DBine checks the server's limits
  before running (dimensions 1–4096, M 1–512, ef_construction 1–3200). The
  `vector.similarity.*`, `db.index.vector.queryNodes` and 2025.10
  (`vector()`, `vector_distance`, `vector_norm`, `vector_dimension_count`)
  functions are offered. A conversion into Neo4j leaves out an index on a
  sparse vector or with a similarity other than cosine or Euclidean. Reads
  from MCP clients and the assistant run in the READ access mode the server
  enforces (see [AI assistant and MCP](#ai-assistant-and-mcp)).
- **Memgraph:** the vector index needs a dimension, from the index or from the
  column (pgvector `vector(3)`, Spanner `vector_length`, Neo4j
  `VECTOR<FLOAT32>(3)`); when nothing states it, the index is left out with the
  reason. Memgraph doesn't list `capacity` or `resize_coefficient` back, so
  sync doesn't count them as changes.

### Engines without vectors

These have no vector column type, or no vector type or index that DBine
drives. A vector converted into them becomes a list of numbers or JSON text
with a warning, and a vector index is left out with the reason. The designer
offers no vector type or index.

- **PostgreSQL family:** Amazon Redshift, Materialize, H2, Denodo, Yellowbrick,
  Amazon Aurora DSQL (which loads no extensions). Babelfish's TDS endpoint
  belongs to the SQL Server family; its PostgreSQL endpoint is Aurora
  PostgreSQL, where pgvector works.
- **MySQL family:** Amazon Aurora MySQL (no vector type in its MySQL 8). Cloud
  SQL for MySQL and MySQL HeatWave's vector store are not covered.
- **SQL Server and ODBC:** SQL Server 2022 and earlier (and
  `sqlserver_legacy`), Fabric Warehouse, Babelfish, Firebird, Db2 for i and
  z/OS, Informix, Teradata, Vertica, Netezza, Exasol, SAP ASE, SQL Anywhere,
  Spark and Hive over ODBC, Access, Altibase, CUBRID and generic ODBC.
- **Cloud and analytics:**
  - **Databricks and Azure Databricks:** vector search indexes are a separate
    service (Mosaic AI Vector Search), not SQL DDL. `vector_search()` runs as
    an ordinary query, always after approval from the assistant, and arrays
    show as arrays.
  - **Trino, Starburst and Athena:** no vector type or index; array similarity
    functions run as ordinary queries.
  - **Presto:** no vector type or index is documented. Its function list has
    `create_vector_index`, which DBine doesn't drive.
  - **Spark, Hive, Cloudera, Impala and Kyuubi:** no native vector column
    type.
  - **Dremio:** no vector type, index or similarity functions.
  - **Drill:** no vector type; its `cosine_distance` is a string-similarity
    function.
  - **InfluxDB 1 and 2, TDengine, ksqlDB and Phoenix / Avatica:** no vector
    type. InfluxDB 3 has no vector type either, but its `array_distance`
    works on lists as an ordinary query.
  - **IoTDB / TimechoDB:** its `VECTOR` is an aligned time series, not
    embeddings.
  - **kdb+:** q vectors are lists, not embeddings; KDB.AI is a separate
    product.
  - **OrientDB:** OrientDB 3.2 has no vector index.
  - **Power BI:** DAX models, no vectors.
  - **Amazon Neptune:** no user-defined indexes, so a vector converted into it
    loses its dimension and arrives as a float list.
  Snowflake, BigQuery, Timeplus and Spanner have vector columns but a
  different story for indexes: see their rows above.
- **Others:** Amazon Keyspaces, CouchDB, DynamoDB, etcd, and Valkey and
  Dragonfly's vector sets.

## Limits

- **Display:** the grid shows the first numbers of a vector; the cell viewer
  and **Copy cell** have the whole vector. Sorting by a vector column covers
  the loaded rows only.
- **`bit(n)` in PostgreSQL** reads and writes as PostgreSQL's bit string
  (`1011`), not as a vector, even when a `bit_hamming_ops` or `bit_jaccard_ops`
  index covers it: plain bit strings exist without pgvector, so DBine can't
  tell a binary vector from one. Only the index is a vector index. A transfer
  into another engine's vector column still gets its bits as a binary vector.
- **Edit and data-sync scripts in pgvector** write a dense vector as `'[…]'`,
  whatever the target column's type, unless the script knows the target's
  types. A transfer into a `sparsevec` column writes the `{i:v}/n` notation,
  and into a `bit(n)` column a 0/1 vector's bit string.
- **Connections set to the simple query protocol only** (PostgreSQL family)
  don't mark vector columns, because that protocol carries no column types.
  The cells are still whole JSON arrays.
- **Scripts that don't know the column types** write the engine's default
  vector form: `TO_REAL_VECTOR` for a SAP HANA `HALF_VECTOR`, `FLOAT32` for a
  Db2 `INT8` column, `ARRAY<FLOAT32>` for a Spanner `FLOAT64` vector, arrays
  for Couchbase base64 fields, a `VECTOR(INT, n)` cast for a Snowflake `INT`
  column (all fixed when the app passes the types of the result or the
  describe). In Elasticsearch and OpenSearch such a script writes bit vectors
  one 0/1 per dimension, and in Solr byte vectors as given: DBine's console
  packs bit vectors by the target's mapping when it runs the script, and run
  anywhere else it needs the packed bytes. Scripts DBine builds from a result
  or a table carry the types.
- **OpenSearch versions:** the designer and conversions still offer and keep
  L1 and L∞ for OpenSearch 3.x, because neither knows the server's version;
  the session refuses them on 3.x with what to choose instead.
- **Elasticsearch 9.2+** gives a `bfloat16` vector under a cosine index back
  normalized, with bfloat16's error.
- **DuckDB 1.5 files** lose an HNSW index's `M` and `ef_construction` (see
  [DuckDB](#embedded-and-analytical-engines)).
- **MongoDB** gets other engines' binary vectors as arrays of 0 and 1, not
  packed BinData.
- **Vector index changes that can't be applied in place:** Elasticsearch and
  OpenSearch need a reindex (sync only warns), GreptimeDB can't add one with
  `ALTER`, SQL Server can't disable or rebuild one, and Cosmos DB for NoSQL
  fixes the vector policy when the container is created.
- **Dimensions and distances are the engine's:** an index past the engine's
  dimension limit, with a distance the algorithm lacks, or on a type it doesn't
  index is refused with the reason, or left out of a conversion and reported.
- **Float arrays that are not vectors:** `Array(Float64)` in ClickHouse
  (outside browsing), BigQuery `ARRAY<FLOAT64>`, Doris and StarRocks
  `ARRAY<FLOAT>`, Spanner without `vector_length` and Neo4j lists are arrays,
  not typed vectors: they are shown as vectors where the engine says they hold
  embeddings, but the engine doesn't enforce a dimension, and converting from
  them keeps a float list.
- **Excel export** stops with a message for a vector longer than a cell
  (32,767 characters). There is no Parquet export.
- **Notebooks** keep only a column's name and type in the file.
- **sqlite-vec** is not read: DBine never loads SQLite extensions.
- **Moving vectors between engines** needs each side's driver to know the
  vector type. SQL Server's vector TDS feature carries float32 only; SAP HANA
  on a server that sends the vector type itself fails ad-hoc queries with a
  vector column (use `TO_NVARCHAR(column)`).
- **A vector set** (Redis 8) keeps normalized vectors.
- **Not tested against a server:** see the "Tested" column above and
  [`engine-support.md`](engine-support.md#vectors). SAP HANA Cloud, Db2 12.1.2,
  Snowflake, SingleStore, Databend, OceanBase, Doris, StarRocks, AlloyDB,
  DocumentDB, Astra DB, Keyspaces, Azure SQL and Cosmos DB for MongoDB vCore
  are covered by unit tests only. So are Elasticsearch 8.0 to 8.11, OpenSearch
  1.x and 2.0 to 2.16, OpenSearch IVF, Faiss PQ, `mode` and
  `compression_level`, Solr 9.4 above 1024 dimensions, and Oracle HNSW over
  binary vectors (the test instance has no vector memory pool, so its IVF index
  was searched instead).

## Contract

- `Capabilities::vectors`: the engine stores vectors and the driver handles
  them. The UI shows vector features only where it is set.
- `ResultColumn::vector` (`VectorMeta`: `dims`, `element`, `sparse`) marks a
  result or describe column. Elements: `f32`, `f64`, `f16`, `bf16`, `i8`,
  `u8`, `i32`, `bit`.
- A vector cell is text with a JSON array of numbers, whole. Sparse vectors
  keep the engine's text. In a transfer it is `Cell::Text` or `Json`. In an
  edit or insert script it is a JSON array (`vector::rows_for_scripts`) and the
  driver writes its engine's literal.
- `Driver::insert_script_typed`, `update_script_typed` and
  `delete_script_typed` take the target's columns with their types, for
  drivers whose literal depends on the column's element or stored form. The
  defaults ignore the types.
- `IndexDef::vector` and `IndexUsage::vector` carry a `VectorIndex`
  (`algorithm`, `metric`, `params`), with the standard names of
  `vector::metric` (`cosine`, `l2`, `ip`, `l1`, `hamming`, `jaccard`) where
  one fits. `DesignerSpec::vector_indexes` (`VectorIndexSpec`) says what the
  designer offers.
- `Driver::supports_schema_sync_for(kind)` (and the plugin manifest's
  `schema_sync_kinds`) says which object kinds schema sync can apply; Redis,
  Valkey and Dragonfly answer for their search indexes.
- The shared helpers (`crates/dbine-driver/src/vector.rs`) parse and write
  vectors and decode the binary forms (float32, float64, float16, bfloat16,
  int8, uint8, bits).
- Schema conversion reads each driver's designer to know what the target can
  create (`crates/dbine-schema`).
