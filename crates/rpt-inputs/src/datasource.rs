//! The distinct data sources a report reads from.
//!
//! A report is not single-connection: every [`Table`](rpt_model::Table) carries its own
//! [`ConnectionInfo`], and subreports are full nested reports with their own tables. Each report
//! *scope* (main + each subreport) uses one server, so connections are grouped by SERVER — a
//! report's main scope and its subreports typically hit the same server, with the subreports
//! omitting the database name.
//!
//! This module only *describes* the connections. Resolving credentials and fetching rows is the
//! caller's job.

use rpt_model::{ConnectionInfo, Database, Report};

/// A distinct data source (connection) a report reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSource {
    /// `QE_ServerDescription` — the server the connection names, when it has one.
    pub server: Option<String>,
    /// `QE_DatabaseName` — the database/catalog, when the connection names one.
    pub database: Option<String>,
    /// `QE_DatabaseType` display string (e.g. "PostgreSQL", "ODBC (RDO)", "Field Definitions Only").
    pub db_type: Option<String>,
    /// The stored user name the report last connected as, when one was saved.
    pub user: Option<String>,
    /// How many tables (across main + subreports) draw from this source.
    pub table_count: usize,
}

impl DataSource {
    /// Does this source need live credentials? A real server/database source does; a
    /// field-definitions-only or empty descriptor (saved-data / no live DB) does not.
    #[must_use]
    pub fn needs_credentials(&self) -> bool {
        let is_field_defs = self
            .db_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("Field Definitions Only"));
        !is_field_defs && (self.server.is_some() || self.database.is_some())
    }

    /// A one-line human description for logs/errors.
    #[must_use]
    pub fn describe(&self) -> String {
        let server = self.server.as_deref().unwrap_or("?");
        let db = self.database.as_deref().unwrap_or("?");
        let ty = self.db_type.as_deref().unwrap_or("?");
        format!(
            "{server}/{db} [{ty}] ({} table{})",
            self.table_count,
            if self.table_count == 1 { "" } else { "s" }
        )
    }

    /// The environment variable that supplies THIS source's connection URL. Keyed by the source's
    /// SERVER, so a report maps to one variable per distinct server — stable, discoverable, and
    /// printed by the CLI (no guessing). E.g. server `Sales DB` → `RPT_DB_URL_SALES_DB`.
    #[must_use]
    pub fn env_var(&self) -> String {
        format!("RPT_DB_URL_{}", self.env_key())
    }

    /// The server-based grouping identity: the server description, or the database name when there is
    /// no server. Sources sharing a `group_id` are one connection: a report's main and subreports
    /// typically hit the same server, with the subreports omitting the database name. A caller that
    /// resolves one connection per source keys its map by this.
    #[must_use]
    pub fn group_id(&self) -> String {
        self.server
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.database.clone().filter(|s| !s.is_empty()))
            .unwrap_or_default()
    }

    fn env_key(&self) -> String {
        let key = sanitize_env_key(&self.group_id());
        if key.is_empty() {
            "DEFAULT".to_string()
        } else {
            key
        }
    }
}

/// Uppercase, keep `[A-Z0-9]`, collapse every other run into a single `_`, and trim edge `_` —
/// producing a valid, stable environment-variable-name fragment.
fn sanitize_env_key(s: &str) -> String {
    let mut out = String::new();
    let mut pending_underscore = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            pending_underscore = false;
            out.push(ch.to_ascii_uppercase());
        } else {
            pending_underscore = true;
        }
    }
    out
}

/// Read a non-empty connection attribute by key.
fn attr<'a>(conn: &'a ConnectionInfo, key: &str) -> Option<&'a str> {
    conn.attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|s| !s.is_empty())
}

/// The (server, database, type, user) identity of a connection.
fn identity(conn: &ConnectionInfo) -> DataSource {
    DataSource {
        server: attr(conn, "QE_ServerDescription").map(str::to_string),
        database: attr(conn, "QE_DatabaseName").map(str::to_string),
        db_type: attr(conn, "QE_DatabaseType").map(str::to_string),
        user: conn.user_name.clone(),
        table_count: 0,
    }
}

/// Enumerate the distinct data sources a report uses, across the main report and all subreports.
#[must_use]
pub fn enumerate(report: &Report) -> Vec<DataSource> {
    let mut acc: Vec<DataSource> = Vec::new();
    collect(report, &mut acc);
    acc
}

fn collect(report: &Report, acc: &mut Vec<DataSource>) {
    for t in &report.database.tables {
        let id = identity(&t.connection);
        let gid = id.group_id();
        match acc.iter_mut().find(|d| d.group_id() == gid) {
            Some(existing) => {
                existing.table_count += 1;
                // Keep the most informative database label if the first table's was blank (a
                // subreport connection often omits the database name the main scope carries).
                if existing.database.as_deref().unwrap_or("").is_empty() && id.database.is_some() {
                    existing.database = id.database;
                }
            }
            None => acc.push(DataSource {
                table_count: 1,
                ..id
            }),
        }
    }
    for sr in &report.subreports {
        collect(&sr.report, acc);
    }
}

/// The server key of a report scope (its first credential-needing table's server), or `None` when
/// the scope has no live tables (nothing to fetch — falls back to saved data).
#[must_use]
pub fn scope_server_key(database: &Database) -> Option<String> {
    database.tables.iter().find_map(|t| {
        let id = identity(&t.connection);
        id.needs_credentials().then(|| id.group_id())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(server: &str, db: &str, ty: &str) -> ConnectionInfo {
        ConnectionInfo {
            attributes: vec![
                ("QE_ServerDescription".into(), server.into()),
                ("QE_DatabaseName".into(), db.into()),
                ("QE_DatabaseType".into(), ty.into()),
            ],
            ..Default::default()
        }
    }

    fn report_with(conns: &[ConnectionInfo]) -> Report {
        Report {
            database: rpt_model::Database {
                tables: conns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| rpt_model::Table {
                        name: format!("t{i}"),
                        alias: format!("t{i}"),
                        connection: c.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn sources_grouped_by_server_across_scopes() {
        // Same server "db1" in the main scope (with a db name) and a subreport (blank db name) is
        // ONE source; "db2" is a second.
        let mut report = report_with(&[
            conn("db1", "app", "PostgreSQL"),
            conn("db2", "app", "PostgreSQL"),
        ]);
        report.subreports = vec![rpt_model::Subreport {
            name: "s".into(),
            report: Box::new(report_with(&[conn("db1", "", "PostgreSQL")])),
            ..Default::default()
        }];

        let sources = enumerate(&report);
        assert_eq!(sources.len(), 2, "grouped by server, not server+db");
        let db1 = sources
            .iter()
            .find(|s| s.server.as_deref() == Some("db1"))
            .expect("the db1 source is enumerated");
        assert_eq!(db1.table_count, 2, "main + subreport table on db1");
    }

    #[test]
    fn env_var_is_server_keyed() {
        let sources = enumerate(&report_with(&[conn("Sales DB", "sales", "ODBC (RDO)")]));
        assert_eq!(sources[0].env_var(), "RPT_DB_URL_SALES_DB");
    }

    /// A field-definitions-only descriptor is not a live connection: it needs no credentials, and a
    /// saved-data render must not be told to look for any.
    #[test]
    fn field_definitions_only_needs_no_credentials() {
        let sources = enumerate(&report_with(&[conn("", "", "Field Definitions Only")]));
        assert!(!sources[0].needs_credentials());
    }

    #[test]
    fn scope_server_key_reads_first_live_table() {
        let report = report_with(&[conn("Sales DB", "sales", "ODBC (RDO)")]);
        assert_eq!(
            scope_server_key(&report.database).as_deref(),
            Some("Sales DB")
        );
    }
}
