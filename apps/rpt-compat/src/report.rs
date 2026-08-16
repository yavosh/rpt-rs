//! What the harness knows about a folder of reports, and about one report in it.
//!
//! Everything here is derived from the decoded report — no database is contacted and nothing is
//! rendered. The one security-relevant piece is [`resolve`]: a corpus is nested, so a report is
//! addressed by a relative path, and that path must never escape the corpus root.

use std::path::{Path, PathBuf};

use rpt_inputs::datasource::DataSource;
use rpt_inputs::params::kind_name;
use rpt_query::{build_query_for_report, Dialect};
use rpt_reader::model::{ParameterField, ParameterValue, RangeBoundType, Report};

/// How deep below the corpus root the scan descends. A corpus is a folder of reports, not a file
/// system to crawl; the bound keeps a stray symlink or a deep tree from stalling the listing.
const MAX_DEPTH: usize = 8;

/// One report file found in the corpus.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The path relative to the corpus root, `/`-separated. This is the report's address in a URL.
    pub id: String,
    /// The file size in bytes.
    pub size: u64,
}

/// Find every `*.rpt` file under `root`, sorted by id. An unreadable folder yields no entries — the
/// page then says the corpus is empty rather than the server dying.
pub fn scan(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Entry>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, depth + 1, out);
        } else if is_rpt(&path) {
            if let Some(id) = relative_id(root, &path) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(Entry { id, size });
            }
        }
    }
}

fn is_rpt(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("rpt"))
}

/// `path` as a `/`-separated id relative to `root`, or `None` when it is not below `root` or carries
/// a component that is not valid UTF-8.
fn relative_id(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// Resolve a report id to a path **inside** the corpus root, or say why it was refused.
///
/// The id is a relative path, so the flat-file-name check the single-folder preview app uses is not
/// enough. Two gates: the id may not carry a `..` component, a path separator that is not `/`, or a
/// root, and the resolved path — canonicalised, so symlinks are followed *before* the check — must
/// still be inside the canonicalised root.
///
/// # Errors
/// A message naming what was refused, suitable for a `400`/`404` body.
pub fn resolve(root: &Path, id: &str) -> Result<PathBuf, String> {
    if id.is_empty() {
        return Err("no report id given".to_string());
    }
    if id.contains('\\') {
        return Err(format!(
            "invalid report id {id:?}: '\\' is not a path separator here"
        ));
    }
    let candidate = Path::new(id);
    if candidate.is_absolute() {
        return Err(format!(
            "invalid report id {id:?}: must be relative to the reports folder"
        ));
    }
    if candidate
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "invalid report id {id:?}: '..' and rooted paths are not allowed"
        ));
    }
    if !is_rpt(candidate) {
        return Err(format!("invalid report id {id:?}: not a .rpt file"));
    }

    let root = root
        .canonicalize()
        .map_err(|e| format!("reports folder {} is unreadable: {e}", root.display()))?;
    let path = root
        .join(candidate)
        .canonicalize()
        .map_err(|_| format!("no such report {id:?}"))?;
    // Canonicalised on both sides, so a symlink pointing out of the corpus is caught here rather
    // than by the textual check above.
    if !path.starts_with(&root) {
        return Err(format!(
            "invalid report id {id:?}: resolves outside the reports folder"
        ));
    }
    if !path.is_file() {
        return Err(format!("no such report {id:?}"));
    }
    Ok(path)
}

/// One declared parameter, as a form needs it.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    /// The parameter's name as declared, and the form field's name.
    pub name: String,
    /// The declared value type's short name, e.g. `"Number"`.
    pub type_name: &'static str,
    /// The author's prompt text, when the report carries one.
    pub prompt: Option<String>,
    /// The prompt may be skipped.
    pub optional: bool,
    /// The parameter accepts more than one value.
    pub multi: bool,
    /// A value outside the stored pick list may be typed. When false and a pick list exists, the
    /// form offers the list alone.
    pub allow_custom: bool,
    /// The stored pick list / default value(s), as text.
    pub defaults: Vec<String>,
    /// The stored last-used value(s), as text — what the report was saved with.
    pub current: Vec<String>,
}

/// One SQL statement the report can run, and where in the report it came from.
#[derive(Debug, Clone)]
pub struct Query {
    /// The scope and role, e.g. `"Main data query"` or `"Subreport RitraEL.rpt · Command table: x"`.
    pub source: String,
    /// `"generated"`, `"command"` or `"expression"` — whether this SQL was built from the report's
    /// table graph or stored verbatim by the author.
    pub kind: &'static str,
    /// The statement.
    pub sql: String,
}

/// Everything the harness shows about one report.
#[derive(Debug)]
pub struct Summary {
    /// The parameters the report declares, in declaration order.
    pub params: Vec<ParamInfo>,
    /// The distinct connections the report and its subreports read from.
    pub sources: Vec<DataSource>,
    /// Every SQL statement, per scope.
    pub queries: Vec<Query>,
    /// How many subreports the report carries.
    pub subreports: usize,
    /// How many database tables the main scope binds.
    pub tables: usize,
    /// The stored record count, when the file carries saved data.
    pub saved_rows: Option<u32>,
    /// The record-selection formula, when the report has one.
    pub selection: Option<String>,
    /// Index into [`sources`](Self::sources) of the connection a live fetch of the **main scope**
    /// would use. `None` when the main scope binds no credential-needing table.
    pub live_source: Option<usize>,
}

/// The index in [`Summary::sources`] of the connection the **main scope** fetches from, when it has
/// a live one. `None` for a report whose main scope binds no credential-needing table — there is
/// nothing to connect to, so only saved data can render it.
#[must_use]
pub fn main_scope_source(report: &Report, sources: &[DataSource]) -> Option<usize> {
    let key = rpt_inputs::datasource::scope_server_key(&report.database)?;
    sources.iter().position(|s| s.group_id() == key)
}

/// The report's SQL Expression fields and record-selection formula — the two inputs a live fetch
/// needs beyond the table graph itself.
#[must_use]
pub fn query_inputs(report: &Report) -> (Vec<(String, String)>, Option<String>) {
    let sql_exprs = report
        .data_definition
        .sql_expression_fields()
        .map(|(fd, x)| (fd.name.clone(), x.text.clone()))
        .collect();
    let selection = report
        .data_definition
        .record_selection
        .as_ref()
        .map(|f| f.0.clone())
        .filter(|s| !s.trim().is_empty());
    (sql_exprs, selection)
}

/// Read everything the harness shows about one decoded report.
pub fn summarize(report: &Report, dialect: Dialect) -> Summary {
    let mut queries = Vec::new();
    collect_queries(report, None, dialect, &mut queries);
    let sources = rpt_inputs::datasource::enumerate(report);
    let live_source = main_scope_source(report, &sources);
    Summary {
        live_source,
        params: report
            .data_definition
            .parameter_fields()
            .map(|(fd, pf)| param_info(&fd.name, pf))
            .collect(),
        sources,
        queries,
        subreports: report.subreports.len(),
        tables: report.database.tables.len(),
        saved_rows: report.saved_data.as_ref().map(|s| s.record_count),
        selection: report
            .data_definition
            .record_selection
            .as_ref()
            .map(|f| f.0.clone())
            .filter(|s| !s.trim().is_empty()),
    }
}

fn param_info(name: &str, pf: &ParameterField) -> ParamInfo {
    ParamInfo {
        name: name.to_string(),
        type_name: kind_name(pf.value_kind),
        prompt: pf.prompt_text.clone().filter(|p| !p.trim().is_empty()),
        optional: pf.optional_prompt,
        multi: pf.allow_multiple_values,
        allow_custom: pf.allow_custom_values,
        defaults: pf.default_values.iter().map(value_text).collect(),
        current: pf.current_values.iter().map(value_text).collect(),
    }
}

/// One stored parameter value as text: a discrete value verbatim, a range in interval notation
/// (`[start..end]`, a square bracket for an included bound and a round one for an excluded or open
/// end). A range is written out because printing the discrete half alone renders `1..100` as `1`.
fn value_text(v: &ParameterValue) -> String {
    let Some(range) = &v.range else {
        return v.value.clone();
    };
    let bracket = |b: RangeBoundType, closed: char, other: char| match b {
        RangeBoundType::BoundInclusive => closed,
        _ => other,
    };
    format!(
        "{}{}..{}{}",
        bracket(range.lower_bound, '[', '('),
        v.value,
        range.end_value,
        bracket(range.upper_bound, ']', ')'),
    )
}

/// Collect every SQL statement for one report level, recursing into subreports.
///
/// A report is not one query: each subreport is a full report with its own tables, connection and
/// selection, so a flat listing would misrepresent where the SQL runs. `scope` is the current
/// level's name (`None` at the main report).
fn collect_queries(report: &Report, scope: Option<&str>, dialect: Dialect, out: &mut Vec<Query>) {
    let prefix = match scope {
        Some(name) => format!("Subreport {name} · "),
        None => String::new(),
    };

    // The engine-generated join query (pruned to referenced tables, with the record-selection
    // formula pushed into WHERE where the dialect can translate it).
    let sql_exprs: Vec<(String, String)> = report
        .data_definition
        .sql_expression_fields()
        .map(|(fd, x)| (fd.name.clone(), x.text.clone()))
        .collect();
    let selection = report
        .data_definition
        .record_selection
        .as_ref()
        .map(|f| f.0.as_str());
    if let Ok(q) = build_query_for_report(report, &sql_exprs, selection, &[], dialect) {
        out.push(Query {
            source: format!("{prefix}Main data query"),
            kind: "generated",
            sql: q.sql,
        });
    }

    // Stored SQL Commands — the author's verbatim queries.
    for t in &report.database.tables {
        if let Some(cmd) = &t.command_text {
            if !cmd.trim().is_empty() {
                out.push(Query {
                    source: format!("{prefix}Command table: {}", t.alias),
                    kind: "command",
                    sql: cmd.clone(),
                });
            }
        }
    }

    // SQL Expression fields — raw fragments the database evaluates.
    for (fd, x) in report.data_definition.sql_expression_fields() {
        out.push(Query {
            source: format!("{prefix}SQL Expression field: {}", fd.name),
            kind: "expression",
            sql: x.text.clone(),
        });
    }

    for sr in &report.subreports {
        collect_queries(&sr.report, Some(&sr.name), dialect, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a corpus root with a nested report and a file outside it, and return the root.
    fn corpus() -> tempdir::TempDir {
        let dir = tempdir::TempDir::new();
        std::fs::create_dir_all(dir.path().join("root/sub")).expect("create corpus");
        std::fs::write(dir.path().join("root/a.rpt"), b"x").expect("write a");
        std::fs::write(dir.path().join("root/sub/b.rpt"), b"x").expect("write b");
        std::fs::write(dir.path().join("root/notes.txt"), b"x").expect("write notes");
        std::fs::write(dir.path().join("outside.rpt"), b"x").expect("write outside");
        dir
    }

    #[test]
    fn scan_finds_rpt_files_recursively_and_ignores_others() {
        let dir = corpus();
        let ids: Vec<String> = scan(&dir.path().join("root"))
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec!["a.rpt".to_string(), "sub/b.rpt".to_string()]);
    }

    /// A nested id resolves; every escape shape is refused before a file is opened.
    #[test]
    fn resolve_keeps_the_id_inside_the_corpus() {
        let dir = corpus();
        let root = dir.path().join("root");

        assert!(resolve(&root, "a.rpt").is_ok());
        assert!(
            resolve(&root, "sub/b.rpt").is_ok(),
            "a nested id is addressable"
        );

        for bad in [
            "",
            "..",
            "../outside.rpt",
            "sub/../../outside.rpt",
            "..\\outside.rpt",
            "sub\\b.rpt",
            "/etc/passwd",
            "notes.txt",
            "missing.rpt",
        ] {
            assert!(resolve(&root, bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// A symlink pointing out of the corpus is caught by the canonicalised containment check, not by
    /// the textual one — the id itself looks perfectly ordinary.
    #[test]
    #[cfg(unix)]
    fn resolve_refuses_a_symlink_that_escapes_the_corpus() {
        let dir = corpus();
        let root = dir.path().join("root");
        std::os::unix::fs::symlink(dir.path().join("outside.rpt"), root.join("link.rpt"))
            .expect("create symlink");
        let err = resolve(&root, "link.rpt").expect_err("an escaping symlink must be refused");
        assert!(err.contains("outside the reports folder"), "got {err:?}");
    }

    /// A minimal temp directory, removed on drop. The workspace has no dev-dependency for this and
    /// one directory of empty files does not earn one.
    mod tempdir {
        use std::path::{Path, PathBuf};

        #[derive(Debug)]
        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> TempDir {
                // The test process id plus the address of a stack local: unique per test, and no
                // clock or RNG dependency.
                let marker = std::process::id();
                let local = 0u8;
                let addr = std::ptr::addr_of!(local) as usize;
                let path = std::env::temp_dir().join(format!("rpt-compat-test-{marker}-{addr:x}"));
                std::fs::create_dir_all(&path).expect("create temp dir");
                TempDir(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
