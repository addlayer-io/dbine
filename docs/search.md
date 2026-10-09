# Search in database

Searches for a text in the **names** of a database's objects, in **column
names** and in the **code** of views, procedures, functions and triggers.
It's useful to know where a table, a column or a function is used before
changing it.

## Where it is

Right-click a **database** › **Search in database…**. It opens in its own tab
(**Search**).

## What can be searched

- The **text** to search for.
- **Names:** the names of objects (tables, views, routines…) **and column
  names**. A column appears with its table and its type.
- **Code:** the text of the definitions of objects that have code. Tables
  aren't part of the code search (their definition is a DDL the engine
  generates, not code someone wrote); their names are.
- **Match case** and **Whole word** (the text can't be inside a longer
  identifier: `id` doesn't match `user_id`).
- **All types**, or a filter by object type (and **Columns**).

## The results

Results are grouped by object, in the order they are found, and arrive **as
the search progresses**, with progress (**Checking N of M objects**). For
each object you see whether it matched in the name (**in the name**) and the
lines of code that match, with their line number.

- A click on a result opens the object: its **definition** if it has code,
  its data if it is a table or collection, or its structure. A click on a
  column opens the **structure of its table**.
- The search can be **cancelled**: partial results stay, with **Cancelled:
  partial results**.
- It stops at a maximum of 2,000 results (**Stopped at the maximum number of
  results**).
- Objects whose code couldn't be read (for example, for lack of permissions)
  are counted separately (**N objects could not be read**); they don't stop
  the search.
- It shows how many definitions were checked.

The search runs on its **own read-only session**: the explorer stays free in
the meantime.

## Engine particularities

- **Columns** are searched using the database structure
  (`database_schema`). If the engine doesn't offer it, the search continues
  with names and code.
- **Code** is read in two ways, with the same result:
  - **Through the engine's catalog, in a single query**, filtered on the
    server: PostgreSQL and its family, SQL Server, Oracle, SAP HANA,
    Firebird, ClickHouse, Snowflake, BigQuery, Databricks and the ODBC
    profiles that implement it.
  - **Object by object**, asking the engine for each one's definition, with
    progress and partial results: the rest of the engines, and the variants
    of the above whose code can only be requested one at a time.
- Engines whose object types have no definition (no code to read) search only
  names and columns.

## Contract

It uses these `Session` methods:

- `list_objects` and `database_schema` (names and columns);
- `definition` (an object's code) and, optionally,
  `search_code(&CodeSearch) -> Option<CodeSearchReport>`: the search in the
  engine's catalog in one query. By default it returns `None` and the search
  is done object by object. Lines are compared with the same rule
  (`line_matches`), so both paths give the same results.

Command: `search_database`, with the `code-search-progress` event and
cancellable with `cancel_query` on `search:<searchId>`. See
[`api-commands.md`](api-commands.md).
