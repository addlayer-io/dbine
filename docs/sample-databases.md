# Sample databases

DBine adds a **Sample databases** folder to the explorer once, so there is
something to explore before connecting anything of your own. Nothing in it
needs a server, and nothing is downloaded until you open it.

## What's in the folder

| Connection | Engine | What it holds |
|---|---|---|
| **Store (SQLite)** | SQLite | A shop's operational database: `clientes` (2,000), `productos` (200), `pedidos` (10,000), `pedido_items` (30,000), `facturas` (10,000), `eventos` (20,000) and `auditoria`. It has foreign keys, indexes, the views `v_ventas_mensuales`, `v_clientes_activos` and `v_kpis` (a view over a view), and the trigger `tr_pedidos_audit`. |
| **Analytics (DuckDB)** | DuckDB | The same model and data, with differences on purpose (below), the macro `fn_total_pedido`, the sequence `seq_auditoria` and one more view, `v_ventas_por_ciudad`. |
| **Files (CSV and Parquet)** | CSV / Parquet / JSON files | `metas.csv`, the monthly sales targets per category, and `eventos.parquet`, the store's events. Each file is a table named after it. |

The names follow the interface's language when the folder is added. The data
is the same in every language; its tables and columns have Spanish names.

### The differences between Store and Analytics

They are there so that comparing finds something:

- **Clients:** every 50th client (40 of them) moved to Ushuaia with a new
  e-mail, and Analytics has 10 clients that Store doesn't (ids 2001–2010).
- **Products:** every 50th price is 10% higher, and Analytics has a `stock`
  column.
- **Tables and views:** Analytics has no `eventos` table (the events are in the
  Parquet file) and has the view `v_ventas_por_ciudad`.
- **Orders, items and invoices** are the same in both, so their totals match.

Some things to try with them:

- **Compare data** between the two `clientes` or `productos` tables, and the
  script that syncs one side to the other.
- **Compare schemas** between the two databases.
- **Migrate** Store from SQLite to a new DuckDB database.
- The **database diagram** of Store, and the **dependencies** of `v_kpis`.

## The tour

A notebook, **DBine tour**, walks through them:

1. It reads the clients of Store and of Analytics, each cell on its own
   connection.
2. A DataFrame SQL cell crosses the two results and charts, by city, the
   clients that differ.
3. It reads the targets from the CSV and the monthly sales from Store's
   `v_ventas_mensuales`.
4. A last DataFrame SQL cell joins sales and targets by month and category and
   charts them.

On a new installation the tour opens by itself the first time. Afterwards,
**Open the tour** in the folder's context menu opens it again (it is written
again if it was deleted).

## When the files are made

- The folder and its three connections are added at the first start, on new
  installations and on installations that update to the version that brings
  them alike. The databases themselves are made the first time each connection
  opens, in the app's data folder (`samples/`).
- **Store** is made at once, by DBine itself.
- **Analytics** and **Files** use DuckDB, which DBine downloads the first time
  any DuckDB connection is used. So opening one of them may start that
  download; nothing is downloaded at startup.
- A database or file that is missing (deleted, or a cloud backup restored on
  another computer) is made again the next time its connection opens.

## Removing them

**Remove the sample databases…** in the folder's context menu deletes the
three connections, the folder and their files. Deleting the folder like any
other one also works: its connections move up to the top level.

Either way, they never come back: DBine remembers that it added them (a
preference that travels with the cloud backup), not whether they are still
there. A connection of your own that you put in the folder is kept and moves
up.
