//! The HTML the harness serves. Self-contained: inline CSS, no external assets, no scripts.

use rpt_pages::{Diagnostic, Severity};

use crate::http::{encode, escape};
use crate::report::{Entry, ParamInfo, Query, Summary};

/// A report row on the index: the file, and either what we read from it or why we could not.
pub type Row = (Entry, Result<Summary, String>);

/// The query-string field naming the row source. Prefixed so it cannot collide with a report
/// parameter of the same name; a report declaring a parameter called `__rpt_source` would find it
/// shadowed, which is worth the certainty that these two never mix.
pub const SOURCE_FIELD: &str = "__rpt_source";

/// The query-string field prefix for a data source's connection string, suffixed by the source's
/// index (`__rpt_conn0`, `__rpt_conn1`, …).
pub const CONN_FIELD: &str = "__rpt_conn";

/// True for a query-string field this app owns rather than one naming a report parameter.
#[must_use]
pub fn is_reserved_field(name: &str) -> bool {
    name.starts_with("__rpt_")
}

const STYLE: &str = "\
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; margin: 2rem auto; max-width: 62rem; padding: 0 1rem;
       line-height: 1.5; }
h1 { font-size: 1.4rem; margin-bottom: .25rem; }
h2 { font-size: 1.1rem; margin-top: 2rem; }
.sub { opacity: .7; font-size: .9rem; margin-top: 0; }
table { border-collapse: collapse; width: 100%; margin: .5rem 0; }
th, td { text-align: left; padding: .35rem .6rem; border-bottom: 1px solid rgba(128,128,128,.35);
         vertical-align: top; }
th { font-weight: 600; font-size: .85rem; text-transform: uppercase; letter-spacing: .03em;
     opacity: .75; }
td.num { text-align: right; font-variant-numeric: tabular-nums; }
pre { background: rgba(128,128,128,.12); padding: .7rem; overflow-x: auto; border-radius: 4px;
      font-size: .85rem; }
code { font-size: .9em; }
form.params { display: grid; gap: .9rem; max-width: 34rem; margin: .5rem 0 1rem; }
label { display: block; font-weight: 600; }
label .meta { font-weight: 400; opacity: .7; font-size: .85rem; }
input, select { width: 100%; padding: .35rem; font: inherit; }
button { padding: .45rem 1.2rem; font: inherit; cursor: pointer; }
.err { color: #b00020; }
.warn { color: #8a6100; }
iframe { width: 100%; height: 46rem; border: 1px solid rgba(128,128,128,.5); border-radius: 4px; }
.empty { opacity: .7; font-style: italic; }
";

/// Wrap a body in the page shell.
fn shell(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n{STYLE}</style>\n</head>\n<body>\n{body}\n</body>\n</html>\n",
        escape(title)
    )
}

/// The corpus listing: one row per report, with what the decoder found in it.
pub fn index(dir: &str, rows: &[Row]) -> String {
    let body = if rows.is_empty() {
        format!(
            "<p class=\"empty\">No .rpt files under <code>{}</code>. Put some there and reload.</p>",
            escape(dir)
        )
    } else {
        let mut table = String::from(
            "<table>\n<tr><th>Report</th><th>Size</th><th>Saved rows</th><th>Params</th>\
             <th>Sources</th><th>Subreports</th></tr>\n",
        );
        for (entry, summary) in rows {
            table.push_str(&index_row(entry, summary.as_ref()));
        }
        table.push_str("</table>\n");
        format!(
            "<p class=\"sub\">{} report(s) under <code>{}</code>. Saved-data renders only — no \
             database is contacted.</p>\n{table}",
            rows.len(),
            escape(dir)
        )
    };
    shell("rpt-compat", &format!("<h1>rpt-compat</h1>\n{body}"))
}

fn index_row(entry: &Entry, summary: Result<&Summary, &String>) -> String {
    let link = format!(
        "<a href=\"/report/{}\">{}</a>",
        encode(&entry.id),
        escape(&entry.id)
    );
    let size = format!("{:.1} KB", entry.size as f64 / 1024.0);
    match summary {
        Ok(s) => format!(
            "<tr><td>{link}</td><td class=\"num\">{size}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>\n",
            s.saved_rows
                .map_or_else(|| "—".to_string(), |n| n.to_string()),
            s.params.len(),
            s.sources.len(),
            s.subreports,
        ),
        // A file the decoder cannot read is a finding, not a row to hide.
        Err(msg) => format!(
            "<tr><td>{}</td><td class=\"num\">{size}</td>\
             <td colspan=\"4\" class=\"err\">cannot read: {}</td></tr>\n",
            escape(&entry.id),
            escape(msg)
        ),
    }
}

/// The report detail page: what the report needs, what SQL it runs, and the form that renders it.
pub fn report(id: &str, summary: &Summary) -> String {
    let mut body = format!(
        "<p><a href=\"/\">← all reports</a></p>\n<h1>{}</h1>\n<p class=\"sub\">{}</p>\n",
        escape(id),
        escape(&facts_line(summary))
    );
    body.push_str(&params_form(id, summary));
    if let Some(sel) = &summary.selection {
        body.push_str(&format!(
            "<h2>Record selection</h2>\n<pre>{}</pre>\n",
            escape(sel)
        ));
    }
    body.push_str(&sources_table(summary));
    body.push_str(&sql_section(&summary.queries));
    shell(&format!("{id} — rpt-compat"), &body)
}

fn facts_line(summary: &Summary) -> String {
    let saved = summary.saved_rows.map_or_else(
        || "no saved data".to_string(),
        |n| format!("{n} saved row(s)"),
    );
    format!(
        "{saved} · {} table(s) · {} parameter(s) · {} data source(s) · {} subreport(s)",
        summary.tables,
        summary.params.len(),
        summary.sources.len(),
        summary.subreports,
    )
}

/// The render form: where the rows come from, then the report's own parameters.
///
/// The row source is a **required** choice with no preselected option. Saved data and a live
/// database can disagree — that is the whole point of the harness — so the page must never quietly
/// pick one and let the reader assume the other.
fn params_form(id: &str, summary: &Summary) -> String {
    let action = format!("/report/{}/view", encode(id));
    let mut form = format!(
        "<h2>Render</h2>\n<form class=\"params\" action=\"{action}\" method=\"get\">\n{}",
        source_picker(summary)
    );
    if summary.params.is_empty() {
        form.push_str("<p class=\"sub\">This report declares no parameters.</p>\n");
    } else {
        form.push_str(
            "<p class=\"sub\">A parameter left blank uses the report's stored value (its last-used \
             value, else its default).</p>\n",
        );
        for p in &summary.params {
            form.push_str(&field(p));
        }
    }
    form.push_str("<div><button type=\"submit\">Render</button></div>\n</form>\n");
    form
}

/// The row-source choice and, for the live option, one connection string per data source.
///
/// The connection string travels in the query string, so it is visible in the URL and in browser
/// history. That is accepted for a tool that binds to `127.0.0.1` only; it is not a shape to carry
/// over to anything reachable from elsewhere.
fn source_picker(summary: &Summary) -> String {
    let saved = summary.saved_rows.map_or_else(
        || "no saved data in this file".to_string(),
        |n| format!("{n} stored row(s)"),
    );
    let mut out = format!(
        "<div><label for=\"{SOURCE_FIELD}\">Row source <span class=\"meta\">— required</span>\
         </label>\n\
         <select id=\"{SOURCE_FIELD}\" name=\"{SOURCE_FIELD}\" required>\n\
         <option value=\"\" selected disabled>— choose —</option>\n\
         <option value=\"saved\">Saved data ({})</option>\n\
         <option value=\"oracle\">Oracle (live)</option>\n\
         </select></div>\n",
        escape(&saved)
    );
    for (i, s) in summary.sources.iter().enumerate() {
        let live = if summary.live_source == Some(i) {
            "fetched from"
        } else {
            "not the main scope's source — subreports only"
        };
        out.push_str(&format!(
            "<div><label for=\"{CONN_FIELD}{i}\">Oracle connection — {} \
             <span class=\"meta\">({})</span></label>\n\
             <input type=\"text\" id=\"{CONN_FIELD}{i}\" name=\"{CONN_FIELD}{i}\" \
             placeholder=\"oracle://user:password@host:1521/service\" value=\"\"></div>\n",
            escape(&s.describe()),
            escape(live),
        ));
    }
    out
}

fn field(p: &ParamInfo) -> String {
    let label_text = p.prompt.clone().unwrap_or_else(|| p.name.clone());
    let mut meta = vec![p.type_name.to_string()];
    if p.optional {
        meta.push("optional".to_string());
    }
    if p.multi {
        meta.push("multi-valued".to_string());
    }
    if !p.current.is_empty() {
        meta.push(format!("saved: {}", p.current.join(", ")));
    }
    let mut out = format!(
        "<div><label for=\"{0}\">{1} <span class=\"meta\">— {2}</span></label>\n",
        escape(&p.name),
        escape(&label_text),
        escape(&meta.join(" · ")),
    );
    // Prefill with the stored value the engine would use, so the first render reproduces the saved
    // run; a multi-value parameter gets one box per stored value plus a spare.
    let prefill: Vec<String> = if p.current.is_empty() {
        p.defaults.clone()
    } else {
        p.current.clone()
    };
    if p.multi {
        let mut values = prefill;
        values.push(String::new());
        for (i, v) in values.iter().enumerate() {
            out.push_str(&input(p, v, i == 0));
        }
    } else {
        out.push_str(&input(p, prefill.first().map_or("", String::as_str), true));
    }
    out.push_str("</div>\n");
    out
}

/// One input for a parameter. `first` carries the `id` the label points at.
fn input(p: &ParamInfo, value: &str, first: bool) -> String {
    let name = escape(&p.name);
    let id_attr = if first {
        format!(" id=\"{name}\"")
    } else {
        String::new()
    };
    // A pick list the author closed to custom values is a choice, not a free-text field.
    if !p.allow_custom && !p.defaults.is_empty() {
        let options: String = p
            .defaults
            .iter()
            .map(|d| {
                let sel = if d == value { " selected" } else { "" };
                format!("<option value=\"{0}\"{sel}>{0}</option>", escape(d))
            })
            .collect();
        return format!(
            "<select name=\"{name}\"{id_attr}><option value=\"\"></option>{options}</select>\n"
        );
    }
    let list = if p.defaults.is_empty() {
        String::new()
    } else {
        format!(" list=\"{name}-defaults\"")
    };
    let datalist = if p.defaults.is_empty() {
        String::new()
    } else {
        let options: String = p
            .defaults
            .iter()
            .map(|d| format!("<option value=\"{}\">", escape(d)))
            .collect();
        format!("<datalist id=\"{name}-defaults\">{options}</datalist>\n")
    };
    format!(
        "<input type=\"{}\"{} name=\"{name}\"{id_attr} value=\"{}\"{list}>\n{datalist}",
        input_type(p.type_name),
        step_attr(p.type_name),
        escape(value),
    )
}

/// The HTML input type for a declared parameter type. Boolean has no numeric/date widget, so it
/// stays a text field carrying the words the coercion accepts.
fn input_type(type_name: &str) -> &'static str {
    match type_name {
        "Number" | "Currency" => "number",
        "Date" => "date",
        "Time" => "time",
        "DateTime" => "datetime-local",
        _ => "text",
    }
}

fn step_attr(type_name: &str) -> &'static str {
    // Without this a number input refuses a decimal, which a Currency parameter needs.
    match type_name {
        "Number" | "Currency" => " step=\"any\"",
        _ => "",
    }
}

fn sources_table(summary: &Summary) -> String {
    if summary.sources.is_empty() {
        return "<h2>Data sources</h2>\n<p class=\"empty\">The report binds no connection.</p>\n"
            .to_string();
    }
    let mut out = String::from(
        "<h2>Data sources</h2>\n<table>\n\
         <tr><th>Server</th><th>Database</th><th>Type</th><th>User</th><th>Tables</th>\
         <th>Live credentials</th></tr>\n",
    );
    for s in &summary.sources {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}</td>\
             <td>{}</td></tr>\n",
            escape(s.server.as_deref().unwrap_or("—")),
            escape(s.database.as_deref().unwrap_or("—")),
            escape(s.db_type.as_deref().unwrap_or("—")),
            escape(s.user.as_deref().unwrap_or("—")),
            s.table_count,
            if s.needs_credentials() {
                format!("needed · {}", escape(&s.env_var()))
            } else {
                "not needed".to_string()
            },
        ));
    }
    out.push_str("</table>\n");
    out
}

fn sql_section(queries: &[Query]) -> String {
    if queries.is_empty() {
        return "<h2>SQL</h2>\n<p class=\"empty\">The report runs no query.</p>\n".to_string();
    }
    let mut out = String::from(
        "<h2>SQL</h2>\n<p class=\"sub\">A generated query is rendered in the Postgres dialect \
         (rpt-rs has no Oracle writer) — it states the report's table graph and selection, not the \
         text the original engine sent. A command or expression is the author's own SQL, \
         verbatim.</p>\n",
    );
    for q in queries {
        out.push_str(&format!(
            "<p><strong>{}</strong> <span class=\"meta\">({})</span></p>\n<pre>{}</pre>\n",
            escape(&q.source),
            escape(q.kind),
            escape(&q.sql)
        ));
    }
    out
}

/// The render result: what the pipeline reported, above the PDF itself.
///
/// The diagnostics are the point of the page. A selection formula that drops every row, or a
/// formula that cannot resolve a field, otherwise reaches the eye as a silently blank PDF.
pub fn view(
    id: &str,
    query: &str,
    page_count: usize,
    diagnostics: &[Diagnostic],
    warnings: &[String],
) -> String {
    let pdf_url = format!(
        "/report/{}/pdf{}",
        encode(id),
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    );
    let mut body = format!(
        "<p><a href=\"/report/{0}\">← {1}</a> · <a href=\"/\">all reports</a></p>\n\
         <h1>{1}</h1>\n<p class=\"sub\">{2} page(s) · {3}</p>\n",
        encode(id),
        escape(id),
        page_count,
        diagnostic_line(diagnostics),
    );
    // A value supplied for a name the report does not declare never reaches the render, so the
    // output answers different criteria than the caller asked for. Said first, before the pages.
    for w in warnings {
        body.push_str(&format!("<p class=\"err\">{}</p>\n", escape(w)));
    }
    if !diagnostics.is_empty() {
        body.push_str(
            "<h2>Diagnostics</h2>\n<table>\n<tr><th></th><th>Message</th><th>Where</th></tr>\n",
        );
        for d in diagnostics {
            body.push_str(&diagnostic_row(d));
        }
        body.push_str("</table>\n");
    }
    body.push_str(&format!(
        "<h2>Output</h2>\n<p class=\"sub\"><a href=\"{0}\">open the PDF on its own</a></p>\n\
         <iframe src=\"{0}\" title=\"rendered report\"></iframe>\n",
        escape(&pdf_url)
    ));
    shell(&format!("{id} — render"), &body)
}

fn diagnostic_line(diagnostics: &[Diagnostic]) -> String {
    if diagnostics.is_empty() {
        return "no diagnostics".to_string();
    }
    let errors = diagnostics
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error))
        .count();
    format!("{} diagnostic(s), {errors} error(s)", diagnostics.len())
}

fn diagnostic_row(d: &Diagnostic) -> String {
    let (class, label) = match d.severity {
        Severity::Error => ("err", "error"),
        Severity::Warning => ("warn", "warning"),
    };
    let mut place = Vec::new();
    if let Some(source) = &d.source {
        place.push(source.clone());
    }
    if let Some(page) = d.location.page {
        place.push(format!("page {page}"));
    }
    if let Some(section) = &d.location.section {
        place.push(section.clone());
    }
    if let Some(record) = d.location.record_index {
        place.push(format!("record {record}"));
    }
    format!(
        "<tr><td class=\"{class}\">{label}<br><span class=\"meta\">{:?}</span></td>\
         <td>{}</td><td>{}</td></tr>\n",
        d.kind,
        escape(&d.message),
        escape(&place.join(" · "))
    )
}
