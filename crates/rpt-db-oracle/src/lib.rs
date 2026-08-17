//! # rpt-db-oracle — a live Oracle [`RowSource`]
//!
//! The Oracle side of the live-data path: given a report's decoded database schema and a
//! connection, it generates SQL via [`rpt_query`] (in [`Dialect::Oracle`]), executes it, and returns
//! a [`RowSource`] the [`rpt_data`] pipeline consumes exactly like the offline
//! `SavedDataSource` — the same seam `rpt-db-postgres` sits on.
//!
//! Two things make Oracle different from the other backends:
//!
//! * **The driver is async.** `oracle_rs` speaks the TNS wire protocol on tokio, while
//!   [`RowSource`] is synchronous. Each connection therefore owns a current-thread tokio runtime and
//!   blocks on it. No global runtime is installed, so an embedding application's own runtime is
//!   untouched — but for that reason these calls must not be made from inside an async context.
//! * **Text is locale-dependent.** Every column is fetched as `VARCHAR2` and re-typed downstream, and
//!   on Oracle a `DATE` rendered as text takes the session's `NLS_DATE_FORMAT`. A connection
//!   therefore pins the date, timestamp and numeric formats before running anything
//!   (see [`OracleConn::connect`]); without that the same query against the same data returns
//!   different text on two servers.
//!
//! ## Connection string
//! `oracle://user:password@host:port/service`. The port defaults to 1521 when omitted.

use rpt_data::{Cell, Column, Row, RowData, RowSource};
use rpt_model::Database;
use rpt_query::{build_query_full, Dialect, SqlQuery, Value as QueryValue};

/// A failure of the live Oracle path — the shared [`rpt_data::DbError`] aliased to this driver's
/// error type. Constructed through its `no_query` / `connect` / `query` helpers.
pub type DbError = rpt_data::DbError<Error>;

/// What can go wrong that is this driver's own fault rather than the database's.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The connection string is not of the form `oracle://user:password@host:port/service`.
    #[error("{0}")]
    ConnectionString(String),
    /// The tokio runtime the async driver needs could not be started.
    #[error("cannot start the async runtime the Oracle driver needs")]
    Runtime(#[source] std::io::Error),
    /// The driver itself failed.
    #[error(transparent)]
    Driver(#[from] oracle_rs::Error),
}

/// The session settings pinned on connect so a text-cast value does not depend on the server's
/// locale. `NLS_NUMERIC_CHARACTERS` fixes `.` as the decimal separator — on a comma-decimal server
/// a cast number would otherwise read back as text the re-typing cannot parse.
const SESSION_FORMATS: &[&str] = &[
    "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS'",
    "ALTER SESSION SET NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS'",
    "ALTER SESSION SET NLS_NUMERIC_CHARACTERS = '.,'",
];

/// How many rows one round trip fetches. The driver pages with `fetch_more`, so this trades round
/// trips against memory rather than capping the result.
const FETCH_SIZE: u32 = 1000;

/// The parts of a parsed connection string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dsn {
    /// EZConnect (`host:port/service`), the form the driver takes.
    connect_string: String,
    user: String,
    password: String,
}

/// Parse `oracle://user:password@host:port/service`.
///
/// Hand-rolled rather than pulling in a URL crate: the accepted shape is one line of structure, and
/// the password is deliberately not percent-decoded — decoding it wrongly would silently attempt a
/// connection with the wrong credentials.
fn parse_dsn(conn_str: &str) -> Result<Dsn, Error> {
    let bad = |what: &str| {
        Error::ConnectionString(format!(
            "{what} — expected oracle://user:password@host:port/service"
        ))
    };
    let rest = conn_str
        .strip_prefix("oracle://")
        .ok_or_else(|| bad("connection string must start with oracle://"))?;
    // Split on the LAST '@': a password may legitimately contain one, a host may not.
    let (credentials, target) = rest.rsplit_once('@').ok_or_else(|| bad("no '@' found"))?;
    let (user, password) = credentials
        .split_once(':')
        .ok_or_else(|| bad("no ':' between user and password"))?;
    let (host_port, service) = target
        .split_once('/')
        .ok_or_else(|| bad("no '/' before the service name"))?;
    if user.is_empty() || host_port.is_empty() || service.is_empty() {
        return Err(bad("user, host and service must all be present"));
    }
    let host_port = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:1521")
    };
    Ok(Dsn {
        connect_string: format!("{host_port}/{service}"),
        user: user.to_string(),
        password: password.to_string(),
    })
}

/// A [`RowSource`] backed by a live Oracle query over a report's linked tables.
#[derive(Debug, Clone)]
pub struct OracleSource(RowData);

impl RowSource for OracleSource {
    fn columns(&self) -> &[Column] {
        self.0.columns()
    }
    fn rows(&self) -> Vec<Row> {
        self.0.rows()
    }
}

impl OracleSource {
    /// Connect to `conn_str` and fetch the report's tables, joined per the link graph.
    ///
    /// `sql_exprs` are the report's SQL Expression fields (`(name, text)`), `selection` is the
    /// record-selection formula, `params` bind `{?Name}` current values, and `comment`, when set, is
    /// prepended as a `/* … */` tracking comment.
    ///
    /// The selection is **not** pushed into `WHERE` on Oracle — only the Postgres path translates
    /// predicates — so the full result set is fetched and the pipeline applies the formula per row.
    /// The rendered output is the same; the query just returns more rows than the report shows.
    ///
    /// # Errors
    ///
    /// - [`rpt_data::DbError::NoQuery`] — no query could be built (the report binds no table).
    /// - [`rpt_data::DbError::Connect`] — bad connection string, or the database was unreachable.
    /// - [`rpt_data::DbError::Query`] — the statement failed; the error carries the SQL.
    pub fn fetch(
        conn_str: &str,
        database: &Database,
        sql_exprs: &[(String, String)],
        selection: Option<&str>,
        params: &[(String, QueryValue)],
        comment: Option<&str>,
    ) -> Result<OracleSource, DbError> {
        let mut query = build_query_full(database, sql_exprs, selection, params, Dialect::Oracle)
            .map_err(|e| DbError::no_query(e.to_string()))?;
        if let Some(c) = comment {
            query = query.with_comment(c);
        }
        let mut conn = OracleConn::connect(conn_str)?;
        conn.run(&query)
    }
}

/// A live Oracle connection, split from the fetch so a caller can order the steps itself:
/// connect → [`ping`](Self::ping) healthcheck → build/log the SQL → [`run`](Self::run).
///
/// Owns the tokio runtime the async driver needs, so nothing outside this crate deals in futures.
pub struct OracleConn {
    runtime: tokio::runtime::Runtime,
    connection: oracle_rs::Connection,
    /// The server label used in logs and errors — host/service, never the credentials.
    label: String,
}

/// Hand-written because the driver's `Connection` is not `Debug`. It prints the server label only:
/// the credentials must not reach a log through a debug rendering.
impl std::fmt::Debug for OracleConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OracleConn")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl OracleConn {
    /// Open a connection and pin the session's date/number formats (see the module docs).
    ///
    /// # Errors
    ///
    /// [`rpt_data::DbError::Connect`] for a malformed connection string, a runtime that will not
    /// start, or a database that cannot be reached. [`rpt_data::DbError::Query`] if the session
    /// formats are refused.
    pub fn connect(conn_str: &str) -> Result<OracleConn, DbError> {
        let dsn = parse_dsn(conn_str).map_err(DbError::connect)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DbError::connect(Error::Runtime(e)))?;
        let connection = runtime
            .block_on(oracle_rs::Connection::connect(
                &dsn.connect_string,
                &dsn.user,
                &dsn.password,
            ))
            .map_err(|e| DbError::connect(Error::Driver(e)))?;
        let conn = OracleConn {
            runtime,
            connection,
            label: dsn.connect_string,
        };
        for stmt in SESSION_FORMATS {
            conn.runtime
                .block_on(conn.connection.query(stmt, &[]))
                .map_err(|e| DbError::query(Error::Driver(e)))?;
        }
        Ok(conn)
    }

    /// The server this connection is to, for logs and errors. Host and service only — never the
    /// credentials the connection string carried.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// A cheap round trip that proves the connection is usable before a real query is built.
    ///
    /// # Errors
    ///
    /// [`rpt_data::DbError::Query`] if the round trip fails.
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.runtime
            .block_on(self.connection.query("SELECT 1 FROM DUAL", &[]))
            .map_err(|e| DbError::query(Error::Driver(e)))?;
        Ok(())
    }

    /// Run a built query and collect every row, paging until the server reports no more.
    ///
    /// # Errors
    ///
    /// [`rpt_data::DbError::Query`] if the statement or a subsequent fetch fails; the error carries
    /// the SQL.
    pub fn run(&mut self, query: &SqlQuery) -> Result<OracleSource, DbError> {
        // The server and the failing statement are attached once, here, so the inner sites stay
        // terse and none of them has to remember the context.
        self.collect(query)
            .map_err(|e| e.in_context(Some(&self.label), Some(&query.sql)))
    }

    /// [`run`](Self::run) without the error context.
    fn collect(&mut self, query: &SqlQuery) -> Result<OracleSource, DbError> {
        let fail = |e: oracle_rs::Error| DbError::query(Error::Driver(e));
        let mut result = self
            .runtime
            .block_on(self.connection.query(&query.sql, &[]))
            .map_err(fail)?;

        let mut cells: Vec<Vec<Option<Cell>>> = Vec::new();
        loop {
            let width = result.columns.len();
            for row in &result.rows {
                cells.push((0..width).map(|i| cell_of(row.get(i))).collect());
            }
            if !result.has_more_rows {
                break;
            }
            result = self
                .runtime
                .block_on(
                    self.connection
                        .fetch_more(result.cursor_id, &result.columns, FETCH_SIZE),
                )
                .map_err(fail)?;
        }

        let mut next = cells.into_iter();
        let data = RowData::from_cells::<std::convert::Infallible>(query.result_columns(), || {
            Ok(next.next())
        })
        .unwrap_or_else(|e| match e {});
        Ok(OracleSource(data))
    }
}

/// One driver value as a raw cell for the pipeline's re-typing.
///
/// Every non-binary column is cast to `VARCHAR2` in the SQL, so the expected shape is a string. The
/// numeric and boolean arms exist for a value that arrives untyped anyway — a raw command table the
/// report author wrote, which is passed through verbatim and never cast. Anything richer (LOB, JSON,
/// VECTOR, a cursor or a collection) has no faithful text form here and is read as NULL rather than
/// as a debug rendering that would then be re-typed as if it were data.
fn cell_of(value: Option<&oracle_rs::row::Value>) -> Option<Cell> {
    use oracle_rs::row::Value as V;
    match value? {
        V::Null => None,
        V::String(s) => Some(Cell::Text(s.clone())),
        V::Bytes(b) => Some(Cell::Bytes(b.clone())),
        V::Integer(i) => Some(Cell::Text(i.to_string())),
        V::Float(f) => Some(Cell::Text(f.to_string())),
        V::Boolean(b) => Some(Cell::Text(if *b { "1" } else { "0" }.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The accepted connection-string shape, and the default port.
    #[test]
    fn dsn_parses_the_url_form() {
        assert_eq!(
            parse_dsn("oracle://scott:tiger@db.example:1521/ORCLPDB1").expect("parses"),
            Dsn {
                connect_string: "db.example:1521/ORCLPDB1".to_string(),
                user: "scott".to_string(),
                password: "tiger".to_string(),
            }
        );
        // The port is optional and defaults to Oracle's own 1521.
        assert_eq!(
            parse_dsn("oracle://scott:tiger@db.example/ORCLPDB1")
                .expect("parses")
                .connect_string,
            "db.example:1521/ORCLPDB1"
        );
        // A password containing '@' is kept whole: the split is on the LAST '@'.
        assert_eq!(
            parse_dsn("oracle://scott:p@ss@db.example/SVC")
                .expect("parses")
                .password,
            "p@ss"
        );
    }

    /// A malformed string is refused with a message naming the expected shape, never guessed at.
    #[test]
    fn dsn_refuses_a_malformed_string() {
        for bad in [
            "",
            "scott:tiger@db.example/SVC",
            "oracle://db.example/SVC",
            "oracle://scott:tiger@db.example",
            "oracle://:tiger@db.example/SVC",
            "oracle://scott:tiger@/SVC",
        ] {
            assert!(parse_dsn(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// A NULL reads as an absent cell, and a cast column as its text. A value with no faithful text
    /// form is NULL rather than a debug rendering that would be re-typed as if it were data.
    #[test]
    fn values_map_to_raw_cells() {
        use oracle_rs::row::Value as V;
        assert_eq!(cell_of(None), None);
        assert_eq!(cell_of(Some(&V::Null)), None);
        assert_eq!(
            cell_of(Some(&V::String("166".to_string()))),
            Some(Cell::Text("166".to_string()))
        );
        assert_eq!(
            cell_of(Some(&V::Integer(166))),
            Some(Cell::Text("166".to_string()))
        );
        assert_eq!(
            cell_of(Some(&V::Boolean(true))),
            Some(Cell::Text("1".to_string()))
        );
        assert_eq!(
            cell_of(Some(&V::Bytes(vec![1, 2]))),
            Some(Cell::Bytes(vec![1, 2]))
        );
    }
}
