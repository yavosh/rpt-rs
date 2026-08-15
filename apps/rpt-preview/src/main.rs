//! `rpt-preview` — a local preview web app for Crystal Reports (`.rpt`) files.
//!
//! Serves a one-page picker over a folder of reports and renders the chosen one to PDF
//! **in-process** (via [`rpt_render`], saved-data mode — no database is contacted), so the browser
//! previews the result directly. Companion to the `rpt-render` CLI for the "which report is this
//! again?" loop: point it at a folder, click through the files.
//!
//! ## Invocation
//! ```text
//! rpt-preview [OPTIONS]
//! ```
//!
//! ## Flags
//! - `--port <n>` — TCP port to listen on (default `8080`; always bound on `127.0.0.1`).
//! - `--reports-dir <dir>` — the folder whose `*.rpt` files (non-recursive) are offered
//!   (default `./reports`). Listed at request time, so files added later appear on reload.
//! - `--locale <tag>` — locale for date/number formatting (e.g. `en-US`, `de-DE`); default `en-US`.
//! - `--datetime-to-date` — render DateTime database fields as dates (the legacy CR8 report option),
//!   as in `rpt-render`.
//! - `-h`/`--help`, `-V`/`--version`.
//!
//! The `RPT_FONT_SCALE` environment variable is honoured as everywhere else in the workspace: the
//! layout crate reads it itself, so exporting it before starting the server scales glyphs the same
//! way `rpt-render --font-scale` does.
//!
//! ## Routes
//! - `GET /` — a self-contained HTML page: a dropdown of the folder's `.rpt` files and a Render
//!   button. No external assets.
//! - `GET /render?file=<name>` — renders that report's **saved data** to PDF and answers
//!   `Content-Type: application/pdf` inline. A render failure answers a plain-text `500` with the
//!   error message.
//!
//! ## Security
//! The server binds `127.0.0.1` only, and `file` must be a bare file name: anything containing
//! `/`, `\` or `..` is rejected, so no path outside `--reports-dir` is ever resolved.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The `-V`/`--version` line, from `[workspace.package] version` via `CARGO_PKG_VERSION` — the same
/// single source the other binaries report.
const VERSION: &str = concat!("rpt-preview ", env!("CARGO_PKG_VERSION"));

const USAGE: &str = concat!(
    "rpt-preview ",
    env!("CARGO_PKG_VERSION"),
    " — preview a folder of Crystal Reports (.rpt) files as PDF in the browser

Serves a one-page picker over --reports-dir and renders the chosen report's SAVED DATA to PDF
in-process (no database contacted). Binds 127.0.0.1 only.

USAGE:
    rpt-preview [OPTIONS]

OPTIONS:
        --port <n>           TCP port to listen on (default: 8080)
        --reports-dir <dir>  folder whose *.rpt files are offered, non-recursive (default: ./reports)
        --locale <tag>       locale for date/number formatting (e.g. en-US, de-DE; default: en-US)
        --datetime-to-date   render DateTime database fields as dates (legacy CR8 report option)
    -h, --help               show this help and exit
    -V, --version            show the version and exit

ENVIRONMENT:
    RPT_FONT_SCALE           glyph-scale factor, read by the layout crate itself (same effect as
                             `rpt-render --font-scale`); export it before starting the server.

ROUTES:
    GET /                    the picker page (dropdown of .rpt files + Render button)
    GET /render?file=<name>  that report's saved data as inline application/pdf

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
    /// Build the render locale for one request, the same way `rpt-render` does: map the tag to a
    /// built-in locale (unknown tags fall back to en-US formatting), then set the DateTime-as-date
    /// option on it.
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
            eprintln!("rpt-preview: {msg}\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let addr = format!("127.0.0.1:{}", cli.port);
    let server = match tiny_http::Server::http(&addr) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("rpt-preview: cannot bind {addr}: {err}");
            return ExitCode::from(1);
        }
    };
    println!(
        "rpt-preview: serving {} on http://{addr}/ (saved-data renders only)",
        cli.reports_dir.display()
    );

    for request in server.incoming_requests() {
        if let Err(err) = handle(&cli, request) {
            // A failed respond means the client went away mid-reply; log and keep serving.
            eprintln!("rpt-preview: response not delivered: {err}");
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

/// Dispatch one request to its route and send the response.
fn handle(cli: &Cli, request: tiny_http::Request) -> std::io::Result<()> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url.as_str(), None),
    };
    match path {
        "/" => {
            let page = index_page(&cli.reports_dir);
            request.respond(html_response(&page))
        }
        "/render" => match query.and_then(|q| query_param(q, "file")) {
            Some(name) => match render_report(cli, &name) {
                Ok(pdf) => request.respond(pdf_response(pdf, &name)),
                Err(msg) => request.respond(text_response(500, &format!("render failed: {msg}"))),
            },
            None => request.respond(text_response(400, "missing ?file=<name> query parameter")),
        },
        _ => request.respond(text_response(404, "not found — try /")),
    }
}

/// An HTML `200` response.
fn html_response(body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(body).with_header(header("Content-Type", "text/html; charset=utf-8"))
}

/// A plain-text response with the given status code.
fn text_response(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

/// An inline `application/pdf` response, so the browser previews it instead of downloading.
fn pdf_response(pdf: Vec<u8>, name: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(pdf)
        .with_header(header("Content-Type", "application/pdf"))
        // The name is a validated bare file name (no quotes survive validation's charset in
        // practice, but strip them anyway so the header stays well-formed).
        .with_header(header(
            "Content-Disposition",
            &format!("inline; filename=\"{}\"", name.replace('"', "")),
        ))
}

/// Build a header from static-shaped parts. Panics only on an invalid header name/value, which the
/// callers never pass.
fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("header name and value are well-formed")
}

/// True when `name` is an acceptable report file name: a bare `*.rpt` name with no path
/// separators and no `..`, so it can only resolve to a direct child of `--reports-dir`.
fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rpt"))
}

/// List the `*.rpt` file names (non-recursive) in `dir`, sorted. An unreadable folder lists as
/// empty — the page then says so rather than the server dying.
fn list_reports(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_file_name(n))
        .collect();
    names.sort();
    names
}

/// The picker page: a dropdown of the folder's `.rpt` files and a Render button. Self-contained —
/// inline CSS, no external assets.
fn index_page(dir: &Path) -> String {
    let files = list_reports(dir);
    let options: String = files
        .iter()
        .map(|f| format!("<option value=\"{0}\">{0}</option>", escape_html(f)))
        .collect();
    let body = if files.is_empty() {
        format!(
            "<p>No .rpt files in <code>{}</code>. Put some there and reload.</p>",
            escape_html(&dir.display().to_string())
        )
    } else {
        format!(
            "<form action=\"/render\" method=\"get\">\n\
             <label for=\"file\">Report</label>\n\
             <select id=\"file\" name=\"file\">{options}</select>\n\
             <button type=\"submit\">Render</button>\n\
             </form>\n\
             <p>{} report(s) in <code>{}</code> — rendered from saved data, inline PDF.</p>",
            files.len(),
            escape_html(&dir.display().to_string())
        )
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>rpt-preview</title>\n\
         <style>\n\
         body {{ font-family: system-ui, sans-serif; margin: 3rem auto; max-width: 40rem; }}\n\
         select {{ min-width: 20rem; margin: 0 .5rem; }}\n\
         </style>\n</head>\n<body>\n<h1>rpt-preview</h1>\n{body}\n</body>\n</html>\n"
    )
}

/// Escape the HTML-significant characters of `s` for element and attribute contexts.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The value of `key` in a query string, percent-decoded. First occurrence wins.
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Decode `%XX` escapes and `+`-as-space in a query value. A malformed escape is kept literally.
fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit() =>
            {
                let hex = [bytes[i + 1], bytes[i + 2]];
                let hex = std::str::from_utf8(&hex).expect("two ASCII hex digits");
                out.push(u8::from_str_radix(hex, 16).expect("two ASCII hex digits"));
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Render one report's saved data to PDF bytes, or the failure as a message.
///
/// The name is validated (bare `*.rpt` only) and resolved strictly inside `--reports-dir`; the
/// render itself is the same saved-data path `rpt-render` takes, with the server's locale options.
fn render_report(cli: &Cli, name: &str) -> Result<Vec<u8>, String> {
    if !valid_file_name(name) {
        return Err(format!(
            "invalid file name {name:?}: a bare *.rpt file name is required (no '/', '\\' or '..')"
        ));
    }
    let path = cli.reports_dir.join(name);
    if !path.is_file() {
        return Err(format!("no such report {name:?} in {}", cli.reports_dir.display()));
    }
    let doc = rpt_render::ReportDocument::load(&path).map_err(|e| e.to_string())?;
    // Saved-data mode (the RenderSource default): no database is contacted.
    let pages = doc.render_with(rpt_render::RenderOptions {
        locale: cli.render_locale(),
        ..Default::default()
    });
    rpt_render::try_render_document(&pages, &rpt_render::PdfOptions::default())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a bare `*.rpt` name passes: every path-traversal shape — separators, `..`, absolute
    /// paths — and every non-report name is rejected before any path is resolved.
    #[test]
    fn valid_file_name_accepts_only_bare_rpt_names() {
        for good in ["report.rpt", "CI_PolicyScheduleV8-saveddata.rpt", "a b.RPT"] {
            assert!(valid_file_name(good), "{good:?} must be accepted");
        }
        for bad in [
            "",
            "../secret.rpt",
            "..\\secret.rpt",
            "sub/report.rpt",
            "sub\\report.rpt",
            "/etc/passwd",
            "report.pdf",
            "report",
            "report.rpt.txt",
            "..",
        ] {
            assert!(!valid_file_name(bad), "{bad:?} must be rejected");
        }
    }

    /// The query parser pairs with the browser's form encoding: `+` is a space, `%2F` decodes —
    /// and the decoded result then still has to pass the file-name validation.
    #[test]
    fn query_param_decodes_the_form_encoding() {
        assert_eq!(
            query_param("file=a+b%2Dc.rpt", "file").as_deref(),
            Some("a b-c.rpt")
        );
        assert_eq!(query_param("x=1&file=r.rpt", "file").as_deref(), Some("r.rpt"));
        assert_eq!(query_param("x=1", "file"), None);
        // An encoded separator decodes to a real one and must then fail validation.
        let sneaky = query_param("file=..%2Fsecret.rpt", "file").expect("decodes");
        assert!(!valid_file_name(&sneaky));
    }

    /// `--port` and `--reports-dir` land in the Cli; the defaults hold when absent.
    #[test]
    fn parse_args_defaults_and_overrides() {
        let cli = parse_args(std::iter::empty()).expect("parse ok").expect("not --help");
        assert_eq!(cli.port, 8080);
        assert_eq!(cli.reports_dir, PathBuf::from("./reports"));
        assert!(cli.locale.is_none() && !cli.datetime_to_date);

        let cli = parse_args(
            ["--port", "9000", "--reports-dir", "/tmp/r", "--locale", "de-DE", "--datetime-to-date"]
                .iter()
                .map(|s| s.to_string()),
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
            ["--locale", "de-DE", "--datetime-to-date"].iter().map(|s| s.to_string()),
        )
        .expect("parse ok")
        .expect("not --help");
        let locale = cli.render_locale();
        assert_eq!(locale.tag, "de-DE");
        assert!(locale.datetime_to_date);
    }
}
