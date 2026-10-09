# Test data

Fills a table with invented but plausible rows: names, emails, dates,
amounts, foreign keys that point to rows that exist. It's useful for testing
an application or a query without copying real data.

## Where it is

Right-click a **table** › **Generate test data…**. The menu appears on tables
(and objects with readable columns) of connections that are **not** read-only.

## The dialog

- **Rows:** how many rows to generate (from 1 to 10,000,000).
- A grid with one row per **column**: its **Type**, the **Generator**, its
  **Settings** and the **% nulls** (only on columns that accept `NULL`).
- **Sample:** 8 example rows with the chosen generators, to see how it will
  look. **Refresh sample** recalculates it, and **Another sample** changes
  the seed.

**Generate** asks for confirmation (**N rows will be inserted into "table" on
the connection "…"**) and runs in the background, as a task with progress and
an option to cancel. When it finishes it says how many rows were inserted.

## The generators

Each column has **Automatic** (the default) or a chosen one:

| Generator | What it does | Settings |
|---|---|---|
| **Automatic** | Chooses by the column's **name** (email, name, city…) and, failing that, by its **type**. | |
| **Skip (default value)** | Leaves the column out of the `INSERT`. | |
| **Null** | `NULL` (fails if the column doesn't accept nulls). | |
| **Fixed value** | The same value in every row. | value |
| **Sequence** | A number that grows. | start, step |
| **Integer** | A random integer. | min., max. |
| **Decimal** | A random decimal. | min., max., decimals |
| **True / false** | A boolean. | |
| **Date**, **Date and time** | A date in a range (by default, the last year). | from, to (`YYYY-MM-DD`) |
| **UUID** | A UUID. | |
| **Text** | Random words. | min., max. (words) |
| **First name**, **Last name**, **Full name**, **Email**, **Phone**, **City**, **Country**, **Company**, **Address** | Values from built-in lists, in Spanish. | |
| **From a list** | One of the values you type, separated by commas (it can't be empty). | values |
| **From the referenced table** | A value that already exists in the foreign key's parent table. | |

### What "Automatic" does

- **Identity or auto-increment** columns: skipped.
- Columns with a **single-column foreign key**: take values from the
  referenced table (up to 1,000 values read from it). If the parent table has
  no rows, **From the referenced table** fails with a message. Multi-column
  foreign keys aren't resolved.
- Text: by the column's name (`email`/`correo`, `nombre`, `apellido`,
  `telefono`/`celular`, `ciudad`, `pais`, `empresa`, `direccion`, `uuid`);
  otherwise, random words.
- Numbers: small integers, or decimals with the type's scale (up to 6); dates
  from the last three years; random booleans; JSON as `{}`.
- Texts **respect the column's length** (they are truncated).
- **Single-column primary key:** values aren't repeated within what is
  generated (if it is an integer, it starts at a random high number so as not
  to collide with the ids the table usually has). The generator only checks
  against what it generated itself: it doesn't query the rows the table
  already has, and it doesn't detect other `UNIQUE` constraints or composite
  keys as unique. If the table has others, choose a generator that doesn't
  repeat (for example **Sequence** or **UUID**).

## How it is inserted

Rows are generated in DBine and inserted **500** at a time with the engine's
`INSERT` script, on their own session, like an import. If a batch fails,
generation stops with the error and the row range (**rows 501–1000: …**);
**the previous batches stay inserted**, because there is no transaction
around everything.

The run uses the same seed as the dialog's sample; **Another sample**
changes it.

## Engine particularities

The generators live in DBine and not in the drivers, so it works on **every
engine that allows inserting** from DBine. The only things that depend on the
engine are:

- the insert script (`insert_script`) and the columns' types;
- reading foreign keys, which exists only on engines with foreign keys.

## Contract

It adds no methods to the drivers: it uses `Session::columns`,
`database_schema` (foreign keys), `browse_query` (the parent's values),
`Driver::insert_script` and `Session::execute`.

Commands: `datagen_preview` and `datagen_run` (event `datagen-progress`,
cancellable with `cancel_query` on `datagen:<genId>`). See
[`api-commands.md`](api-commands.md).
