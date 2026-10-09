# Optimize query

Looks for versions of a query that might run faster, **compares** them with
the original in the database (times, rows and result) and suggests indexes
based on the execution plan. **None of this changes the database**: the
scripts and rewrites only reach an editor, and you run them.

## Where it is

The **Optimize** button in the query editor toolbar. It takes the
**selection** or, with no selection, the statement where the cursor is (if
there's none, it warns **Put the cursor in a query or select it**). It opens
in its own tab, **Optimize**, and **Analyze again** repeats the analysis.

If the query **modifies data** (`UPDATE`, `DELETE`…), it's never run:
**Compare** compares only their estimated plans.

## What it shows

1. **Original query**, with its plan and its estimated cost when the engine
   gives execution plans.
2. **Alternatives**: the rewrites from the rules, from the AI and your own.
3. **Observations**: things to know that aren't a safe rewrite.
4. **Suggested indexes** and **Plan warnings**.

What couldn't be read (the tables' structure, the plan) is listed under
**Could not be read**, with the reason.

## The rules

A rule only offers a rewrite when it can ensure it's **equivalent**. If it
can't prove it from the query text and the tables' structure, it leaves it as
is or reports it as an observation. The text you wrote is preserved: the
rewrite changes only what's necessary.

| Rule | What it does | When it applies |
|---|---|---|
| `in_to_exists` | `x IN (subquery)` becomes `EXISTS` with the condition inside. | Simple subquery, single-column and without aggregates or further subqueries, and `x` is still the outer column. |
| `not_in_to_not_exists` | `NOT IN (subquery)` becomes `NOT EXISTS`. | Only if **both columns** are `NOT NULL` according to the structure. |
| `exists_to_in` | A correlated `EXISTS` becomes `IN`. | Subquery over one table, without aggregates, correlated by column equalities. |
| `scalar_to_join` | Scalar subqueries in the `SELECT` (`COUNT`, `SUM`, `MIN`, `MAX`, `AVG`) become a `LEFT JOIN` with a grouped table. A `COUNT` with no rows still gives 0. | Without `GROUP BY`, `HAVING` or aggregates in the block, and without comma-joined tables. |
| `or_to_union` | `a = 1 OR b = 2` becomes `UNION ALL`, where each branch excludes the rows of the previous ones. | `OR` of 2 to 4 conditions on **different columns**, single-block query without `DISTINCT`, `TOP`/`LIMIT`, `GROUP BY`, `ORDER BY`, aggregates or functions that give a different value on each call (`RAND`, `NEWID`, `NOW`…). |
| `redundant_distinct` | Removes an unnecessary `DISTINCT`. | The list includes a key from each table (the primary key or an unfiltered unique index on `NOT NULL` columns), or its `*`; all tables known; without `GROUP BY` or `TOP`. Needs the structure. |
| `function_to_range` | `YEAR(col) = 2024` (or `EXTRACT`, `DATE(col)`, `CAST(col AS DATE)`, `col::date`, `TRUNC(col)`) becomes a range `col >= … AND col < …`. | PostgreSQL, MySQL, SQL Server and Oracle; the column is a date, an index **starts** with it and the value is a literal date. |
| `count_to_exists` | `(SELECT COUNT(*) …) > 0` (and the equivalent forms with `= 0`, `>= 1`, `<> 0`…) becomes `EXISTS` / `NOT EXISTS`. | Simple subquery, without `DISTINCT`, and nothing else bound to the comparison. |
| `mongo_where` | `$where` becomes query operators. | MongoDB only, and only if the expression is field-to-constant comparisons joined by `&&`. The candidate asks you to **compare before using it**, because with fields that are arrays or mix types the result may change. |

### Observations

They don't rewrite, but they're worth knowing:

- **`SELECT *`**: tells how many columns the table has, with **Open with the
  columns** to open a version that lists them (so you keep the ones you use).
  Only with one table in the `FROM` and a known structure.
- **`NOT IN` with a column that accepts `NULL`**: if the subquery returns a
  `NULL`, no row comes out.
- **Comparing text with a number** (`text_column = 5`): the engine converts
  the column on each row and doesn't use its index.
- **Comparing a `char`, `varchar` or `text` column with an `N'…'` string**
  (SQL Server): the engine converts the column.

According to the code, the rules **don't apply** to the query of Cosmos DB,
PartiQL (DynamoDB), ksqlDB, IoTDB, TDengine, InfluxQL, OrientDB or N1QL: the
analysis answers **The rewrite rules don't apply to this engine's language**
and the AI and your own alternatives remain. In non-SQL engines, only
`mongo_where` exists.

## Suggested indexes

They come from the query's **estimated plan**, without running it:

- **The engine suggests it** (SQL Server, with its missing-index warning): the
  `CREATE INDEX` is shown with the **estimated impact** in %.
- **Full scan** of a table that the query filters or joins by columns no
  index starts with: an index on those columns is suggested. In MongoDB, a
  `COLLSCAN` generates the index of the filter's fields.
- **Full scan with no known columns**: in engines whose language isn't
  analyzed (N1QL, Cosmos DB, PartiQL), it only warns that the table is
  scanned in full.

The script is in the engine's language and **is only opened in a query**
(**Open in a query**): you review it and run it. An index speeds up reads and
slows down writes. Engines without execution plans say **This engine doesn't
show execution plans, so there are no index suggestions**. **Plan warnings**
gathers what the plan reports (spills to disk, conversions, scans…).

## AI alternatives

**Ask the AI for alternatives** uses the configured assistant (see
[`ai-assistant.md`](ai-assistant.md)). If there's none, it offers to open it.

- **Only** these are sent: the query, the **structure** of the tables it names
  (columns, keys and indexes) and a summary of the plan. **Rows are never
  sent.** The screen says what was sent and to which provider.
- It proposes up to **3** equivalent rewrites, each with its title and its
  explanation. It doesn't propose indexes or structure changes.
- Each one is marked **AI** and with the warning that **it hasn't been proven
  equivalent**: compare it before using it. The AI never runs anything.
- If it sees no improvements, it says so (**The AI found no improvements for
  this query**).

With a local model less structure is sent (up to 12,000 characters) than with
a cloud one (up to 60,000).

You can also **Add an alternative** of your own: an equivalent version you
write yourself, which is compared like the others (**Yours**).

## Compare

**Compare** runs the original and each alternative in its own **read-only
session** and compares:

- **Runs:** each version runs **once to warm up** and then N times (1 to 20;
  3 by default). The minimum and average times are shown.
- **Max. rows** (100,000 by default): how many rows are read and compared
  from each version.
- **Result:** as they arrive, the rows are **summarized into a fingerprint**
  (never stored). The fingerprints of the original and of each version are
  compared:
  - **Same result**: same rows and same content. If the original doesn't
    sort, row order doesn't count; if it has an `ORDER BY` (or a `sort` in
    MongoDB) at the top level, it does.
  - **NOT equivalent**: the result differs. The version is marked.
  - **More than N rows: not compared**: it returned more than the maximum, so
    its result isn't verified.
  - **Unverified**, **Error** or **Plan only** (what modifies data).
- **Cost:** the plan's estimated cost, if the engine gives it.
- **vs. original:** how much faster or slower it is, and how much the cost
  changes.

The fastest version is marked **Recommended** **only if** its result is
equivalent and its average is at least 10% lower than the original's. A
version that hasn't been proven equivalent is never recommended.

It can be cancelled while comparing. The comparison takes as long as the
query multiplied by the runs: choose few with heavy queries.

## Use this version

The **Use this version** button of each alternative has two options:

- **Open in a new query** (the main action).
- **Replace the query in its editor**: changes the text of the selection or
  the original statement in the editor it came from, **without running it**.
  If that editor was closed or the text changed, it doesn't overwrite
  anything: it opens the version in a new query and says so.

Before using it, DBine asks if the version **did NOT return the same
result** as the original, or if it **hasn't been verified yet**. The question
is only skipped for a rule proven equivalent, or a version that already gave
**Same result** (or **Plan only**).

## Per-engine particularities

- Plans and suggested indexes depend on the engine giving execution plans
  ([`engine-support.md`](engine-support.md#planes-de-ejecución)).
- The rules change with the dialect (see above), and `function_to_range` is
  only in four.
- What each engine doesn't do is in
  [`engine-support.md`](engine-support.md#optimize-query).

## Contract

It adds no methods to the drivers: it uses `Session::explain` (the estimated
plan) and `supports_explain()`, `database_schema` (the structure for the
rules and the AI), `table_ddl` with the indexes (the engine's `CREATE INDEX`)
and `Session::execute` with the export's row receiver (for the result
fingerprint).

Commands: `optimizer_analyze`, `optimizer_ai`, `optimizer_compare` (event
`optimizer-progress`) and `optimizer_cancel`; `cancel_query` on
`optimize:<runId>` also stops them. See [`api-commands.md`](api-commands.md).
