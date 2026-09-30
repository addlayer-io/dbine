//! Same-engine clone against a real server: a source database that uses
//! every feature the clone keeps, cloned into a fresh database (script →
//! data through the native copy → script again, as a resumed clone would),
//! then both compared catalog by catalog. Reads `DBINE_TEST_SQLSERVER_URL`
//! (`mssql://user:pass@host:port`), by default the `dbine-test-sqlserver`
//! container:
//!
//! ```sh
//! cargo test -p dbine-driver-sqlserver --test clone -- --ignored clone --test-threads=1 --nocapture
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::transfer::{CopySpec, LoadSpec, ReadSpec};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

const DEFAULT_URL: &str = "mssql://sa:Pw_12345!@localhost:25013";
const SRC_DB: &str = "dbine_clone_src";
const DST_DB: &str = "dbine_clone_dst";

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    }
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers().remove(0)
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100_000, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    try_run(s, sql).await.unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

/// Every row of every result as text.
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<String> {
    let out = run(s, sql).await;
    let mut v: Vec<String> = out
        .results
        .iter()
        .flat_map(|r| r.rows.iter())
        .map(|r| r.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" | "))
        .collect();
    v.sort();
    v
}

async fn drop_databases(admin: &mut Box<dyn Session>) {
    for db in [SRC_DB, DST_DB] {
        run(
            admin,
            &format!("IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END"),
        )
        .await;
    }
}

/// The source: every feature the clone keeps, with a little data.
const SOURCE_SQL: &str = r#"
ALTER DATABASE CURRENT SET RECOVERY SIMPLE
GO
DECLARE @dir nvarchar(4000) = (SELECT TOP 1 LEFT(physical_name, LEN(physical_name) - CHARINDEX('/', REVERSE(physical_name)) + 1) FROM sys.database_files WHERE type = 0);
DECLARE @sql nvarchar(max) = N'ALTER DATABASE CURRENT ADD FILEGROUP FG_A;
ALTER DATABASE CURRENT ADD FILE (NAME = N''clone_fga'', FILENAME = N''' + @dir + N'dbine_clone_src_fga.ndf'', SIZE = 8MB) TO FILEGROUP FG_A;
ALTER DATABASE CURRENT ADD FILEGROUP FG_MEM CONTAINS MEMORY_OPTIMIZED_DATA;
ALTER DATABASE CURRENT ADD FILE (NAME = N''clone_mem'', FILENAME = N''' + @dir + N'dbine_clone_src_mem'') TO FILEGROUP FG_MEM;';
EXEC (@sql);
GO
CREATE SCHEMA ven
GO
CREATE SCHEMA hist
GO
CREATE XML SCHEMA COLLECTION ven.OrderSchema AS N'<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema"><xsd:element name="order" type="xsd:string"/></xsd:schema>'
GO
CREATE TYPE ven.Code FROM varchar(20) NOT NULL
GO
CREATE TYPE ven.Lines AS TABLE (Id int PRIMARY KEY, Qty int CHECK (Qty > 0), Note nvarchar(10) DEFAULT N'x', INDEX IX_Qty (Qty))
GO
CREATE SEQUENCE ven.Seq AS bigint START WITH 100 INCREMENT BY 5 CACHE 20
GO
CREATE SEQUENCE ven.Unused AS int START WITH 7 INCREMENT BY 1 MINVALUE 1 MAXVALUE 1000 CYCLE NO CACHE
GO
DECLARE @x bigint = NEXT VALUE FOR ven.Seq; SET @x = NEXT VALUE FOR ven.Seq; SET @x = NEXT VALUE FOR ven.Seq;
GO
CREATE PARTITION FUNCTION pf_region (int) AS RANGE RIGHT FOR VALUES (100, 200)
GO
CREATE PARTITION SCHEME ps_region AS PARTITION pf_region TO (FG_A, [PRIMARY], FG_A)
GO
ALTER PARTITION SCHEME ps_region NEXT USED [PRIMARY]
GO
CREATE PARTITION FUNCTION pf_day (date) AS RANGE LEFT FOR VALUES ('2024-01-01', '2025-01-01')
GO
CREATE PARTITION SCHEME ps_day AS PARTITION pf_day ALL TO ([PRIMARY])
GO
CREATE FUNCTION ven.fn_double (@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x * 2 END
GO
CREATE FUNCTION ven.fn_positive (@x money) RETURNS bit AS BEGIN RETURN CASE WHEN @x >= 0 THEN 1 ELSE 0 END END
GO
CREATE TABLE ven.Customer (
    Id int IDENTITY(1000, 5) NOT NULL,
    Code ven.Code,
    Name nvarchar(100) COLLATE Latin1_General_CS_AS NOT NULL CONSTRAINT DF_Customer_Name DEFAULT N'?',
    Twice AS ven.fn_double(Id) PERSISTED,
    Rg uniqueidentifier ROWGUIDCOL NOT NULL CONSTRAINT DF_Customer_Rg DEFAULT NEWSEQUENTIALID(),
    Sp int SPARSE NULL,
    Doc xml(CONTENT ven.OrderSchema) NULL,
    Email varchar(100) MASKED WITH (FUNCTION = 'email()') NULL,
    Notes nvarchar(max) NULL,
    NextNo bigint NOT NULL CONSTRAINT DF_Customer_NextNo DEFAULT (NEXT VALUE FOR ven.Seq),
    CONSTRAINT PK_Customer PRIMARY KEY NONCLUSTERED (Id DESC) WITH (FILLFACTOR = 90) ON FG_A
) ON FG_A TEXTIMAGE_ON FG_A
GO
CREATE CLUSTERED INDEX CIX_Customer_Code ON ven.Customer (Code) ON FG_A
GO
CREATE NONCLUSTERED INDEX IX_Customer_Name ON ven.Customer (Name ASC, Code DESC) INCLUDE (Email) WHERE Email IS NOT NULL
    WITH (PAD_INDEX = ON, FILLFACTOR = 80, ALLOW_PAGE_LOCKS = OFF, STATISTICS_NORECOMPUTE = ON) ON [PRIMARY]
GO
ALTER TABLE ven.Customer ADD CONSTRAINT UQ_Customer_Rg UNIQUE NONCLUSTERED (Rg) WITH (IGNORE_DUP_KEY = OFF, ALLOW_ROW_LOCKS = OFF)
GO
CREATE INDEX IX_Customer_Disabled ON ven.Customer (Twice)
GO
ALTER INDEX IX_Customer_Disabled ON ven.Customer DISABLE
GO
CREATE STATISTICS ST_Customer_NameCode ON ven.Customer (Name, Code) WHERE Code > 'B' WITH NORECOMPUTE
GO
INSERT INTO ven.Customer (Code, Name, Sp, Doc, Email, Notes)
SELECT 'C' + RIGHT('000' + CAST(n AS varchar(3)), 3), N'Cliente ' + CAST(n AS nvarchar(10)), CASE WHEN n % 3 = 0 THEN n END,
       N'<order>x</order>', 'c' + CAST(n AS varchar(10)) + '@ejemplo.com', REPLICATE(N'n', n)
  FROM (SELECT TOP 50 ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS n FROM sys.all_objects) x
GO
CREATE TABLE ven.OrderHeader (
    Id int NOT NULL,
    Region int NOT NULL,
    Day date NOT NULL,
    Amount money NOT NULL,
    -- LOB on a partitioned table: its LOB data space is the scheme, not a filegroup.
    Remarks nvarchar(max) NULL,
    CONSTRAINT PK_OrderHeader PRIMARY KEY CLUSTERED (Id, Region)
        WITH (DATA_COMPRESSION = PAGE ON PARTITIONS (1), DATA_COMPRESSION = ROW ON PARTITIONS (2 TO 3)) ON ps_region (Region)
) ON ps_region (Region)
GO
CREATE INDEX IX_OrderHeader_Day ON ven.OrderHeader (Day) ON ps_day (Day)
GO
INSERT INTO ven.OrderHeader (Id, Region, Day, Amount, Remarks) SELECT n, (n % 3) * 100, DATEADD(day, n * 20, '2023-06-01'), n * 10, REPLICATE(N'r', n) FROM (SELECT TOP 60 ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS n FROM sys.all_objects) x
GO
ALTER TABLE ven.OrderHeader ADD CONSTRAINT CK_OrderHeader_Amount CHECK (ven.fn_positive(Amount) = 1)
GO
ALTER TABLE ven.OrderHeader WITH NOCHECK ADD CONSTRAINT CK_OrderHeader_Day CHECK (Day > '2000-01-01')
GO
ALTER TABLE ven.OrderHeader ADD CONSTRAINT CK_OrderHeader_Off CHECK (Amount < 1000000)
GO
ALTER TABLE ven.OrderHeader NOCHECK CONSTRAINT CK_OrderHeader_Off
GO
CREATE TABLE ven.OrderLine (
    Id bigint IDENTITY(1, 1) NOT NULL CONSTRAINT PK_OrderLine PRIMARY KEY CLUSTERED,
    OrderId int NOT NULL,
    Region int NOT NULL,
    CustomerId int NULL,
    Qty int NOT NULL,
    Body xml NULL
)
GO
INSERT INTO ven.OrderLine (OrderId, Region, CustomerId, Qty, Body)
SELECT n, (n % 3) * 100, 1000 + (n % 10) * 5, n, N'<l><q>' + CAST(n AS nvarchar(10)) + N'</q></l>' FROM (SELECT TOP 60 ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS n FROM sys.all_objects) x
GO
CREATE PRIMARY XML INDEX PXML_OrderLine_Body ON ven.OrderLine (Body)
GO
CREATE XML INDEX SXML_OrderLine_Path ON ven.OrderLine (Body) USING XML INDEX PXML_OrderLine_Body FOR PATH
GO
ALTER TABLE ven.OrderLine ADD CONSTRAINT FK_OrderLine_Header FOREIGN KEY (OrderId, Region) REFERENCES ven.OrderHeader (Id, Region) ON DELETE CASCADE
GO
ALTER TABLE ven.OrderLine WITH NOCHECK ADD CONSTRAINT FK_OrderLine_Customer FOREIGN KEY (CustomerId) REFERENCES ven.Customer (Id) ON UPDATE CASCADE
GO
ALTER TABLE ven.OrderLine ADD CONSTRAINT FK_OrderLine_Customer2 FOREIGN KEY (CustomerId) REFERENCES ven.Customer (Id) NOT FOR REPLICATION
GO
ALTER TABLE ven.OrderLine NOCHECK CONSTRAINT FK_OrderLine_Customer2
GO
CREATE TABLE ven.Fact (Day date NOT NULL, Store int NOT NULL, Qty int NOT NULL)
GO
CREATE CLUSTERED COLUMNSTORE INDEX CCI_Fact ON ven.Fact ORDER (Day, Store) WITH (COMPRESSION_DELAY = 10)
GO
INSERT INTO ven.Fact SELECT DATEADD(day, n, '2024-01-01'), n % 7, n FROM (SELECT TOP 100 ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS n FROM sys.all_objects) x
GO
CREATE TABLE ven.FactArchive (Day date NOT NULL, Qty int NOT NULL, INDEX CCI_FactArchive CLUSTERED COLUMNSTORE WITH (DATA_COMPRESSION = COLUMNSTORE_ARCHIVE))
GO
INSERT INTO ven.FactArchive VALUES ('2020-01-01', 1), ('2020-01-02', 2)
GO
CREATE TABLE ven.Stock (Id int NOT NULL CONSTRAINT PK_Stock PRIMARY KEY WITH (OPTIMIZE_FOR_SEQUENTIAL_KEY = ON), Qty int NOT NULL, Deleted bit NOT NULL)
GO
CREATE NONCLUSTERED COLUMNSTORE INDEX NCCI_Stock ON ven.Stock (Qty, Deleted) WHERE Deleted = 0
GO
INSERT INTO ven.Stock VALUES (1, 10, 0), (2, 20, 1), (3, 30, 0)
GO
CREATE INDEX IX_Stock_Qty ON ven.Stock (Qty DESC) WITH (DATA_COMPRESSION = PAGE)
GO
CREATE TABLE ven.Place (Id int NOT NULL CONSTRAINT PK_Place PRIMARY KEY CLUSTERED, Shape geometry NULL, Loc geography NULL)
GO
CREATE SPATIAL INDEX SIX_Place_Shape ON ven.Place (Shape) USING GEOMETRY_GRID
    WITH (BOUNDING_BOX = (0, 0, 500, 200.5), GRIDS = (LEVEL_1 = LOW, LEVEL_2 = MEDIUM, LEVEL_3 = HIGH, LEVEL_4 = MEDIUM), CELLS_PER_OBJECT = 32)
GO
CREATE SPATIAL INDEX SIX_Place_Loc ON ven.Place (Loc) USING GEOGRAPHY_AUTO_GRID WITH (CELLS_PER_OBJECT = 12)
GO
INSERT INTO ven.Place VALUES (1, geometry::STGeomFromText('POINT (10 20)', 0), geography::Point(-34.6, -58.4, 4326))
GO
CREATE TABLE ven.Session (
    Id int IDENTITY(1, 1) NOT NULL CONSTRAINT PK_Session PRIMARY KEY NONCLUSTERED HASH WITH (BUCKET_COUNT = 1024),
    Token nvarchar(50) NOT NULL,
    LastSeen datetime2(3) NOT NULL,
    INDEX IX_Session_Last NONCLUSTERED (LastSeen DESC),
    CONSTRAINT UQ_Session_Token UNIQUE NONCLUSTERED HASH (Token) WITH (BUCKET_COUNT = 2048)
) WITH (MEMORY_OPTIMIZED = ON, DURABILITY = SCHEMA_AND_DATA)
GO
INSERT INTO ven.Session (Token, LastSeen) VALUES (N'a', '2024-01-01'), (N'b', '2024-01-02'), (N'c', '2024-01-03')
GO
CREATE TABLE ven.Price (
    Id int NOT NULL CONSTRAINT PK_Price PRIMARY KEY,
    Amount decimal(10, 2) NOT NULL,
    ValidFrom datetime2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL,
    ValidTo datetime2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL,
    PERIOD FOR SYSTEM_TIME (ValidFrom, ValidTo)
) WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = hist.PriceHistory, HISTORY_RETENTION_PERIOD = 6 MONTHS))
GO
INSERT INTO ven.Price (Id, Amount) VALUES (1, 10), (2, 20), (3, 30)
GO
UPDATE ven.Price SET Amount = Amount + 1 WHERE Id < 3
GO
CREATE TABLE ven.Empty (Id int IDENTITY(1, 1) NOT NULL CONSTRAINT PK_Empty PRIMARY KEY, X int NULL)
GO
INSERT INTO ven.Empty (X) VALUES (1), (2), (3)
GO
DELETE FROM ven.Empty
GO
CREATE TABLE ven.Log (Msg nvarchar(100) NULL) WITH (DATA_COMPRESSION = ROW)
GO
INSERT INTO ven.Log VALUES (N'uno'), (N'dos')
GO
SET ANSI_NULLS OFF
GO
SET QUOTED_IDENTIFIER OFF
GO
CREATE VIEW ven.vCustomer AS SELECT Id, Name, "x" AS Tag FROM ven.Customer WHERE Email <> NULL
GO
SET ANSI_NULLS ON
GO
SET QUOTED_IDENTIFIER ON
GO
CREATE FUNCTION ven.fnOrders (@region int) RETURNS TABLE AS RETURN (SELECT Id, Amount FROM ven.OrderHeader WHERE Region = @region)
GO
CREATE VIEW ven.vOrderTotals WITH SCHEMABINDING AS
SELECT Region, COUNT_BIG(*) AS N, SUM(Amount) AS Total FROM ven.OrderHeader GROUP BY Region
GO
CREATE UNIQUE CLUSTERED INDEX CIX_vOrderTotals ON ven.vOrderTotals (Region)
GO
CREATE INDEX IX_vOrderTotals_N ON ven.vOrderTotals (N)
GO
CREATE VIEW ven.vRegion100 AS SELECT * FROM ven.fnOrders(100)
GO
EXEC (N'CREATE PROCEDURE ven.pGet @id int AS
-- a line that reads just GO follows, inside a comment
/*
' + N'GO
*/
SELECT * FROM ven.vCustomer WHERE Id = @id')
GO
CREATE TRIGGER ven.trCustomer_A ON ven.Customer AFTER INSERT AS SET NOCOUNT ON
GO
CREATE TRIGGER ven.trCustomer_B ON ven.Customer AFTER INSERT, UPDATE AS SET NOCOUNT ON
GO
CREATE TRIGGER ven.trCustomer_Off ON ven.Customer AFTER DELETE AS SET NOCOUNT ON
GO
DISABLE TRIGGER ven.trCustomer_Off ON ven.Customer
GO
EXEC sp_settriggerorder @triggername = N'ven.trCustomer_B', @order = N'First', @stmttype = N'INSERT'
GO
EXEC sp_settriggerorder @triggername = N'ven.trCustomer_A', @order = N'Last', @stmttype = N'INSERT'
GO
CREATE TRIGGER ven.trRegion ON ven.vRegion100 INSTEAD OF INSERT AS SET NOCOUNT ON
GO
CREATE SYNONYM ven.Cust FOR ven.Customer
GO
EXEC sys.sp_addextendedproperty @name = N'Base', @value = N'Clon de prueba'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Ventas', @level0type = N'SCHEMA', @level0name = N'ven'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Clientes (sin IVA)', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TABLE', @level1name = N'Customer'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Nombre', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TABLE', @level1name = N'Customer', @level2type = N'COLUMN', @level2name = N'Name'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Por nombre', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TABLE', @level1name = N'Customer', @level2type = N'INDEX', @level2name = N'IX_Customer_Name'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Monto positivo', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TABLE', @level1name = N'OrderHeader', @level2type = N'CONSTRAINT', @level2name = N'CK_OrderHeader_Amount'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Al insertar', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TABLE', @level1name = N'Customer', @level2type = N'TRIGGER', @level2name = N'trCustomer_A'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Totales', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'VIEW', @level1name = N'vOrderTotals'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'El id', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'PROCEDURE', @level1name = N'pGet', @level2type = N'PARAMETER', @level2name = N'@id'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Código', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TYPE', @level1name = N'Code'
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Numerador', @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'SEQUENCE', @level1name = N'Seq'
DECLARE @n sql_variant = CAST(42 AS int);
EXEC sys.sp_addextendedproperty @name = N'Version', @value = @n, @level0type = N'SCHEMA', @level0name = N'ven', @level1type = N'TABLE', @level1name = N'Stock'
"#;

/// Catalog views compared between both databases, by name (never by id).
const COMPARE: &[(&str, &str)] = &[
    ("filegroups", "SELECT name, type FROM sys.filegroups"),
    ("schemas", "SELECT name FROM sys.schemas WHERE schema_id > 4 AND schema_id < 16384"),
    (
        "tables",
        "SELECT SCHEMA_NAME(t.schema_id) + '.' + t.name, t.is_memory_optimized, t.durability_desc, t.temporal_type_desc,
                OBJECT_SCHEMA_NAME(t.history_table_id) + '.' + OBJECT_NAME(t.history_table_id), t.history_retention_period,
                t.history_retention_period_unit_desc, lob.name,
                (SELECT COUNT(*) FROM sys.partitions p WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)),
                (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)),
                (SELECT STRING_AGG(p.data_compression_desc, ',') WITHIN GROUP (ORDER BY p.partition_number) FROM sys.partitions p WHERE p.object_id = t.object_id AND p.index_id = 0)
           FROM sys.tables t LEFT JOIN sys.data_spaces lob ON lob.data_space_id = t.lob_data_space_id WHERE t.is_ms_shipped = 0",
    ),
    (
        "columns",
        "SELECT OBJECT_SCHEMA_NAME(c.object_id) + '.' + OBJECT_NAME(c.object_id), c.column_id, c.name, TYPE_NAME(c.user_type_id),
                c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed, c.collation_name, c.is_sparse,
                c.is_rowguidcol, c.is_hidden, c.generated_always_type_desc, mc.masking_function, xc.name, c.is_xml_document,
                cc.definition, cc.is_persisted, dc.name, dc.definition,
                CAST(ic.seed_value AS nvarchar(40)), CAST(ic.increment_value AS nvarchar(40))
           FROM sys.columns c
           JOIN sys.objects o ON o.object_id = c.object_id AND o.is_ms_shipped = 0 AND o.type IN ('U', 'V', 'TT')
           LEFT JOIN sys.masked_columns mc ON mc.object_id = c.object_id AND mc.column_id = c.column_id
           LEFT JOIN sys.xml_schema_collections xc ON xc.xml_collection_id = c.xml_collection_id AND c.xml_collection_id > 1
           LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id
           LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
           LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id
          WHERE o.type <> 'TT'",
    ),
    (
        "indexes",
        "SELECT OBJECT_SCHEMA_NAME(i.object_id) + '.' + OBJECT_NAME(i.object_id), i.name, i.type_desc, i.is_primary_key, i.is_unique,
                i.is_unique_constraint, i.filter_definition, i.fill_factor, i.is_padded, i.ignore_dup_key, i.allow_row_locks,
                i.allow_page_locks, i.is_disabled, i.optimize_for_sequential_key, i.compression_delay, ds.name, st.no_recompute,
                (SELECT STRING_AGG(COL_NAME(ic.object_id, ic.column_id) + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END
                        + ':' + CAST(ic.key_ordinal AS varchar(3)) + ':' + CAST(ic.is_included_column AS varchar(1))
                        + ':' + CAST(ic.partition_ordinal AS varchar(3)) + ':' + CAST(ic.column_store_order_ordinal AS varchar(3)), ',')
                        WITHIN GROUP (ORDER BY ic.index_column_id)
                   FROM sys.index_columns ic WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id),
                (SELECT STRING_AGG(p.data_compression_desc, ',') WITHIN GROUP (ORDER BY p.partition_number)
                   FROM sys.partitions p WHERE p.object_id = i.object_id AND p.index_id = i.index_id),
                h.bucket_count, xi.secondary_type_desc, pxi.name,
                tes.tessellation_scheme, tes.bounding_box_xmin, tes.bounding_box_ymax, tes.level_1_grid_desc, tes.level_3_grid_desc, tes.cells_per_object
           FROM sys.indexes i
           JOIN sys.objects o ON o.object_id = i.object_id AND o.is_ms_shipped = 0 AND o.type IN ('U', 'V')
           LEFT JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id
           LEFT JOIN sys.stats st ON st.object_id = i.object_id AND st.stats_id = i.index_id
           LEFT JOIN sys.hash_indexes h ON h.object_id = i.object_id AND h.index_id = i.index_id
           LEFT JOIN sys.xml_indexes xi ON xi.object_id = i.object_id AND xi.index_id = i.index_id
           LEFT JOIN sys.indexes pxi ON pxi.object_id = xi.object_id AND pxi.index_id = xi.using_xml_index_id
           LEFT JOIN sys.spatial_index_tessellations tes ON tes.object_id = i.object_id AND tes.index_id = i.index_id
          WHERE i.type > 0",
    ),
    (
        "statistics",
        "SELECT OBJECT_SCHEMA_NAME(s.object_id) + '.' + OBJECT_NAME(s.object_id), s.name, s.filter_definition, s.no_recompute,
                (SELECT STRING_AGG(COL_NAME(sc.object_id, sc.column_id), ',') WITHIN GROUP (ORDER BY sc.stats_column_id)
                   FROM sys.stats_columns sc WHERE sc.object_id = s.object_id AND sc.stats_id = s.stats_id)
           FROM sys.stats s JOIN sys.objects o ON o.object_id = s.object_id AND o.is_ms_shipped = 0 WHERE s.user_created = 1",
    ),
    (
        "checks",
        "SELECT OBJECT_SCHEMA_NAME(parent_object_id) + '.' + OBJECT_NAME(parent_object_id), name, definition, is_disabled, is_not_trusted, is_not_for_replication
           FROM sys.check_constraints WHERE parent_object_id IN (SELECT object_id FROM sys.tables)",
    ),
    (
        "foreign keys",
        "SELECT OBJECT_SCHEMA_NAME(fk.parent_object_id) + '.' + OBJECT_NAME(fk.parent_object_id), fk.name,
                OBJECT_SCHEMA_NAME(fk.referenced_object_id) + '.' + OBJECT_NAME(fk.referenced_object_id),
                fk.delete_referential_action_desc, fk.update_referential_action_desc, fk.is_disabled, fk.is_not_trusted, fk.is_not_for_replication,
                (SELECT STRING_AGG(COL_NAME(c.parent_object_id, c.parent_column_id) + '>' + COL_NAME(c.referenced_object_id, c.referenced_column_id), ',')
                        WITHIN GROUP (ORDER BY c.constraint_column_id)
                   FROM sys.foreign_key_columns c WHERE c.constraint_object_id = fk.object_id)
           FROM sys.foreign_keys fk",
    ),
    (
        "identity",
        "SELECT OBJECT_SCHEMA_NAME(object_id) + '.' + OBJECT_NAME(object_id), CAST(last_value AS nvarchar(40)), CAST(IDENT_CURRENT(OBJECT_SCHEMA_NAME(object_id) + '.' + OBJECT_NAME(object_id)) AS nvarchar(40))
           FROM sys.identity_columns WHERE OBJECTPROPERTY(object_id, 'IsUserTable') = 1 AND OBJECT_NAME(object_id) <> 'Empty'",
    ),
    (
        "modules",
        "SELECT SCHEMA_NAME(o.schema_id) + '.' + o.name, o.type, m.definition, m.uses_ansi_nulls, m.uses_quoted_identifier, m.is_schema_bound,
                tr.is_disabled, tr.is_instead_of_trigger, OBJECT_NAME(tr.parent_id),
                (SELECT STRING_AGG(te.type_desc + ':' + CAST(te.is_first AS varchar(1)) + CAST(te.is_last AS varchar(1)), ',') WITHIN GROUP (ORDER BY te.type_desc)
                   FROM sys.trigger_events te WHERE te.object_id = o.object_id)
           FROM sys.sql_modules m JOIN sys.objects o ON o.object_id = m.object_id LEFT JOIN sys.triggers tr ON tr.object_id = m.object_id
          WHERE o.is_ms_shipped = 0",
    ),
    (
        "extended properties",
        "SELECT ep.class_desc, ep.name, CAST(ep.value AS nvarchar(4000)), CAST(SQL_VARIANT_PROPERTY(ep.value, 'BaseType') AS sysname),
                CASE WHEN ep.class IN (1, 2, 7) THEN OBJECT_SCHEMA_NAME(ep.major_id) + '.' + OBJECT_NAME(ep.major_id)
                     WHEN ep.class = 3 THEN SCHEMA_NAME(ep.major_id) WHEN ep.class = 6 THEN TYPE_NAME(ep.major_id) END,
                CASE WHEN ep.class = 1 AND ep.minor_id > 0 THEN COL_NAME(ep.major_id, ep.minor_id)
                     WHEN ep.class = 7 THEN (SELECT name FROM sys.indexes WHERE object_id = ep.major_id AND index_id = ep.minor_id)
                     WHEN ep.class = 2 THEN (SELECT name FROM sys.parameters WHERE object_id = ep.major_id AND parameter_id = ep.minor_id) END
           FROM sys.extended_properties ep",
    ),
    (
        "periods",
        "SELECT OBJECT_NAME(object_id), COL_NAME(object_id, start_column_id), COL_NAME(object_id, end_column_id) FROM sys.periods",
    ),
    ("database", "SELECT is_temporal_history_retention_enabled FROM sys.databases WHERE database_id = DB_ID()"),
    (
        "partition functions",
        "SELECT pf.name, pf.type_desc, pf.boundary_value_on_right, pf.fanout, TYPE_NAME(pp.system_type_id),
                (SELECT STRING_AGG(CONVERT(nvarchar(100), rv.value, 126), ',') WITHIN GROUP (ORDER BY rv.boundary_id)
                   FROM sys.partition_range_values rv WHERE rv.function_id = pf.function_id)
           FROM sys.partition_functions pf JOIN sys.partition_parameters pp ON pp.function_id = pf.function_id",
    ),
    (
        "partition schemes",
        "SELECT ps.name, pf.name,
                (SELECT STRING_AGG(ds.name, ',') WITHIN GROUP (ORDER BY dds.destination_id)
                   FROM sys.destination_data_spaces dds JOIN sys.data_spaces ds ON ds.data_space_id = dds.data_space_id
                  WHERE dds.partition_scheme_id = ps.data_space_id)
           FROM sys.partition_schemes ps JOIN sys.partition_functions pf ON pf.function_id = ps.function_id",
    ),
    (
        "sequences",
        // start_value aside: a used sequence is restarted at its current
        // value (said in the notes).
        "SELECT SCHEMA_NAME(schema_id) + '.' + name, TYPE_NAME(user_type_id), CAST(increment AS nvarchar(40)),
                CAST(minimum_value AS nvarchar(40)), CAST(maximum_value AS nvarchar(40)), is_cycling, is_cached, cache_size,
                CAST(current_value AS nvarchar(40)), CAST(last_used_value AS nvarchar(40))
           FROM sys.sequences",
    ),
    ("synonyms", "SELECT SCHEMA_NAME(schema_id) + '.' + name, base_object_name FROM sys.synonyms"),
    (
        "types",
        "SELECT SCHEMA_NAME(ty.schema_id) + '.' + ty.name, ty.is_table_type, TYPE_NAME(ty.system_type_id), ty.max_length, ty.is_nullable,
                (SELECT STRING_AGG(CAST(c.name + ':' + TYPE_NAME(c.user_type_id) + ':' + CAST(c.is_nullable AS varchar(1)) AS nvarchar(400)) COLLATE DATABASE_DEFAULT
                        + ':' + ISNULL(OBJECT_DEFINITION(c.default_object_id), '') COLLATE DATABASE_DEFAULT, ',') WITHIN GROUP (ORDER BY c.column_id)
                   FROM sys.table_types tt JOIN sys.columns c ON c.object_id = tt.type_table_object_id WHERE tt.user_type_id = ty.user_type_id),
                (SELECT STRING_AGG(CAST(i.type_desc AS nvarchar(100)) COLLATE DATABASE_DEFAULT + ':' + CAST(i.is_primary_key AS varchar(1)) + ':' + ISNULL(CASE WHEN i.is_primary_key = 1 THEN '' ELSE i.name END, '') COLLATE DATABASE_DEFAULT, ',')
                   FROM sys.table_types tt JOIN sys.indexes i ON i.object_id = tt.type_table_object_id WHERE tt.user_type_id = ty.user_type_id),
                (SELECT STRING_AGG(cc.definition, ',') FROM sys.table_types tt JOIN sys.check_constraints cc ON cc.parent_object_id = tt.type_table_object_id
                  WHERE tt.user_type_id = ty.user_type_id)
           FROM sys.types ty WHERE ty.is_user_defined = 1",
    ),
    ("xml collections", "SELECT SCHEMA_NAME(schema_id) + '.' + name FROM sys.xml_schema_collections WHERE xml_collection_id > 1"),
];

/// Columns the copy writes: no computed nor rowversion ones.
async fn insertable(s: &mut Box<dyn Session>, t: &ObjectRef) -> Vec<String> {
    let sql = format!(
        "SELECT name FROM sys.columns WHERE object_id = OBJECT_ID(N'[{}].[{}]') AND is_computed = 0 AND system_type_id <> 189 AND is_column_set = 0 ORDER BY column_id",
        t.schema.as_deref().unwrap(),
        t.name
    );
    run(s, &sql).await.results[0].rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

async fn run_all(s: &mut Box<dyn Session>, what: &str, stmts: &[String]) {
    for sql in stmts {
        if let Err(e) = try_run(s, sql).await {
            panic!("{what}: {e}\n{sql}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clone_everything() {
    use futures::FutureExt;
    let r = std::panic::AssertUnwindSafe(clone_and_compare()).catch_unwind().await;
    // The test databases go away whatever happened.
    let mut admin = driver().connect(&config(), Some("master")).await.expect("connect");
    drop_databases(&mut admin).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

async fn clone_and_compare() {
    let d = driver();
    assert!(d.supports_clone());
    let others: Vec<bool> = dbine_driver_sqlserver::drivers().iter().map(|d| d.supports_clone()).collect();
    assert_eq!(others, vec![true, true, false, false], "SQL Server, Azure SQL, Fabric, Babelfish");

    let mut admin = d.connect(&config(), Some("master")).await.expect("connect");
    drop_databases(&mut admin).await;
    run(&mut admin, &format!("CREATE DATABASE [{SRC_DB}]\nGO\nCREATE DATABASE [{DST_DB}]\nGO\nALTER DATABASE [{DST_DB}] SET RECOVERY SIMPLE")).await;

    let mut setup = d.connect(&config(), Some(SRC_DB)).await.unwrap();
    run(&mut setup, SOURCE_SQL).await;
    drop(setup);

    let tables: Vec<ObjectRef> = ["Customer", "OrderHeader", "OrderLine", "Fact", "FactArchive", "Stock", "Place", "Session", "Price", "Empty", "Log"]
        .iter()
        .map(|n| ObjectRef { kind: "table".into(), schema: Some("ven".into()), name: (*n).into() })
        .chain(std::iter::once(ObjectRef { kind: "table".into(), schema: Some("hist".into()), name: "PriceHistory".into() }))
        .collect();

    // The source through the app's read-only wrapper.
    let mut src: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some(SRC_DB)).await.unwrap()));
    let mut dst = d.connect(&config(), Some(DST_DB)).await.unwrap();
    let started = std::time::Instant::now();
    let script = d.clone_script(&mut *src, &mut *dst, &tables).await.expect("clone script");
    println!("clone script in {:?}: {} before, {} tables, {} after", started.elapsed(), script.before.len(), script.tables.len(), script.after.len());
    for n in &script.notes {
        println!("note: {n}");
    }
    let dump = std::env::var("DBINE_CLONE_DUMP").is_ok();
    if dump {
        for s in script.before.iter().chain(script.tables.iter().flat_map(|t| std::iter::once(&t.create).chain(t.after_data.iter()))).chain(script.after.iter()) {
            println!("----\n{s}");
        }
    }

    // Twice: the second run is a resumed clone (everything already there).
    for round in 0..2 {
        run_all(&mut dst, "before", &script.before).await;
        for t in &script.tables {
            run_all(&mut dst, "create", std::slice::from_ref(&t.create)).await;
            run_all(&mut dst, "before_data", &t.before_data).await;
            if round == 0 {
                let columns = insertable(&mut src, &t.table).await;
                let memory = run(&mut dst, &format!("SELECT OBJECTPROPERTY(OBJECT_ID(N'[{}].[{}]'), 'TableIsMemoryOptimized')", t.table.schema.as_deref().unwrap(), t.table.name))
                    .await
                    .results[0]
                    .rows[0][0]
                    == serde_json::json!(1);
                let spec = CopySpec {
                    source: ReadSpec { table: t.table.clone(), columns: Some(columns.clone()), filter: None },
                    target: LoadSpec {
                        table: t.table.clone(),
                        columns,
                        table_lock: !memory,
                        keep_identity: true,
                        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
                    },
                };
                d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await.unwrap_or_else(|e| panic!("copy {}: {e}", t.table.name));
            }
            run_all(&mut dst, "after_data", &t.after_data).await;
        }
        run_all(&mut dst, "after", &script.after).await;
    }

    // Compare.
    let mut a = d.connect(&config(), Some(SRC_DB)).await.unwrap();
    let mut b = d.connect(&config(), Some(DST_DB)).await.unwrap();
    let mut differences = Vec::new();
    for (what, sql) in COMPARE {
        let (x, y) = (rows(&mut a, sql).await, rows(&mut b, sql).await);
        println!("compared {what}: {} rows", x.len());
        for r in x.iter().filter(|r| !y.contains(r)) {
            differences.push(format!("{what}: solo en el origen: {r}"));
        }
        for r in y.iter().filter(|r| !x.contains(r)) {
            differences.push(format!("{what}: solo en el destino: {r}"));
        }
    }
    // Rows, each table (EXCEPT both ways over what the copy moved).
    for t in &tables {
        let name = format!("[{}].[{}]", t.schema.as_deref().unwrap(), t.name);
        let cols = insertable(&mut a, t).await.iter().map(|c| format!("[{c}]")).collect::<Vec<_>>().join(", ");
        // xml / spatial compare as text.
        let cols = cols
            .split(", ")
            .map(|c| match c {
                "[Doc]" | "[Body]" => format!("CAST({c} AS nvarchar(max)) AS {c}"),
                "[Shape]" | "[Loc]" => format!("{c}.ToString() AS {c}"),
                _ => c.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {cols} FROM {name}");
        let (x, y) = (rows(&mut a, &sql).await, rows(&mut b, &sql).await);
        if x != y {
            differences.push(format!("rows: {name}: {} rows in the source, {} in the target, or different values", x.len(), y.len()));
        }
    }
    // The emptied identity table hands out the same next value.
    let next_src = rows(&mut a, "INSERT INTO ven.Empty (X) OUTPUT inserted.Id VALUES (9)").await;
    let next_dst = rows(&mut b, "INSERT INTO ven.Empty (X) OUTPUT inserted.Id VALUES (9)").await;
    if next_src != next_dst {
        differences.push(format!("identity: ven.Empty next value {next_src:?} (origen) vs {next_dst:?} (destino)"));
    }
    let seq = "SELECT NEXT VALUE FOR ven.Seq";
    let (sa, sb) = (rows(&mut a, seq).await, rows(&mut b, seq).await);
    if sa != sb {
        differences.push(format!("sequence: ven.Seq next value {sa:?} vs {sb:?}"));
    }
    drop((a, b, src, dst));

    for dif in &differences {
        println!("DIFF {dif}");
    }
    drop(admin);
    // Only what can't be kept: the used sequence's START WITH.
    assert_eq!(script.notes.len(), 1, "{:?}", script.notes);
    assert!(script.notes[0].contains("[ven].[Seq]"), "{:?}", script.notes);
    assert!(differences.is_empty(), "{} differences", differences.len());
}

// ---------------------------------------------------------------------------
// Edge cases: each one its own pair of databases, cloned (script → data →
// script again) and compared like above.
//
// cargo test -p dbine-driver-sqlserver --test clone -- --ignored clone_edge_cases --test-threads=1 --nocapture
// (ONLY=<name> runs one.)
// ---------------------------------------------------------------------------

fn qi(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

fn nlit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// Columns the copy writes, and the ones compared: no computed, rowversion,
/// column set, graph or ledger ones.
async fn copied_columns(s: &mut Box<dyn Session>, t: &ObjectRef) -> Result<Vec<String>, String> {
    let name = format!("{}.{}", qi(t.schema.as_deref().unwrap()), qi(&t.name));
    let sql = format!(
        "SELECT name FROM sys.columns WHERE object_id = OBJECT_ID({}) AND is_computed = 0 AND system_type_id <> 189 AND is_column_set = 0
            AND graph_type IS NULL AND generated_always_type NOT IN (7, 8, 9, 10) ORDER BY column_id",
        nlit(&name)
    );
    Ok(try_run(s, &sql).await?.results[0].rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect())
}

async fn drop_named(admin: &mut Box<dyn Session>, dbs: &[&str]) {
    for db in dbs {
        let _ = try_run(admin, &format!("IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END")).await;
    }
}

struct Case {
    name: &'static str,
    sql: &'static str,
    tables: &'static [(&'static str, &'static str)],
    /// Extra queries compared on both sides.
    checks: &'static [(&'static str, &'static str)],
    /// Tables left out of the clone (said in the notes); nothing compared.
    left_out: bool,
    /// Texts that must all be in the notes.
    notes: &'static [&'static str],
    /// Objects left out of the clone (said in the notes): their rows are
    /// only on the source (or differ on the target), so they aren't compared.
    skipped: &'static [&'static str],
}

/// Errors and differences of one case (empty: identical).
async fn run_case(c: &Case) -> Vec<String> {
    let d = driver();
    let (sdb, ddb) = (format!("dbine_cedge_{}_src", c.name), format!("dbine_cedge_{}_dst", c.name));
    let mut admin = d.connect(&config(), Some("master")).await.expect("connect");
    drop_named(&mut admin, &[&sdb, &ddb]).await;
    let r = async {
        try_run(&mut admin, &format!("CREATE DATABASE [{sdb}]\nGO\nCREATE DATABASE [{ddb}]\nGO\nALTER DATABASE [{ddb}] SET RECOVERY SIMPLE\nGO\nALTER DATABASE [{sdb}] SET RECOVERY SIMPLE"))
            .await
            .map_err(|e| format!("databases: {e}"))?;
        let mut setup = d.connect(&config(), Some(&sdb)).await.map_err(|e| e.to_string())?;
        try_run(&mut setup, c.sql).await.map_err(|e| format!("source: {e}"))?;
        drop(setup);
        if let Some((_, sql)) = TARGET_SQL.iter().find(|(n, _)| *n == c.name) {
            let mut setup = d.connect(&config(), Some(&ddb)).await.map_err(|e| e.to_string())?;
            try_run(&mut setup, sql).await.map_err(|e| format!("target: {e}"))?;
        }
        let tables: Vec<ObjectRef> = c.tables.iter().map(|(s, n)| ObjectRef { kind: "table".into(), schema: Some((*s).into()), name: (*n).into() }).collect();
        let mut src: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some(&sdb)).await.map_err(|e| e.to_string())?));
        let mut dst = d.connect(&config(), Some(&ddb)).await.map_err(|e| e.to_string())?;
        let script = d.clone_script(&mut *src, &mut *dst, &tables).await.map_err(|e| format!("clone_script: {e}"))?;
        let mut errs = Vec::new();
        for n in &script.notes {
            println!("[{}] note: {n}", c.name);
        }
        for want in c.notes {
            if !script.notes.iter().any(|n| n.contains(want)) {
                errs.push(format!("missing note «{want}»"));
            }
        }
        if c.left_out && !script.tables.is_empty() {
            errs.push(format!("{} table(s) cloned, expected none", script.tables.len()));
        }
        for round in 0..2 {
            for s in &script.before {
                if let Err(e) = try_run(&mut dst, s).await {
                    errs.push(format!("round {round} before: {e}\n{s}"));
                }
            }
            for t in &script.tables {
                if let Err(e) = try_run(&mut dst, &t.create).await {
                    errs.push(format!("round {round} create {}: {e}\n{}", t.table.name, t.create));
                    continue;
                }
                if round == 0 {
                    let columns = copied_columns(&mut src, &t.table).await?;
                    let spec = CopySpec {
                        source: ReadSpec { table: t.table.clone(), columns: Some(columns.clone()), filter: None },
                        target: LoadSpec {
                            table: t.table.clone(),
                            columns,
                            table_lock: true,
                            keep_identity: true,
                            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
                        },
                    };
                    if let Err(e) = d.copy_native(&mut *src, &mut *dst, &spec, &|_| {}).await {
                        errs.push(format!("copy {}: {e}", t.table.name));
                    }
                }
                for s in &t.after_data {
                    if let Err(e) = try_run(&mut dst, s).await {
                        errs.push(format!("round {round} after_data {}: {e}\n{s}", t.table.name));
                    }
                }
            }
            for s in &script.after {
                if let Err(e) = try_run(&mut dst, s).await {
                    errs.push(format!("round {round} after: {e}\n{s}"));
                }
            }
        }
        if c.left_out {
            return Ok(errs);
        }
        let mut a = d.connect(&config(), Some(&sdb)).await.map_err(|e| e.to_string())?;
        let mut b = d.connect(&config(), Some(&ddb)).await.map_err(|e| e.to_string())?;
        for (what, sql) in COMPARE.iter().chain(c.checks.iter()) {
            let (x, y) = (rows(&mut a, sql).await, rows(&mut b, sql).await);
            for r in x.iter().filter(|r| !y.contains(r) && !c.skipped.iter().any(|k| r.contains(k))) {
                errs.push(format!("{what}: solo en el origen: {r}"));
            }
            for r in y.iter().filter(|r| !x.contains(r) && !c.skipped.iter().any(|k| r.contains(k))) {
                errs.push(format!("{what}: solo en el destino: {r}"));
            }
        }
        for t in &tables {
            let name = format!("{}.{}", qi(t.schema.as_deref().unwrap()), qi(&t.name));
            let cols = copied_columns(&mut a, t).await?.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");
            let sql = format!("SELECT {cols} FROM {name}");
            if rows(&mut a, &sql).await != rows(&mut b, &sql).await {
                errs.push(format!("rows: {name}"));
            }
        }
        Ok::<_, String>(errs)
    }
    .await;
    let mut admin = d.connect(&config(), Some("master")).await.expect("connect");
    drop_named(&mut admin, &[&sdb, &ddb]).await;
    r.unwrap_or_else(|e| vec![format!("FATAL {e}")])
}

const OBJECTS: (&str, &str) =
    ("objects", "SELECT SCHEMA_NAME(schema_id) + '.' + name, type FROM sys.objects WHERE is_ms_shipped = 0 AND type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR')");

const CASES: &[Case] = &[
    // Names that need every kind of quoting; a filtered unique index, a
    // disabled untrusted FK, a disabled trigger, a cycling sequence, an
    // indexed view with user statistics, a temporal table with a HIDDEN
    // period and retention.
    Case {
        name: "names",
        sql: r#"
CREATE SCHEMA [sch ]]é'x]
GO
CREATE SEQUENCE [sch ]]é'x].[Seq]]ç'] AS int START WITH 3 INCREMENT BY 1 MINVALUE 1 MAXVALUE 5 CYCLE NO CACHE
GO
DECLARE @i int = 0, @x int; WHILE @i < 7 BEGIN SET @x = NEXT VALUE FOR [sch ]]é'x].[Seq]]ç']; SET @i += 1; END
GO
CREATE TABLE [sch ]]é'x].[Pár]]ent 'P'] (
  [I d]] ñ] int NOT NULL CONSTRAINT [PK ]]p'] PRIMARY KEY,
  [Có'de] nvarchar(20) NULL,
  [Del]] ] bit NOT NULL CONSTRAINT [DF ]]d'] DEFAULT 0
)
GO
CREATE UNIQUE INDEX [UX ]]code' filt] ON [sch ]]é'x].[Pár]]ent 'P'] ([Có'de]) WHERE [Có'de] IS NOT NULL AND [Del]] ] = 0
GO
INSERT INTO [sch ]]é'x].[Pár]]ent 'P'] VALUES (1, N'a', 0), (2, NULL, 0), (3, NULL, 1), (4, N'a', 1)
GO
CREATE TABLE [sch ]]é'x].[Chi]]ld 'C'] (
  [Id] int IDENTITY(10, 10) NOT NULL CONSTRAINT [PK ]]c'] PRIMARY KEY,
  [P ]]id'] int NULL CONSTRAINT [CK ]]p'] CHECK ([P ]]id'] > 0)
)
GO
INSERT INTO [sch ]]é'x].[Chi]]ld 'C'] ([P ]]id']) VALUES (1), (2), (99)
GO
ALTER TABLE [sch ]]é'x].[Chi]]ld 'C'] WITH NOCHECK ADD CONSTRAINT [FK ]]x'] FOREIGN KEY ([P ]]id']) REFERENCES [sch ]]é'x].[Pár]]ent 'P'] ([I d]] ñ])
GO
ALTER TABLE [sch ]]é'x].[Chi]]ld 'C'] NOCHECK CONSTRAINT [FK ]]x']
GO
CREATE TRIGGER [sch ]]é'x].[tr ]]off'] ON [sch ]]é'x].[Chi]]ld 'C'] AFTER INSERT AS SET NOCOUNT ON
GO
DISABLE TRIGGER [sch ]]é'x].[tr ]]off'] ON [sch ]]é'x].[Chi]]ld 'C']
GO
CREATE VIEW [sch ]]é'x].[v ]]agg'] WITH SCHEMABINDING AS SELECT [P ]]id'], COUNT_BIG(*) AS n FROM [sch ]]é'x].[Chi]]ld 'C'] GROUP BY [P ]]id']
GO
CREATE UNIQUE CLUSTERED INDEX [CIX ]]v'] ON [sch ]]é'x].[v ]]agg'] ([P ]]id'])
GO
CREATE STATISTICS [ST ]]v'] ON [sch ]]é'x].[v ]]agg'] (n)
GO
CREATE TABLE [sch ]]é'x].[Pri]]ce 'T'] (
  Id int NOT NULL CONSTRAINT [PK ]]t'] PRIMARY KEY,
  Amount int NOT NULL,
  [Valid ]]From'] datetime2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL,
  [Valid ]]To'] datetime2 GENERATED ALWAYS AS ROW END NOT NULL,
  PERIOD FOR SYSTEM_TIME ([Valid ]]From'], [Valid ]]To'])
) WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = [sch ]]é'x].[Pri]]ce 'H'], HISTORY_RETENTION_PERIOD = 3 DAYS))
GO
INSERT INTO [sch ]]é'x].[Pri]]ce 'T'] (Id, Amount) VALUES (1, 1), (2, 2)
GO
UPDATE [sch ]]é'x].[Pri]]ce 'T'] SET Amount = Amount + 1
GO
EXEC sys.sp_addextendedproperty @name = N'Desc ]''x', @value = N'ñ', @level0type = N'SCHEMA', @level0name = N'sch ]é''x', @level1type = N'TABLE', @level1name = N'Pár]ent ''P''', @level2type = N'COLUMN', @level2name = N'I d] ñ'
"#,
        tables: &[("sch ]é'x", "Pár]ent 'P'"), ("sch ]é'x", "Chi]ld 'C'"), ("sch ]é'x", "Pri]ce 'T'"), ("sch ]é'x", "Pri]ce 'H'")],
        checks: &[
            ("seq next", "SELECT NEXT VALUE FOR [sch ]]é'x].[Seq]]ç']"),
            ("ident next", "INSERT INTO [sch ]]é'x].[Chi]]ld 'C'] ([P ]]id']) OUTPUT inserted.Id VALUES (NULL)"),
            (
                "filtered unique enforced",
                "BEGIN TRY INSERT INTO [sch ]]é'x].[Pár]]ent 'P'] VALUES (9, N'a', 0); SELECT 'accepted' END TRY BEGIN CATCH SELECT 'rejected' END CATCH",
            ),
        ],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // sp_rename and ALTER SCHEMA TRANSFER leave the stored text with the
    // old name.
    Case {
        name: "renamed",
        sql: r#"
CREATE SCHEMA ven
GO
CREATE SCHEMA other
GO
CREATE TABLE ven.T (Id int PRIMARY KEY)
GO
CREATE VIEW ven.vOld AS SELECT Id FROM ven.T
GO
EXEC sp_rename 'ven.vOld', 'vNew'
GO
CREATE PROCEDURE ven.pMoved AS SELECT 1 AS one
GO
ALTER SCHEMA other TRANSFER ven.pMoved
GO
CREATE FUNCTION ven.fOld () RETURNS int AS BEGIN RETURN 1 END
GO
ALTER SCHEMA other TRANSFER ven.fOld
GO
EXEC sp_rename 'other.fOld', 'fNew'
GO
CREATE TRIGGER ven.trOld ON ven.T AFTER INSERT AS SET NOCOUNT ON
GO
EXEC sp_rename 'ven.trOld', 'trNew'
"#,
        tables: &[("ven", "T")],
        checks: &[OBJECTS],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // CREATEs with no schema, by a user whose default schema isn't dbo.
    Case {
        name: "unqualified",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
CREATE USER cedge_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cedge_u
GO
EXECUTE AS USER = 'cedge_u'
GO
CREATE VIEW vT AS SELECT Id FROM T
GO
CREATE FUNCTION fT () RETURNS TABLE AS RETURN (SELECT Id FROM T)
GO
CREATE TRIGGER trT ON T AFTER INSERT AS SET NOCOUNT ON
GO
REVERT
"#,
        tables: &[("app", "T")],
        checks: &[OBJECTS, ("no helper user left", "SELECT name FROM sys.database_principals WHERE name LIKE 'dbine%'")],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // A schema-bound function over one table is another table's DEFAULT
    // (asked for before the table it reads); another reads its own table.
    Case {
        name: "earlyfn",
        sql: r#"
CREATE TABLE dbo.Cfg (K int PRIMARY KEY, V int NOT NULL)
GO
INSERT dbo.Cfg VALUES (1, 42)
GO
CREATE FUNCTION dbo.fnCfg() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT V FROM dbo.Cfg WHERE K = 1) END
GO
CREATE TABLE dbo.Item (Id int PRIMARY KEY, V int NOT NULL CONSTRAINT DF_Item_V DEFAULT (dbo.fnCfg()))
GO
INSERT dbo.Item (Id) VALUES (1)
GO
CREATE TABLE dbo.Own (Id int PRIMARY KEY, Prev int NULL)
GO
CREATE FUNCTION dbo.fnMax() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT MAX(Id) FROM dbo.Own) END
GO
ALTER TABLE dbo.Own ADD CONSTRAINT DF_Own_Prev DEFAULT (dbo.fnMax()) FOR Prev
GO
INSERT dbo.Own (Id) VALUES (1)
GO
INSERT dbo.Own (Id) VALUES (2)
"#,
        tables: &[("dbo", "Item"), ("dbo", "Own"), ("dbo", "Cfg")],
        checks: &[
            ("default works", "INSERT dbo.Item (Id) OUTPUT inserted.V VALUES (2)"),
            ("own default works", "INSERT dbo.Own (Id) OUTPUT inserted.Prev VALUES (3)"),
        ],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // A disabled primary key and unique constraint that foreign keys
    // reference (disabling them disabled those too).
    Case {
        name: "disabledkeys",
        sql: r#"
CREATE TABLE dbo.P (Id int NOT NULL CONSTRAINT PK_P PRIMARY KEY NONCLUSTERED, Code int NOT NULL CONSTRAINT UQ_P UNIQUE NONCLUSTERED)
GO
CREATE CLUSTERED INDEX CIX_P ON dbo.P (Code)
GO
INSERT dbo.P VALUES (1, 10), (2, 20)
GO
CREATE TABLE dbo.C (Id int NOT NULL, PId int NULL CONSTRAINT FK_C_P REFERENCES dbo.P (Id), PCode int NULL CONSTRAINT FK_C_Code REFERENCES dbo.P (Code))
GO
INSERT dbo.C VALUES (1, 1, 10)
GO
ALTER INDEX PK_P ON dbo.P DISABLE
GO
ALTER INDEX UQ_P ON dbo.P DISABLE
"#,
        tables: &[("dbo", "P"), ("dbo", "C")],
        checks: &[],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    Case {
        name: "graph",
        sql: r#"
CREATE TABLE dbo.Person (Id int PRIMARY KEY, Name nvarchar(50)) AS NODE
GO
CREATE TABLE dbo.Knows (Since int) AS EDGE
GO
INSERT dbo.Person VALUES (1, N'a'), (2, N'b')
GO
INSERT dbo.Knows VALUES ((SELECT $node_id FROM dbo.Person WHERE Id = 1), (SELECT $node_id FROM dbo.Person WHERE Id = 2), 2020)
"#,
        tables: &[("dbo", "Person"), ("dbo", "Knows")],
        checks: &[],
        left_out: true,
        skipped: &[],
        notes: &["[dbo].[Person]: es una tabla de grafo (AS NODE)", "[dbo].[Knows]: es una tabla de grafo (AS EDGE)"],
    },
    Case {
        name: "ledger",
        sql: r#"
CREATE TABLE dbo.L (Id int NOT NULL, V int NULL) WITH (LEDGER = ON (APPEND_ONLY = ON))
GO
INSERT dbo.L VALUES (1, 1), (2, 2)
"#,
        tables: &[("dbo", "L")],
        checks: &[],
        left_out: true,
        skipped: &[],
        notes: &["[dbo].[L]: es una tabla ledger"],
    },
    // Unqualified names in the body of schema-qualified modules, created by
    // a user whose default schema isn't dbo.
    Case {
        name: "bodyunq",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
INSERT app.T VALUES (1)
GO
CREATE USER cv2_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv2_u
GO
EXECUTE AS USER = 'cv2_u'
GO
CREATE VIEW app.vQ AS SELECT Id FROM T
GO
CREATE FUNCTION app.fQ() RETURNS TABLE AS RETURN SELECT Id FROM T
GO
REVERT
"#,
        tables: &[("app", "T")],
        checks: &[OBJECTS, HELPER, ("view works", "SELECT COUNT(*) FROM app.vQ")],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // An unqualified module WITH EXECUTE AS SELF would be bound to the
    // helper user (which then couldn't be dropped): left out, said.
    Case {
        name: "execself",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
CREATE USER cv2_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv2_u
GO
EXECUTE AS USER = 'cv2_u'
GO
CREATE PROCEDURE pS WITH EXECUTE AS SELF AS SELECT Id FROM T
GO
CREATE VIEW vOk AS SELECT Id FROM T
GO
REVERT
"#,
        tables: &[("app", "T")],
        checks: &[
            HELPER,
            ("objects but pS", "SELECT SCHEMA_NAME(schema_id) + '.' + name, type FROM sys.objects WHERE is_ms_shipped = 0 AND type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR') AND name <> 'pS'"),
            ("bound to a principal", "SELECT COUNT(*) FROM sys.sql_modules WHERE execute_as_principal_id > 0 AND OBJECT_NAME(object_id) <> 'pS'"),
        ],
        left_out: false,
        skipped: &["\"app.pS\""],
        notes: &["[app].[pS] (procedimiento): se declara WITH EXECUTE AS SELF y su CREATE no nombra esquema", "(en el origen es [cv2_u])"],
    },
    // A qualified one is created as the cloning user: said.
    Case {
        name: "execselfq",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
CREATE USER cv2_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv2_u
GO
EXECUTE AS USER = 'cv2_u'
GO
CREATE PROCEDURE app.pS WITH EXECUTE AS SELF AS SELECT Id FROM app.T
GO
REVERT
"#,
        tables: &[("app", "T")],
        checks: &[OBJECTS, HELPER],
        left_out: false,
        skipped: &[],
        notes: &["[app].[pS] (procedimiento): se declara WITH EXECUTE AS SELF, así que en el destino se ejecuta como el usuario con el que se clona (en el origen, como [cv2_u])"],
    },
    // WITH EXECUTE AS 'user' binds a module to a user, and users aren't
    // cloned: left out before any SQL (qualified or not) when the target
    // lacks the user, said with that cause (also by the view and the
    // DEFAULT that use such a function); dbo and OWNER come; no helper user
    // left behind.
    Case {
        name: "execnamed",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
INSERT app.T VALUES (1)
GO
CREATE USER cv3_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv3_u
GO
CREATE PROCEDURE app.pN WITH EXECUTE AS 'cv3_u' AS SELECT Id FROM app.T
GO
EXECUTE AS USER = 'cv3_u'
GO
CREATE PROCEDURE pNU WITH EXECUTE AS 'cv3_u' AS SELECT Id FROM T
GO
CREATE VIEW vOk AS SELECT Id FROM T
GO
REVERT
GO
CREATE PROCEDURE app.pD WITH EXECUTE AS 'dbo' AS SELECT Id FROM app.T
GO
CREATE PROCEDURE app.pO WITH EXECUTE AS OWNER AS SELECT Id FROM app.T
GO
CREATE FUNCTION app.fN() RETURNS int WITH EXECUTE AS 'cv3_u' AS BEGIN RETURN 7 END
GO
CREATE VIEW app.vF AS SELECT app.fN() AS x
GO
CREATE TABLE app.T2 (Id int PRIMARY KEY, DnoDF int NOT NULL CONSTRAINT DF_T2_D DEFAULT (app.fN()))
GO
INSERT app.T2 (Id) VALUES (1)
"#,
        tables: &[("app", "T"), ("app", "T2")],
        checks: &[
            HELPER,
            ("objects but pN, pNU, fN, vF", "SELECT SCHEMA_NAME(schema_id) + '.' + name, type FROM sys.objects WHERE is_ms_shipped = 0 AND type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR') AND name NOT IN ('pN', 'pNU', 'fN', 'vF')"),
            ("bound to a principal", "SELECT OBJECT_NAME(object_id), USER_NAME(execute_as_principal_id) FROM sys.sql_modules WHERE execute_as_principal_id IS NOT NULL AND OBJECT_NAME(object_id) NOT IN ('pN', 'pNU', 'fN')"),
        ],
        left_out: false,
        skipped: &["\"app.pN\"", "\"app.pNU\"", "\"app.fN\"", "\"app.vF\"", "DnoDF"],
        notes: &[
            "[app].[pN] (procedimiento): se declara WITH EXECUTE AS 'cv3_u' y no se clona: el usuario «cv3_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
            "[app].[pNU] (procedimiento): se declara WITH EXECUTE AS 'cv3_u' y no se clona: el usuario «cv3_u» de EXECUTE AS no existe en el destino",
            "[app].[vF] (vista): no se crea porque usa [app].[fN], que no se clona: el usuario «cv3_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
            "[app].[T2]: la columna [DnoDF] queda sin su DEFAULT porque usa [app].[fN], que no se clona: el usuario «cv3_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
        ],
    },
    // The same modules when the target has the user (created there before
    // cloning): all of them come, bound to it, and the DEFAULT stays.
    Case {
        name: "execexists",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
INSERT app.T VALUES (1)
GO
CREATE USER cv4_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv4_u
GO
CREATE PROCEDURE app.pN WITH EXECUTE AS 'cv4_u' AS SELECT Id FROM app.T
GO
EXECUTE AS USER = 'cv4_u'
GO
CREATE PROCEDURE pNU WITH EXECUTE AS 'cv4_u' AS SELECT Id FROM T
GO
REVERT
GO
CREATE PROCEDURE app.pD WITH EXECUTE AS 'dbo' AS SELECT Id FROM app.T
GO
CREATE FUNCTION app.fN() RETURNS int WITH EXECUTE AS 'cv4_u' AS BEGIN RETURN 7 END
GO
CREATE VIEW app.vF AS SELECT app.fN() AS x
GO
CREATE TABLE app.T2 (Id int PRIMARY KEY, D int NOT NULL CONSTRAINT DF_T2_D DEFAULT (app.fN()))
GO
INSERT app.T2 (Id) VALUES (1)
"#,
        tables: &[("app", "T"), ("app", "T2")],
        checks: &[
            OBJECTS,
            HELPER,
            ("bound to a principal", "SELECT OBJECT_NAME(object_id), USER_NAME(execute_as_principal_id) FROM sys.sql_modules WHERE execute_as_principal_id IS NOT NULL"),
            ("vF", "SELECT x FROM app.vF"),
            ("default", "BEGIN TRAN; INSERT app.T2 (Id) VALUES (2); SELECT D FROM app.T2 WHERE Id = 2; ROLLBACK"),
        ],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // Delta's untrusted-key mark is DBine's own bookkeeping: not cloned;
    // the key's other properties are.
    Case {
        name: "deltamark",
        sql: r#"
CREATE TABLE dbo.P (Id int PRIMARY KEY)
GO
CREATE TABLE dbo.C (Id int PRIMARY KEY, P int NULL CONSTRAINT FK_C_P REFERENCES dbo.P (Id))
GO
INSERT dbo.P VALUES (1)
GO
INSERT dbo.C VALUES (1, 1)
GO
EXEC sys.sp_addextendedproperty @name = N'dbine_delta_untrusted', @value = N'[dbo].[C]', @level0type = N'SCHEMA', @level0name = N'dbo', @level1type = N'TABLE', @level1name = N'C', @level2type = N'CONSTRAINT', @level2name = N'FK_C_P'
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'hijo', @level0type = N'SCHEMA', @level0name = N'dbo', @level1type = N'TABLE', @level1name = N'C', @level2type = N'CONSTRAINT', @level2name = N'FK_C_P'
"#,
        tables: &[("dbo", "P"), ("dbo", "C")],
        checks: &[(
            "no delta mark on the target",
            "SELECT 'mark on the target' FROM sys.extended_properties WHERE name = N'dbine_delta_untrusted' AND DB_NAME() LIKE N'%[_]dst'",
        )],
        left_out: false,
        skipped: &["dbine_delta_untrusted"],
        notes: &[],
    },
    // SELF on an unqualified trigger, on a function written with comments
    // and EXEC, and on a renamed procedure: all left out and said.
    Case {
        name: "selfvariants",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
CREATE TABLE app.Log (Id int)
GO
INSERT app.T VALUES (1)
GO
CREATE USER cv3_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv3_u
GO
EXECUTE AS USER = 'cv3_u'
GO
CREATE TRIGGER trS ON T WITH EXECUTE AS SELF AFTER INSERT AS INSERT Log SELECT Id FROM inserted
GO
CREATE FUNCTION fS() RETURNS int WITH /* x */ EXEC
  -- y
  AS self AS BEGIN RETURN (SELECT COUNT(*) FROM T) END
GO
CREATE PROCEDURE pOld WITH EXECUTE AS SELF AS SELECT Id FROM T
GO
CREATE PROCEDURE pOk AS SELECT Id FROM T
GO
REVERT
GO
EXEC sp_rename 'app.pOld', 'pNew'
"#,
        tables: &[("app", "T"), ("app", "Log")],
        checks: &[
            HELPER,
            ("objects but the SELF ones", "SELECT SCHEMA_NAME(schema_id) + '.' + name, type FROM sys.objects WHERE is_ms_shipped = 0 AND type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR') AND name NOT IN ('trS', 'fS', 'pNew')"),
            ("bound to a principal", "SELECT COUNT(*) FROM sys.sql_modules WHERE execute_as_principal_id > 0 AND OBJECT_NAME(object_id) NOT IN ('trS', 'fS', 'pNew')"),
        ],
        left_out: false,
        skipped: &["\"app.trS\"", "\"app.fS\"", "\"app.pNew\""],
        notes: &[
            "[app].[trS] (trigger): se declara WITH EXECUTE AS SELF y su CREATE no nombra esquema",
            "[app].[fS] (función): se declara WITH EXECUTE AS SELF y su CREATE no nombra esquema",
            "[app].[pNew] (procedimiento): se declara WITH EXECUTE AS SELF y su CREATE no nombra esquema",
        ],
    },
    // Unqualified views moved to another schema or renamed: their names
    // still resolve to what they do on the source.
    Case {
        name: "movedunq",
        sql: r#"
CREATE SCHEMA app
GO
CREATE SCHEMA other
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
CREATE TABLE other.T (Id int PRIMARY KEY)
GO
INSERT app.T VALUES (1)
GO
INSERT other.T VALUES (2)
GO
CREATE USER cv3_u WITHOUT LOGIN WITH DEFAULT_SCHEMA = app
GO
ALTER ROLE db_owner ADD MEMBER cv3_u
GO
EXECUTE AS USER = 'cv3_u'
GO
CREATE VIEW vX AS SELECT Id FROM T
GO
CREATE VIEW vR AS SELECT Id FROM T
GO
REVERT
GO
ALTER SCHEMA other TRANSFER app.vX
GO
EXEC sp_rename 'app.vR', 'vR2'
"#,
        tables: &[("app", "T"), ("other", "T")],
        checks: &[
            OBJECTS,
            HELPER,
            ("vX", "SELECT 'vX', Id FROM other.vX"),
            ("vR2", "SELECT 'vR2', Id FROM app.vR2"),
            ("definitions", "SELECT OBJECT_SCHEMA_NAME(object_id) + '.' + OBJECT_NAME(object_id), definition FROM sys.sql_modules"),
        ],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // Schema-bound functions in a computed column, a CHECK and a DEFAULT,
    // one reading a schema-bound view.
    Case {
        name: "computedfn",
        sql: r#"
CREATE TABLE dbo.Rate (K int PRIMARY KEY, R int NOT NULL)
GO
INSERT dbo.Rate VALUES (1, 3)
GO
CREATE VIEW dbo.vRate WITH SCHEMABINDING AS SELECT K, R FROM dbo.Rate
GO
CREATE FUNCTION dbo.fnRate() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT R FROM dbo.vRate WHERE K = 1) END
GO
CREATE FUNCTION dbo.fnOk(@q int) RETURNS bit WITH SCHEMABINDING AS BEGIN RETURN CASE WHEN @q <= (SELECT MAX(R) FROM dbo.Rate) * 10 THEN 1 ELSE 0 END END
GO
CREATE TABLE dbo.Ord (Id int PRIMARY KEY, Qty int NOT NULL CONSTRAINT CK_Ord_Qty CHECK (dbo.fnOk(Qty) = 1), Total AS (Qty * dbo.fnRate()), D int NOT NULL CONSTRAINT DF_Ord_D DEFAULT (dbo.fnRate()))
GO
INSERT dbo.Ord (Id, Qty) VALUES (1, 2)
"#,
        tables: &[("dbo", "Ord"), ("dbo", "Rate")],
        checks: &[
            OBJECTS,
            ("computed", "SELECT Id, Total, D FROM dbo.Ord"),
            ("check enforced", "BEGIN TRY INSERT dbo.Ord (Id, Qty) VALUES (9, 999) SELECT 'accepted' END TRY BEGIN CATCH SELECT 'rejected' END CATCH"),
        ],
        left_out: false,
        skipped: &[],
        notes: &[],
    },
    // The user a module's text names was renamed on the source afterwards:
    // the text (what the CREATE runs) still says the old name, so it's left
    // out saying so, even though the target has the new one; a text that
    // spells the current name in another case comes, bound to it.
    Case {
        name: "execrenamed",
        sql: r#"
CREATE SCHEMA app
GO
CREATE TABLE app.T (Id int PRIMARY KEY)
GO
INSERT app.T VALUES (1)
GO
CREATE USER cv6_old WITHOUT LOGIN
GO
CREATE PROCEDURE app.pR WITH EXECUTE AS 'cv6_old' AS SELECT Id FROM app.T
GO
ALTER USER cv6_old WITH NAME = cv6_new
GO
CREATE PROCEDURE app.pC WITH EXECUTE AS 'CV6_NEW' AS SELECT Id FROM app.T
"#,
        tables: &[("app", "T")],
        checks: &[
            OBJECTS,
            HELPER,
            ("bound to a principal", "SELECT OBJECT_NAME(object_id), USER_NAME(execute_as_principal_id) FROM sys.sql_modules WHERE execute_as_principal_id IS NOT NULL AND OBJECT_NAME(object_id) <> 'pR'"),
        ],
        left_out: false,
        skipped: &["\"app.pR\""],
        notes: &["[app].[pR] (procedimiento): se declara WITH EXECUTE AS 'cv6_old' y no se clona: su texto nombra al usuario «cv6_old», \
                  que en el origen ahora se llama «cv6_new»; actualizá el módulo en el origen para que nombre a «cv6_new» y volvé a clonar"],
    },
    // A case-insensitive source whose module spells its user otherwise
    // than the catalog, cloned to a case-sensitive target that has the
    // catalog's spelling: left out saying so (the CREATE would fail). And a
    // DEFAULT over a function left out because it calls one left out for
    // its user: the note names both without chaining.
    Case {
        name: "execcase",
        sql: r#"
CREATE USER cv7_u WITHOUT LOGIN
GO
CREATE USER cv8_u WITHOUT LOGIN
GO
CREATE TABLE dbo.T (Id int PRIMARY KEY)
GO
INSERT dbo.T VALUES (1)
GO
CREATE PROCEDURE dbo.pC WITH EXECUTE AS 'CV7_U' AS SELECT Id FROM dbo.T
GO
CREATE FUNCTION dbo.f() RETURNS int WITH EXECUTE AS 'cv8_u' AS BEGIN RETURN 7 END
GO
CREATE FUNCTION dbo.g() RETURNS int AS BEGIN RETURN dbo.f() END
GO
CREATE TABLE dbo.T2 (Id int PRIMARY KEY, DnoDF int NOT NULL CONSTRAINT DF_T2_DnoDF DEFAULT (dbo.g()))
GO
INSERT dbo.T2 (Id) VALUES (1)
"#,
        tables: &[("dbo", "T"), ("dbo", "T2")],
        checks: &[OBJECTS, HELPER],
        left_out: false,
        skipped: &["\"dbo.pC\"", "\"dbo.f\"", "\"dbo.g\"", "DnoDF"],
        notes: &[
            "[dbo].[pC] (procedimiento): se declara WITH EXECUTE AS 'CV7_U' y no se clona: el usuario «CV7_U» de EXECUTE AS no existe en el destino, \
             que distingue mayúsculas y minúsculas (ahí está «cv7_u»)",
            "[dbo].[T2]: la columna [DnoDF] queda sin su DEFAULT porque usa [dbo].[g], que depende de [dbo].[f], y [dbo].[f] no se clona: \
             el usuario «cv8_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
        ],
    },
    // A CHECK over a function left out for its EXECUTE AS user: the
    // constraint is left out saying why. And a DEFAULT over a function left
    // out both for calling one left out for its user and for reading a
    // table that isn't cloned: the note says both causes.
    Case {
        name: "execcheck",
        sql: r#"
CREATE USER cv9_u WITHOUT LOGIN
GO
CREATE USER cv11_u WITHOUT LOGIN
GO
CREATE FUNCTION dbo.fC(@x int) RETURNS bit WITH EXECUTE AS 'cv9_u' AS BEGIN RETURN 1 END
GO
CREATE TABLE dbo.T (Id int PRIMARY KEY CONSTRAINT CK_T CHECK (dbo.fC(Id) = 1))
GO
INSERT dbo.T VALUES (1)
GO
CREATE TABLE dbo.Other (Id int PRIMARY KEY)
GO
CREATE FUNCTION dbo.f() RETURNS int WITH EXECUTE AS 'cv11_u' AS BEGIN RETURN 7 END
GO
CREATE FUNCTION dbo.g() RETURNS int AS BEGIN RETURN dbo.f() + (SELECT COUNT(*) FROM dbo.Other) END
GO
CREATE TABLE dbo.T2 (Id int PRIMARY KEY, DgDF int NOT NULL CONSTRAINT DF_T2_DgDF DEFAULT (dbo.g()))
GO
INSERT dbo.T2 (Id) VALUES (1)
"#,
        tables: &[("dbo", "T"), ("dbo", "T2")],
        checks: &[OBJECTS, HELPER],
        left_out: false,
        skipped: &["\"dbo.fC\"", "\"dbo.f\"", "\"dbo.g\"", "\"dbo.Other\"", "CK_T", "DgDF"],
        notes: &[
            "[dbo].[T]: la restricción CHECK [CK_T] no se crea porque usa [dbo].[fC], que no se clona: \
             el usuario «cv9_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
            "[dbo].[T2]: la columna [DgDF] queda sin su DEFAULT porque usa [dbo].[g], que depende de [dbo].[f], y [dbo].[f] no se clona: \
             el usuario «cv11_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar; \
             además, [dbo].[g] usa tablas u objetos que no se clonan",
        ],
    },
];

/// SQL run on a case's target database before cloning (by case name).
const TARGET_SQL: &[(&str, &str)] = &[
    ("execexists", "CREATE USER cv4_u WITHOUT LOGIN"),
    ("execrenamed", "CREATE USER cv6_new WITHOUT LOGIN"),
    ("execcase", "ALTER DATABASE CURRENT COLLATE Latin1_General_CS_AS\nGO\nCREATE USER cv7_u WITHOUT LOGIN"),
];

/// A computed column over a function left out for its EXECUTE AS user:
/// the clone is refused, saying that cause.
const REFUSED: Case = Case {
    name: "execcomputed",
    sql: r#"
CREATE USER cv5_u WITHOUT LOGIN
GO
CREATE FUNCTION dbo.fN() RETURNS int WITH EXECUTE AS 'cv5_u' AS BEGIN RETURN 7 END
GO
CREATE TABLE dbo.T (Id int PRIMARY KEY, X AS (Id + dbo.fN()))
"#,
    tables: &[("dbo", "T")],
    checks: &[],
    left_out: false,
    skipped: &[],
    notes: &[],
};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clone_refuses_computed_over_missing_execute_as_user() {
    let problems = run_case(&REFUSED).await;
    let want = "[dbo].[T]: la tabla no se puede crear igual porque la columna calculada [X] usa [dbo].[fN], que no se clona: \
                el usuario «cv5_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar";
    assert!(problems.len() == 1 && problems[0].starts_with("FATAL clone_script") && problems[0].contains(want), "{problems:#?}");
}

const HELPER: (&str, &str) = ("no helper user left", "SELECT name FROM sys.database_principals WHERE name LIKE N'dbine%'");

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clone_edge_cases() {
    let only = std::env::var("ONLY").ok();
    let mut failed = Vec::new();
    for c in CASES.iter().filter(|c| only.as_deref().is_none_or(|o| o == c.name)) {
        let problems = run_case(c).await;
        println!("===== {}: {} problem(s)", c.name, problems.len());
        for p in &problems {
            println!("  * {p}");
        }
        if !problems.is_empty() {
            failed.push(c.name);
        }
    }
    assert!(failed.is_empty(), "failed: {failed:?}");
}
