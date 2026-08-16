//! `rpt-compat` — a local compatibility harness for Crystal Reports (`.rpt`) files.
//!
//! Points at a folder of reports and serves it as a browsable test surface: pick a report, read the
//! SQL it runs and the data sources it reads from, fill in the parameters it declares, and render it
//! to PDF in the browser. Everything is decoded and rendered **in-process** by the `rpt-*` crates,
//! from the report's own saved data — no database is contacted.
//!
//! It exists to answer "what does this file actually contain, and what does it render to?" without a
//! Windows box and without the report's original database.
//!
//! ## Invocation
//! ```text
//! rpt-compat [OPTIONS]
//! ```
//!
//! ## Flags
//! - `--port <n>` — TCP port to listen on (default `8080`; always bound on `127.0.0.1`).
//! - `--reports-dir <dir>` — the corpus root, scanned **recursively** for `*.rpt` (default
//!   `./reports`). Scanned at request time, so files added later appear on reload.
//! - `--locale <tag>` — locale for date/number formatting (e.g. `en-US`, `de-DE`); default `en-US`.
//! - `--datetime-to-date` — render DateTime database fields as dates (the legacy CR8 report option).
//! - `-h`/`--help`, `-V`/`--version`.
//!
//! The `RPT_FONT_SCALE` environment variable is honoured as everywhere else in the workspace.
//!
//! ## Routes
//! - `GET /` — the corpus listing.
//! - `GET /report/<id>` — one report: parameters, data sources, SQL.
//! - `GET /report/<id>/view?<params>` — render, and report page count + diagnostics above the PDF.
//! - `GET /report/<id>/pdf?<params>` — the same render as inline `application/pdf`.
//!
//! `<id>` is the report's percent-encoded path relative to `--reports-dir`.
//!
//! ## Security
//! The server binds `127.0.0.1` only, and every id is resolved through
//! [`report::resolve`], which refuses `..`, rooted paths and symlinks leaving the corpus.

mod http;
mod pages;
mod report;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rpt_query::Dialect;
use rpt_render::{PdfOptions, RenderOptions, ReportDocument};

/// The `-V`/`--version` line, from `[workspace.package] version` via `CARGO_PKG_VERSION`.
const VERSION: &str = concat!("rpt-compat ", env!("CARGO_PKG_VERSION"));

const USAGE: &str = concat!(
    "rpt-compat ",
    env!("CARGO_PKG_VERSION"),
    " — browse a folder of Crystal Reports (.rpt) files and render them to PDF

Serves --reports-dir as a test surface: each report's parameters, data sources and SQL, plus a
render of its SAVED DATA (no database is contacted). Binds 127.0.0.1 only.

USAGE:
    rpt-compat [OPTIONS]

OPTIONS:
        --port <n>           TCP port to listen on (default: 8080)
        --reports-dir <dir>  corpus root, scanned recursively for *.rpt (default: ./reports)
        --locale <tag>       locale for date/number formatting (e.g. en-US, de-DE; default: en-US)
        --datetime-to-date   render DateTime database fields as dates (legacy CR8 report option)
    -h, --help               show this help and exit
    -V, --version            show the version and exit

ENVIRONMENT:
    RPT_FONT_SCALE           glyph-scale factor, read by the layout crate itself (same effect as
                             `rpt-render --font-scale`); export it before starting the server.

ROUTES:
    GET /                          the corpus listing
    GET /report/<id>               one report: parameters, data sources, SQL
    GET /report/<id>/view?<params> render + diagnostics above the PDF
    GET /report/<id>/pdf?<params>  the render as inline application/pdf

ABOUT:
    Part of the rpt-rs project — a pure-Rust reader/renderer for the Crystal Reports (.rpt) format.
    Homepage:     https://github.com/MrSrsen/rpt-rs
    Report bugs:  https://github.com/MrSrsen/rpt-rs/issues
"
);

/// The parsed command line.
#[derive(Debug)]
struct Cli {
    port: u16,
    reports_dir: PathBuf,
    /// `--locale`: the render locale tag; `None` uses the en-US default.
    locale: Option<String>,
    /// `--datetime-to-date`: render DateTime database fields as dates (legacy CR8 report option).
    datetime_to_date: bool,
}

impl Cli {
    /// Build the render locale for one request, the same way `rpt-render` does.
    fn render_locale(&self) -> rpt_render::Locale {
        let mut locale = match &self.locale {
            Some(tag) => rpt_render::Locale::from_tag(tag),
            None => rpt_render::Locale::default(),
        };
        locale.datetime_to_date = self.datetime_to_date;
        locale
    }
}

fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(Some(cli)) => cli,
        Ok(None) => return ExitCode::SUCCESS, // --help / --version
        Err(msg) => {
            eprintln!("rpt-compat: {msg}\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let addr = format!("127.0.0.1:{}", cli.port);
    let server = match tiny_http::Server::http(&addr) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("rpt-compat: cannot bind {addr}: {err}");
            return ExitCode::from(1);
        }
    };
    println!(
        "rpt-compat: serving {} on http://{addr}/ (saved-data renders only)",
        cli.reports_dir.display()
    );

    for request in server.incoming_requests() {
        if let Err(err) = handle(&cli, request) {
            // A failed respond means the client went away mid-reply; log and keep serving.
            eprintln!("rpt-compat: response not delivered: {err}");
        }
    }
    ExitCode::SUCCESS
}

/// Parse argv into a [`Cli`]. `Ok(None)` means `--help` or `--version` was shown.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Cli>, String> {
    let mut port: u16 = 8080;
    let mut reports_dir = PathBuf::from("./reports");
    let mut locale: Option<String> = None;
    let mut datetime_to_date = false;

    let mut args = args;
    while let Some(arg) = args.next() {
        let mut take = |flag: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("{VERSION}");
                return Ok(None);
            }
            "--port" => {
                let v = take("--port")?;
                port = v
                    .parse()
                    .map_err(|_| format!("--port must be a number 1-65535, got {v:?}"))?;
            }
            "--reports-dir" => reports_dir = PathBuf::from(take("--reports-dir")?),
            "--locale" => locale = Some(take("--locale")?),
            "--datetime-to-date" => datetime_to_date = true,
            other => return Err(format!("unknown option {other:?}")),
        }
    }

    Ok(Some(Cli {
        port,
        reports_dir,
        locale,
        datetime_to_date,
    }))
}

/// One request's target.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// The corpus listing.
    Index,
    /// One report's detail page.
    Detail(String),
    /// A render, reported as HTML.
    View(String),
    /// A render, as the PDF itself.
    Pdf(String),
    /// Anything else.
    NotFound,
}

/// Map a request path to its [`Route`], decoding the report id.
///
/// The id is percent-encoded, so it never carries a literal `/` — the last path segment is
/// unambiguously the verb.
fn route(path: &str) -> Route {
    if path == "/" {
        return Route::Index;
    }
    let Some(rest) = path.strip_prefix("/report/") else {
        return Route::NotFound;
    };
    if let Some(id) = rest.strip_suffix("/view") {
        return Route::View(http::decode(id));
    }
    if let Some(id) = rest.strip_suffix("/pdf") {
        return Route::Pdf(http::decode(id));
    }
    if rest.contains('/') {
        return Route::NotFound;
    }
    Route::Detail(http::decode(rest))
}

/// Dispatch one request to its route and send the response.
fn handle(cli: &Cli, request: tiny_http::Request) -> std::io::Result<()> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url.as_str(), ""),
    };
    match route(path) {
        Route::Index => {
            let rows = index_rows(&cli.reports_dir);
            let page = pages::index(&cli.reports_dir.display().to_string(), &rows);
            request.respond(http::html(&page))
        }
        Route::Detail(id) => match summarize(cli, &id) {
            Ok(summary) => request.respond(http::html(&pages::report(&id, &summary))),
            Err(msg) => request.respond(http::text(404, &msg)),
        },
        Route::View(id) => match render(cli, &id, query) {
            Ok((doc, warnings)) => {
                let page =
                    pages::view(&id, query, doc.pages.len(), &doc.diagnostics, &warnings);
                request.respond(http::html(&page))
            }
            Err(msg) => request.respond(http::text(400, &msg)),
        },
        Route::Pdf(id) => match render(cli, &id, query).and_then(|(doc, _)| to_pdf(&doc)) {
            Ok(bytes) => {
                let name = id.rsplit('/').next().unwrap_or(&id).to_string();
                request.respond(http::pdf(bytes, &name))
            }
            Err(msg) => request.respond(http::text(400, &msg)),
        },
        Route::NotFound => request.respond(http::text(404, "not found — try /")),
    }
}

/// The corpus listing: every report, with what the decoder could read from it.
fn index_rows(root: &Path) -> Vec<pages::Row> {
    report::scan(root)
        .into_iter()
        .map(|entry| {
            let summary = report::resolve(root, &entry.id)
                .and_then(|path| load(&path))
                .map(|doc| report::summarize(doc.report(), Dialect::default()));
            (entry, summary)
        })
        .collect()
}

/// Open and decode one report.
fn load(path: &Path) -> Result<ReportDocument, String> {
    ReportDocument::load(path).map_err(|e| e.to_string())
}

/// Everything the detail page shows about one report id.
fn summarize(cli: &Cli, id: &str) -> Result<report::Summary, String> {
    let path = report::resolve(&cli.reports_dir, id)?;
    let doc = load(&path)?;
    Ok(report::summarize(doc.report(), Dialect::default()))
}

/// Render one report from its saved data, binding the query string's parameter values.
///
/// Returns the paginated document and any warnings the parameter binding raised. A blank field is
/// dropped rather than coerced: an empty text box means "not supplied", and the engine then binds
/// the parameter's own stored value.
fn render(cli: &Cli, id: &str, query: &str) -> Result<(rpt_pages::PagedDocument, Vec<String>), String> {
    let path = report::resolve(&cli.reports_dir, id)?;
    let doc = load(&path)?;
    let supplied: Vec<(String, String)> = http::decode_pairs(query)
        .into_iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .collect();
    let built =
        rpt_inputs::params::build(doc.report(), &supplied).map_err(|e| e.to_string())?;
    let pages = doc.render_with(RenderOptions {
        params: built.params,
        locale: cli.render_locale(),
        ..Default::default()
    });
    Ok((pages, built.warnings))
}

/// Export a rendered document to PDF bytes.
fn to_pdf(doc: &rpt_pages::PagedDocument) -> Result<Vec<u8>, String> {
    rpt_render::try_render_document(doc, &PdfOptions::default()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--port` and `--reports-dir` land in the Cli; the defaults hold when absent.
    #[test]
    fn parse_args_defaults_and_overrides() {
        let cli = parse_args(std::iter::empty())
            .expect("parse ok")
            .expect("not --help");
        assert_eq!(cli.port, 8080);
        assert_eq!(cli.reports_dir, PathBuf::from("./reports"));
        assert!(cli.locale.is_none() && !cli.datetime_to_date);

        let cli = parse_args(
            [
                "--port",
                "9000",
                "--reports-dir",
                "/tmp/r",
                "--locale",
                "de-DE",
                "--datetime-to-date",
            ]
            .iter()
            .map(|s| (*s).to_string()),
        )
        .expect("parse ok")
        .expect("not --help");
        assert_eq!(cli.port, 9000);
        assert_eq!(cli.reports_dir, PathBuf::from("/tmp/r"));
        assert_eq!(cli.locale.as_deref(), Some("de-DE"));
        assert!(cli.datetime_to_date);
    }

    /// The locale options reach the render locale the same way `rpt-render`'s flags do.
    #[test]
    fn render_locale_carries_the_flags() {
        let cli = parse_args(
            ["--locale", "de-DE", "--datetime-to-date"]
                .iter()
                .map(|s| (*s).to_string()),
        )
        .expect("parse ok")
        .expect("not --help");
        let locale = cli.render_locale();
        assert_eq!(locale.tag, "de-DE");
        assert!(locale.datetime_to_date);
    }

    /// Each route is recognised, and the report id is percent-decoded — including a nested id whose
    /// separator is encoded, which is what makes the last segment an unambiguous verb.
    #[test]
    fn routes_decode_the_report_id() {
        assert_eq!(route("/"), Route::Index);
        assert_eq!(route("/report/a.rpt"), Route::Detail("a.rpt".to_string()));
        assert_eq!(
            route("/report/sub%2Fb.rpt"),
            Route::Detail("sub/b.rpt".to_string())
        );
        assert_eq!(route("/report/a.rpt/view"), Route::View("a.rpt".to_string()));
        assert_eq!(
            route("/report/sub%2Fb.rpt/pdf"),
            Route::Pdf("sub/b.rpt".to_string())
        );
        assert_eq!(route("/nope"), Route::NotFound);
        // A raw separator in the id is not a route: ids arrive encoded.
        assert_eq!(route("/report/sub/b.rpt"), Route::NotFound);
    }
}
