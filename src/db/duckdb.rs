//! The DuckDB boundary.
//!
//! Embedded like SQLite: no server, no credentials, a file or nothing at all.
//! What sets it apart is what it reads besides its own tables. A CSV, a
//! Parquet file, a JSON file and -- through `db::xlsx` -- a spreadsheet are
//! each a `FROM` away, which is what makes a profile with no file, held in
//! memory, a useful thing to have open.
//!
//! Results come back as Arrow batches and every cell is rendered by Arrow's
//! own formatter, so a `LIST`, a `STRUCT` or a `DECIMAL(18,3)` reads the way
//! DuckDB's own shell prints it rather than through a match on every type this
//! file would have to keep up with.
//!
//! Nothing here is editable in the grid yet: DuckDB does not say which table a
//! result column was read from, and a predicate dbdelve cannot read off the
//! grid is one it must not write.

use std::path::Path;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duckdb::arrow::array::Array;
use duckdb::arrow::datatypes::DataType;
use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
use duckdb::{Config, InterruptHandle};

use super::{
    Catalog, Cell, Column, DbError, Engine, QueryResult, Reference, Structure, assemble_catalog,
    assemble_foreign_keys, assemble_references, assemble_structure, home_expanded, plain_error,
    required_cell,
};
use crate::i18n::{tr, trf};

/// What an empty path means: a database held in memory, gone when the
/// connection closes. The files it reads stay where they are.
pub const IN_MEMORY: &str = ":memory:";

/// The path out of a `duckdb:` URL, the way `sqlite.rs` reads its own: all of
/// it after the scheme, less an optional `//`. Nothing after the scheme is a
/// database in memory.
pub fn path_from_url(url: &str) -> Result<String, String> {
    let rest = url.split_once(':').map_or("", |(_, rest)| rest);
    let path = rest.strip_prefix("//").unwrap_or(rest);
    let decoded = super::percent_decoded(path)?;
    Ok(match decoded.is_empty() {
        true => IN_MEMORY.to_string(),
        false => decoded,
    })
}

#[derive(Clone)]
pub struct Connection {
    connection: Arc<Mutex<duckdb::Connection>>,
    interrupt: Arc<InterruptHandle>,
    statement_timeout: Option<Duration>,
}

impl Connection {
    pub fn open(path: &str, statement_timeout: u32) -> Result<Self, DbError> {
        let path = home_expanded(path.trim());
        let connection = match path.as_str() {
            "" | IN_MEMORY => duckdb::Connection::open_in_memory(),
            path => {
                // Not created when missing, for the reason `sqlite.rs` gives:
                // a mistyped path would open as an empty database and read as
                // one with nothing in it.
                if !Path::new(path).exists() {
                    return Err(plain_error(trf!("No database file at {}", path)));
                }
                duckdb::Connection::open_with_flags(path, Config::default())
            }
        }
        .map_err(|error| plain_error(trf!("Cannot open {}: {}", path, error)))?;

        connection
            .register_table_function::<super::xlsx::ReadXlsx>("read_xlsx")
            .map_err(|error| plain_error(error.to_string()))?;

        Ok(Self {
            interrupt: connection.interrupt_handle(),
            statement_timeout: (statement_timeout > 0)
                .then(|| Duration::from_secs(u64::from(statement_timeout))),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Local and immediate, as SQLite's is: an interrupt on an idle connection
    /// does nothing.
    pub fn cancel(&self) -> Result<(), DbError> {
        self.interrupt.interrupt();
        Ok(())
    }

    /// The statement timeout, the way `sqlite.rs` arms its own: a thread that
    /// interrupts unless this is dropped first.
    fn deadline(&self) -> Option<Sender<()>> {
        let limit = self.statement_timeout?;
        let interrupt = self.interrupt.clone();
        let (sender, receiver) = channel::<()>();
        std::thread::spawn(move || {
            if receiver.recv_timeout(limit) == Err(RecvTimeoutError::Timeout) {
                interrupt.interrupt();
            }
        });
        Some(sender)
    }

    /// Run the submission verbatim and keep the last result set.
    ///
    /// DuckDB prepares one statement at a time. A submission of several is
    /// run whole as a batch, which reports no rows -- the editor's own
    /// statement splitting already sends a selection one statement at a time,
    /// so this is the path of a script, not of a query.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        let _deadline = self.deadline();
        let connection = self.connection.lock().map_err(|_| DbError {
            message: tr("The connection is unavailable after an earlier internal failure.").into(),
            position: None,
        })?;
        let started = Instant::now();

        let mut statement = match connection.prepare(sql) {
            Ok(statement) => statement,
            Err(error) if error.to_string().contains("multiple statements") => {
                connection.execute_batch(sql).map_err(query_error)?;
                return Ok(QueryResult {
                    elapsed: started.elapsed(),
                    ..QueryResult::default()
                });
            }
            Err(error) => return Err(query_error(error)),
        };

        let batches = statement.query_arrow([]).map_err(query_error)?;
        let schema = batches.get_schema();
        let columns: Vec<Column> = schema
            .fields()
            .iter()
            .map(|field| Column {
                name: field.name().clone(),
                data_type: Some(type_name(field.data_type())),
            })
            .collect();

        let options = FormatOptions::default().with_null("");
        let mut rows = Vec::new();
        let mut bytes = 0;
        for batch in batches {
            let formatters = batch
                .columns()
                .iter()
                .map(|array| ArrayFormatter::try_new(array.as_ref(), &options))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| plain_error(error.to_string()))?;
            for row in 0..batch.num_rows() {
                let cells: Vec<Cell> = batch
                    .columns()
                    .iter()
                    .zip(&formatters)
                    .map(|(array, formatter)| {
                        (!array.is_null(row)).then(|| formatter.value(row).to_string())
                    })
                    .collect();
                bytes += cells.iter().flatten().map(String::len).sum::<usize>();
                rows.push(cells);
            }
        }

        Ok(QueryResult {
            rows_affected: Some(rows.len() as u64),
            columns,
            rows,
            bytes,
            elapsed: started.elapsed(),
            ..QueryResult::default()
        })
    }

    /// The tables and views of the database the connection opened, less the
    /// catalogs DuckDB keeps for itself.
    pub fn catalog(&self) -> Result<Catalog, DbError> {
        let relations = self.query(
            "SELECT schema_name, table_name AS relation_name, 'table' AS relation_kind,
                    estimated_size AS row_estimate
             FROM duckdb_tables()
             WHERE NOT internal AND database_name = current_database()
             UNION ALL
             SELECT schema_name, view_name, 'view', NULL
             FROM duckdb_views()
             WHERE NOT internal AND database_name = current_database()
             ORDER BY schema_name, relation_name",
        )?;
        assemble_catalog(relations, QueryResult::default())
    }

    /// DuckDB's macros are functions of a kind, but not ones with a body to
    /// show the way a routine tab does; none are listed.
    pub fn routines(&self) -> Result<Catalog, DbError> {
        Ok(Catalog::default())
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        let (schema_literal, relation_literal) = (
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        );
        let columns = self.query(&format!(
            "SELECT column_name, lower(data_type) AS data_type,
                    CASE WHEN is_nullable THEN 'yes' ELSE 'no' END AS nullable,
                    COALESCE(column_default, '') AS column_default
             FROM duckdb_columns()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND table_name = {relation_literal}
             ORDER BY column_index"
        ))?;
        let indexes = self.query(&format!(
            "SELECT index_name AS object_name, COALESCE(sql, '') AS definition
             FROM duckdb_indexes()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND table_name = {relation_literal}
             ORDER BY index_name"
        ))?;
        // Written out of the column lists rather than taken from
        // `constraint_text`, which spells a key `PRIMARY KEY(id)`: the
        // Structure view and `Structure::primary_key` read the shape every
        // other engine gives them, `PRIMARY KEY (id)`.
        let constraints = self.query(&format!(
            "SELECT constraint_type || ' (' || array_to_string(constraint_column_names, ', ') || ')'
                        AS object_name,
                    constraint_type || ' (' || array_to_string(constraint_column_names, ', ') || ')'
                        || CASE WHEN constraint_type = 'FOREIGN KEY'
                                THEN ' REFERENCES ' || referenced_table || ' ('
                                     || array_to_string(referenced_column_names, ', ') || ')'
                                ELSE '' END
                        AS definition
             FROM duckdb_constraints()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND table_name = {relation_literal}
               AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY')
             ORDER BY constraint_index"
        ))?;
        let mut structure = assemble_structure(columns, indexes, constraints)?;
        structure.foreign_keys = assemble_foreign_keys(&self.query(&format!(
            "SELECT unnest(constraint_column_names) AS column_name,
                    schema_name AS referenced_schema,
                    referenced_table,
                    unnest(referenced_column_names) AS referenced_column
             FROM duckdb_constraints()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND table_name = {relation_literal}
               AND constraint_type = 'FOREIGN KEY'"
        ))?)?;
        Ok(structure)
    }

    /// Who points at this relation. A DuckDB foreign key names its parent by
    /// table alone, in its own schema.
    pub fn references(&self, schema: &str, relation: &str) -> Result<Vec<Reference>, DbError> {
        let result = self.query(&format!(
            "SELECT schema_name AS source_schema, table_name AS source_table,
                    unnest(constraint_column_names) AS column_name,
                    unnest(referenced_column_names) AS referenced_column,
                    CAST(constraint_index AS VARCHAR) AS constraint_name
             FROM duckdb_constraints()
             WHERE database_name = current_database()
               AND constraint_type = 'FOREIGN KEY'
               AND schema_name = {} AND referenced_table = {}",
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        ))?;
        assemble_references(&result)
    }

    pub fn ddl(&self, schema: &str, relation: &str) -> Result<String, DbError> {
        let (schema_literal, relation_literal) = (
            Engine::DuckDb.quote_literal(schema),
            Engine::DuckDb.quote_literal(relation),
        );
        let listed = self.query(&format!(
            "SELECT sql FROM duckdb_tables()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND table_name = {relation_literal}
             UNION ALL
             SELECT sql FROM duckdb_views()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND view_name = {relation_literal}
             UNION ALL
             SELECT sql FROM duckdb_indexes()
             WHERE database_name = current_database()
               AND schema_name = {schema_literal} AND table_name = {relation_literal}
               AND sql IS NOT NULL"
        ))?;
        let statements: Vec<String> = listed
            .rows
            .iter()
            .map(|row| required_cell(&listed, row, "sql").map(str::to_string))
            .collect::<Result<_, _>>()?;
        if statements.is_empty() {
            return Err(plain_error(trf!(
                "{} has no relation {}.",
                schema,
                relation
            )));
        }
        Ok(statements
            .iter()
            .map(|statement| statement.trim_end_matches(';').to_string())
            .collect::<Vec<_>>()
            .join(";\n")
            + ";")
    }
}

/// An Arrow type under the name DuckDB gives it, lowercased as every engine's
/// type names are in the grid, so `db::is_numeric_type` and the Structure view
/// read the same words DuckDB's own shell prints.
fn type_name(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "boolean".into(),
        DataType::Int8 => "tinyint".into(),
        DataType::Int16 => "smallint".into(),
        DataType::Int32 => "integer".into(),
        DataType::Int64 => "bigint".into(),
        DataType::UInt8 => "utinyint".into(),
        DataType::UInt16 => "usmallint".into(),
        DataType::UInt32 => "uinteger".into(),
        DataType::UInt64 => "ubigint".into(),
        DataType::Float16 | DataType::Float32 => "float".into(),
        DataType::Float64 => "double".into(),
        DataType::Decimal128(precision, scale) | DataType::Decimal256(precision, scale) => {
            format!("decimal({precision},{scale})")
        }
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "varchar".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "blob".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Time32(_) | DataType::Time64(_) => "time".into(),
        DataType::Timestamp(_, None) => "timestamp".into(),
        DataType::Timestamp(_, Some(_)) => "timestamp with time zone".into(),
        DataType::Interval(_) | DataType::Duration(_) => "interval".into(),
        DataType::List(item) | DataType::LargeList(item) => {
            format!("{}[]", type_name(item.data_type()))
        }
        DataType::Struct(_) => "struct".into(),
        DataType::Map(..) => "map".into(),
        DataType::Dictionary(_, value) => type_name(value),
        other => other.to_string().to_lowercase(),
    }
}

/// DuckDB's message as it wrote it. It carries no offset into the statement
/// that `DbError::position` could point the editor at; its `LINE 1:` excerpt
/// already shows where.
fn query_error(error: duckdb::Error) -> DbError {
    DbError {
        message: error.to_string(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures beside the other engines' seeds, by absolute path: a
    /// relative one would resolve against wherever the test runner stands.
    fn fixture(name: &str) -> String {
        format!("{}/dev/duckdb/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn memory() -> Connection {
        Connection::open(IN_MEMORY, 0).expect("an in-memory database opens")
    }

    #[test]
    fn a_csv_file_is_a_table_away() {
        let result = memory()
            .query(&format!(
                "SELECT nombre FROM read_csv('{}') WHERE ciudad = 'Londres'",
                fixture("clientes.csv")
            ))
            .unwrap();
        assert_eq!(result.columns[0].name, "nombre");
        assert_eq!(result.rows, vec![vec![Some("Ada Lovelace".to_string())]]);
    }

    #[test]
    fn a_spreadsheet_reads_by_its_header_with_numbers_as_numbers() {
        let connection = memory();
        let result = connection
            .query(&format!(
                "SELECT * FROM read_xlsx('{}')",
                fixture("facturas.xlsx")
            ))
            .unwrap();
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["numero", "cliente", "importe", "fecha", "pagada"]);
        assert_eq!(result.columns[2].data_type.as_deref(), Some("double"));
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][2].as_deref(), Some("1250.5"));
        assert_eq!(result.rows[0][3].as_deref(), Some("2026-01-03"));
        assert_eq!(result.rows[2][2], None);

        let february = connection
            .query(&format!(
                "SELECT numero FROM read_xlsx('{}', sheet := 'Febrero')",
                fixture("facturas.xlsx")
            ))
            .unwrap();
        assert_eq!(february.rows, vec![vec![Some("F-010".to_string())]]);
    }

    #[test]
    fn nested_and_exact_types_render_as_duckdb_prints_them() {
        let result = memory()
            .query("SELECT [1, 2] AS list, 12.50::DECIMAL(5,2) AS money, NULL AS nothing")
            .unwrap();
        assert_eq!(result.columns[1].data_type.as_deref(), Some("decimal(5,2)"));
        assert_eq!(
            result.rows[0],
            vec![Some("[1, 2]".into()), Some("12.50".into()), None]
        );
    }

    #[test]
    fn the_catalog_and_a_tables_structure_read_back() {
        let connection = memory();
        connection
            .query(
                "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre VARCHAR NOT NULL);
                 CREATE TABLE facturas (id INTEGER PRIMARY KEY,
                                        cliente INTEGER REFERENCES clientes(id));",
            )
            .unwrap();
        let catalog = connection.catalog().unwrap();
        let main = &catalog.schemas[0];
        assert_eq!(main.name, "main");
        let names: Vec<&str> = main.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["clientes", "facturas"]);

        let structure = connection.structure("main", "facturas").unwrap();
        assert_eq!(structure.primary_key(), ["id"]);
        assert_eq!(structure.foreign_keys[0].referenced_table, "clientes");
        let references = connection.references("main", "clientes").unwrap();
        assert_eq!(references[0].table, "facturas");
    }
}
