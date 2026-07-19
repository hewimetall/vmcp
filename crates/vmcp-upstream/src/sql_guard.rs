//! Defence-in-depth SQL guard for the postgres upstream.
//!
//! The upstream `@modelcontextprotocol/server-postgres` ships no SQL-level
//! filter, so a caller with no extra grants can `SELECT pg_read_file(...)`
//! and read arbitrary server files. The first line of defence is REVOKE on
//! the postgres role (`pg_read_server_files` etc.); this module is the
//! second line of defence — a parser-based pre-flight check on the SQL
//! string before it reaches the upstream.
//!
//! Policy:
//! - Parse with `PostgreSqlDialect`. On parse failure, return Ok(()) — we
//!   are not the only defence, and surprising the user with a 500-style
//!   error on every malformed query would be worse than letting the upstream
//!   produce its real SQL error. The parse failure is logged at WARN.
//! - Any `COPY` statement is denied (file/program I/O, silent-drop risk).
//! - Any call to a denied function (file/large-object readers) is denied,
//!   regardless of where in the AST it appears — projection, WHERE, ORDER
//!   BY, JOIN, CTE, scalar subquery, table-valued from-clause, etc.
//!
//! The match is case-insensitive on the last identifier of the function's
//! qualified name (so `pg_catalog.pg_read_file` is still caught).

use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Statement, TableFactor, Visit, Visitor};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tracing::warn;

/// Functions that read server files, OS metadata, or large objects.
///
/// Kept lowercase for O(1) case-insensitive matching.
const FORBIDDEN_FUNCTIONS: &[&str] = &[
    // File reads.
    "pg_read_file",
    "pg_read_file_off_len",
    "pg_read_binary_file",
    // Directory listings.
    "pg_ls_dir",
    "pg_ls_logdir",
    "pg_ls_waldir",
    "pg_ls_archive_statusdir",
    "pg_ls_tmpdir",
    // Large object I/O.
    "lo_import",
    "lo_export",
    // OS-level metadata.
    "pg_stat_file",
    "pg_current_logfile",
];

/// Reason a SQL string was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// A denied function was referenced as a call (not just an identifier).
    ForbiddenFunction(String),
    /// Any `COPY` statement (TO/FROM/PROGRAM).
    Copy,
    /// Reserved for strict-mode (not used on the default fail-open path).
    #[allow(dead_code)]
    ParseFailure(String),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::ForbiddenFunction(name) => write!(
                f,
                "function '{name}' is not allowed (reads server files / OS metadata)"
            ),
            GuardError::Copy => write!(
                f,
                "COPY statements are not allowed (use SELECT/INSERT/UPDATE instead)"
            ),
            GuardError::ParseFailure(msg) => write!(f, "could not parse SQL: {msg}"),
        }
    }
}

impl std::error::Error for GuardError {}

/// Inspect a SQL string for denied constructs. Fail-open on parse error.
pub fn inspect(sql: &str) -> Result<(), GuardError> {
    let statements = match Parser::parse_sql(&PostgreSqlDialect {}, sql) {
        Ok(s) => s,
        Err(e) => {
            // The REVOKE on the postgres role is the real backstop. If we
            // can't parse, let the upstream return its own SQL error rather
            // than block a query the operator might want to actually run.
            warn!(error = %e, "SQL guard could not parse query; passing through");
            return Ok(());
        }
    };

    let mut guard = Guard;
    if let ControlFlow::Break(err) = statements.visit(&mut guard) {
        return Err(err);
    }

    // The Visitor we use catches every Expr::Function and TableFactor we
    // care about, but `Statement::Copy` is a node type the visitor doesn't
    // hook directly. Do an explicit pass for COPY at the statement level.
    for st in &statements {
        match st {
            Statement::Copy { .. } | Statement::CopyIntoSnowflake { .. } => {
                return Err(GuardError::Copy);
            }
            _ => {}
        }
    }

    Ok(())
}

/// Visitor that breaks with a `GuardError` as soon as a forbidden function
/// call is seen anywhere in the AST.
struct Guard;

impl Visitor for Guard {
    type Break = GuardError;

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if let Expr::Function(func) = expr {
            if let Some(name) = function_last_ident(&func.name) {
                if is_forbidden(name) {
                    return ControlFlow::Break(GuardError::ForbiddenFunction(name.to_string()));
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        match table_factor {
            // `SELECT * FROM pg_read_file('x') AS t(line text)` is parsed as
            // a Table with non-None `args`. A plain `FROM some_table` has
            // `args: None`, so a column named pg_read_file is not caught.
            TableFactor::Table {
                name,
                args: Some(_),
                ..
            } => {
                if let Some(ident) = function_last_ident(name) {
                    if is_forbidden(ident) {
                        return ControlFlow::Break(GuardError::ForbiddenFunction(
                            ident.to_string(),
                        ));
                    }
                }
            }
            TableFactor::Function { name, .. } => {
                if let Some(ident) = function_last_ident(name) {
                    if is_forbidden(ident) {
                        return ControlFlow::Break(GuardError::ForbiddenFunction(
                            ident.to_string(),
                        ));
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// Last identifier of an `ObjectName`. Returns None if the final part is
/// not a plain identifier (e.g. dialect-specific function-in-name).
fn function_last_ident(name: &sqlparser::ast::ObjectName) -> Option<&str> {
    name.0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.as_str())
}

fn is_forbidden(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    FORBIDDEN_FUNCTIONS.iter().any(|f| *f == lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_fn(sql: &str) -> String {
        match inspect(sql) {
            Err(GuardError::ForbiddenFunction(n)) => n,
            other => panic!("expected ForbiddenFunction, got {other:?} for sql: {sql}"),
        }
    }

    fn err_copy(sql: &str) {
        match inspect(sql) {
            Err(GuardError::Copy) => {}
            other => panic!("expected Copy, got {other:?} for sql: {sql}"),
        }
    }

    fn ok(sql: &str) {
        match inspect(sql) {
            Ok(()) => {}
            other => panic!("expected Ok, got {other:?} for sql: {sql}"),
        }
    }

    // ---- ForbiddenFunction in projection ------------------------------

    #[test]
    fn blocks_pg_read_file_in_projection() {
        assert_eq!(
            err_fn("SELECT pg_read_file('postgresql.conf')"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_binary_file_in_projection() {
        assert_eq!(
            err_fn("SELECT pg_read_binary_file('/etc/passwd')"),
            "pg_read_binary_file"
        );
    }

    // ---- ForbiddenFunction in WHERE / ORDER BY / GROUP BY -------------

    #[test]
    fn blocks_pg_read_file_in_where() {
        assert_eq!(
            err_fn("SELECT 1 FROM employees WHERE pg_read_file('x') IS NOT NULL"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_file_in_order_by() {
        assert_eq!(
            err_fn("SELECT * FROM employees ORDER BY pg_read_file('x')"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_file_in_group_by() {
        assert_eq!(
            err_fn("SELECT 1 FROM employees GROUP BY pg_read_file('x')"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_file_in_having() {
        assert_eq!(
            err_fn("SELECT id FROM t GROUP BY id HAVING pg_read_file('x') IS NOT NULL"),
            "pg_read_file"
        );
    }

    // ---- ForbiddenFunction in table-valued FROM ----------------------

    #[test]
    fn blocks_pg_read_file_as_table_function() {
        assert_eq!(
            err_fn("SELECT * FROM pg_read_file('x') AS t(line text)"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_ls_dir_as_table_function() {
        assert_eq!(err_fn("SELECT * FROM pg_ls_dir('.')"), "pg_ls_dir");
    }

    // ---- ForbiddenFunction in subqueries / CTEs / derived tables -----

    #[test]
    fn blocks_pg_read_file_in_scalar_subquery() {
        assert_eq!(err_fn("SELECT (SELECT pg_read_file('x'))"), "pg_read_file");
    }

    #[test]
    fn blocks_pg_read_file_in_cte() {
        assert_eq!(
            err_fn("WITH t AS (SELECT pg_read_file('x') f) SELECT * FROM t"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_file_in_derived_table() {
        assert_eq!(
            err_fn("SELECT * FROM (VALUES (1)) v(c) WHERE c::text = pg_read_file('/etc/passwd')"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_file_in_insert_select() {
        assert_eq!(
            err_fn("INSERT INTO t SELECT pg_read_file('x')"),
            "pg_read_file"
        );
    }

    #[test]
    fn blocks_pg_read_file_in_join_on() {
        assert_eq!(
            err_fn("SELECT * FROM a JOIN b ON a.id = b.id AND pg_read_file('x') IS NOT NULL"),
            "pg_read_file"
        );
    }

    // ---- All forbidden names -----------------------------------------

    #[test]
    fn blocks_pg_ls_dir() {
        assert_eq!(err_fn("SELECT pg_ls_dir('.')"), "pg_ls_dir");
    }

    #[test]
    fn blocks_pg_ls_logdir() {
        assert_eq!(err_fn("SELECT pg_ls_logdir()"), "pg_ls_logdir");
    }

    #[test]
    fn blocks_pg_ls_waldir() {
        assert_eq!(err_fn("SELECT pg_ls_waldir()"), "pg_ls_waldir");
    }

    #[test]
    fn blocks_lo_export() {
        assert_eq!(err_fn("SELECT lo_export(123, '/tmp/x')"), "lo_export");
    }

    #[test]
    fn blocks_lo_import() {
        assert_eq!(err_fn("SELECT lo_import('/tmp/x')"), "lo_import");
    }

    #[test]
    fn blocks_pg_stat_file() {
        assert_eq!(
            err_fn("SELECT pg_stat_file('postgresql.conf')"),
            "pg_stat_file"
        );
    }

    #[test]
    fn blocks_pg_current_logfile() {
        assert_eq!(err_fn("SELECT pg_current_logfile()"), "pg_current_logfile");
    }

    // ---- Case-insensitive matching -----------------------------------

    #[test]
    fn case_insensitive_upper() {
        assert_eq!(err_fn("SELECT PG_READ_FILE('x')"), "PG_READ_FILE");
    }

    #[test]
    fn case_insensitive_mixed() {
        assert_eq!(err_fn("SELECT Pg_Read_File('x')"), "Pg_Read_File");
    }

    // ---- Column references NOT caught --------------------------------

    #[test]
    fn allows_column_named_like_forbidden_function() {
        ok("SELECT pg_read_file FROM x");
    }

    #[test]
    fn allows_table_named_like_forbidden_function() {
        ok("SELECT * FROM pg_read_file");
    }

    // ---- COPY ---------------------------------------------------------

    #[test]
    fn blocks_copy_to_stdout() {
        err_copy("COPY employees TO STDOUT");
    }

    #[test]
    fn blocks_copy_query_to_file() {
        err_copy("COPY (SELECT 1) TO '/tmp/x.csv'");
    }

    #[test]
    fn blocks_copy_to_program() {
        err_copy("COPY employees TO PROGRAM 'cat > /tmp/p.csv'");
    }

    #[test]
    fn blocks_copy_from_stdin() {
        err_copy("COPY employees FROM STDIN");
    }

    // ---- Benign queries pass ----------------------------------------

    #[test]
    fn allows_simple_select() {
        ok("SELECT * FROM employees WHERE id = 1");
    }

    #[test]
    fn allows_join() {
        ok("SELECT a.id, b.name FROM a JOIN b ON a.id = b.id WHERE a.active");
    }

    #[test]
    fn allows_cte_without_forbidden() {
        ok("WITH t AS (SELECT id FROM employees) SELECT * FROM t");
    }

    #[test]
    fn allows_aggregate_functions() {
        ok("SELECT COUNT(*), SUM(salary), AVG(age) FROM employees");
    }

    // ---- Parse failure passes through (fail-open) --------------------

    #[test]
    fn parse_failure_passes_through() {
        ok("SELECT FROMM nonexistent");
    }

    #[test]
    fn empty_string_passes() {
        ok("");
    }

    #[test]
    fn garbled_passes() {
        ok("not even sql !!!! ??");
    }
}
