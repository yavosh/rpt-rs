//! The HTTP glue: responses, HTML escaping, and form/query decoding.
//!
//! Deliberately hand-rolled and small — the server answers four routes and needs no framework.

/// An HTML `200` response.
pub fn html(body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(body)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
}

/// A plain-text response with the given status code.
pub fn text(status: u16, body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

/// An inline `application/pdf` response, so the browser previews it instead of downloading.
pub fn pdf(bytes: Vec<u8>, name: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(bytes)
        .with_header(header("Content-Type", "application/pdf"))
        // The name comes from a validated report id; strip quotes anyway so the header stays
        // well-formed.
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

/// Escape the HTML-significant characters of `s` for element and attribute contexts.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Percent-encode `s` for use inside a URL path segment or query value: everything outside the
/// unreserved set is escaped, so a report id containing `/`, a space or a Greek letter round-trips
/// through [`decode_pairs`] unchanged.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*b));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Every `key=value` pair of a query string or form body, percent-decoded, in order — repeats kept,
/// because repeating one name is how a multi-value parameter is supplied.
pub fn decode_pairs(encoded: &str) -> Vec<(String, String)> {
    encoded
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Decode `%XX` escapes and `+`-as-space. A malformed escape is kept literally.
pub fn decode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The query parser pairs with the browser's form encoding, and keeps repeats: a multi-value
    /// parameter is supplied by repeating its name.
    #[test]
    fn pairs_decode_the_form_encoding_and_keep_repeats() {
        assert_eq!(
            decode_pairs("a=1&b=x+y&a=2"),
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "x y".to_string()),
                ("a".to_string(), "2".to_string()),
            ]
        );
        assert_eq!(decode_pairs(""), vec![]);
        // A percent escape decodes, including one that produces a path separator.
        assert_eq!(
            decode_pairs("f=sub%2Fr.rpt"),
            vec![("f".to_string(), "sub/r.rpt".to_string())]
        );
    }

    /// Encoding then decoding returns the original, for the characters a report id can carry.
    #[test]
    fn encode_round_trips_through_decode() {
        for original in ["plain.rpt", "sub/dir/r.rpt", "a b&c=d.rpt", "Ελληνικά.rpt"] {
            let pairs = decode_pairs(&format!("id={}", encode(original)));
            assert_eq!(pairs[0].1, original, "{original:?} must round-trip");
        }
    }

    #[test]
    fn escaping_covers_element_and_attribute_contexts() {
        assert_eq!(
            escape(r#"<a href="x">&</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;"
        );
    }
}
