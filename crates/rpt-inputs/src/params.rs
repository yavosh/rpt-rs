//! Turn caller-supplied `Name=Value` pairs into resolved [`Parameters`], coercing each value to the
//! parameter's declared type and collapsing repeats of one name into a multi-value array.
//!
//! The caller supplies text — a CLI `--param` pair, an HTML form field — and this module decides
//! what that text means to the report. It reports problems as data ([`BuiltParams::warnings`])
//! rather than logging them, so a CLI can print them and a web app can render them.

use rpt_data::{normalize_param_name, Parameters};
use rpt_formula::eval::{Date, Time, Value};
use rpt_model::{ParameterField, ParameterValue, ParameterValueKind as Vk, RangeBoundType, Report};

use crate::InputsError;

/// A coerced parameter and the declared/coerced type name, for reporting what the render saw.
#[derive(Debug, Clone)]
pub struct ResolvedParam {
    /// The parameter's normalized name (see [`normalize_param_name`]).
    pub name: String,
    /// The declared value type's short name, e.g. `"Number"`.
    pub type_name: &'static str,
    /// The coerced value, debug-formatted for display.
    pub display: String,
}

/// A parameter the report declares — the "what is expected" listing.
#[derive(Debug, Clone)]
pub struct DeclaredParam {
    /// The parameter's name as declared by the report.
    pub name: String,
    /// The declared value type's short name, e.g. `"Number"`.
    pub type_name: &'static str,
    /// The prompt may be skipped (`EnableOptionalPrompt`).
    pub optional: bool,
    /// The parameter accepts more than one value (`EnableAllowMultipleValue`).
    pub multi: bool,
}

/// The result of [`build`]: the bound parameters, what each supplied value coerced to, and any
/// problems that did not stop the render.
#[derive(Debug)]
pub struct BuiltParams {
    /// The bound parameter values, including defaults for the ones the caller left unset.
    pub params: Parameters,
    /// One entry per *supplied* parameter, in first-seen order.
    pub resolved: Vec<ResolvedParam>,
    /// Problems the caller should surface: currently, values supplied for names the report does not
    /// declare. Each is a complete human-readable sentence.
    pub warnings: Vec<String>,
}

/// The parameters the report declares, in declaration order.
#[must_use]
pub fn declared(report: &Report) -> Vec<DeclaredParam> {
    report
        .data_definition
        .parameter_fields()
        .map(|(fd, pf)| DeclaredParam {
            name: fd.name.clone(),
            type_name: kind_name(pf.value_kind),
            optional: pf.optional_prompt,
            multi: pf.allow_multiple_values,
        })
        .collect()
}

/// Build [`Parameters`] from `Name=Value` pairs, coercing each to its declared type.
///
/// Repeats of one name build a multi-value parameter. A declared parameter the caller left unset
/// binds to its stored value (see [`stored_default`]). A supplied name the report does not declare
/// is reported in [`BuiltParams::warnings`] and otherwise ignored.
///
/// # Errors
/// [`InputsError::Params`] when a value does not coerce to the parameter's declared type, or when
/// more than one value is given for a single-value parameter.
pub fn build(report: &Report, raw: &[(String, String)]) -> Result<BuiltParams, InputsError> {
    // Group values by normalized name, preserving first-seen order.
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (name, value) in raw {
        let key = normalize_param_name(name);
        match grouped.iter_mut().find(|(k, _)| *k == key) {
            Some((_, values)) => values.push(value.clone()),
            None => grouped.push((key, vec![value.clone()])),
        }
    }

    let mut params = Parameters::new();
    let mut resolved = Vec::new();
    let mut warnings = Vec::new();
    for (key, values) in grouped {
        let def = report
            .data_definition
            .parameter_fields()
            .find(|(fd, _)| normalize_param_name(&fd.name) == key);
        let (kind, allow_multiple) = match &def {
            Some((_, pf)) => (pf.value_kind, pf.allow_multiple_values),
            None => {
                // A parameter the report does not declare is almost always a typo — and unlike the
                // pipeline's fail-open cases there is no correct behaviour to fall back to: the value
                // cannot reach the render, so the output is for different criteria than the caller
                // asked for. Surfaced as a warning the caller must show; the render still proceeds,
                // since the report's own defaults remain a defined result.
                warnings.push(undeclared_warning(report, &key));
                (Vk::StringParameter, false)
            }
        };
        let coerced = values
            .iter()
            .map(|v| coerce(kind, v))
            .collect::<Result<Vec<Value>, String>>()
            .map_err(|e| InputsError::Params(format!("parameter {key:?}: {e}")))?;
        // A multi-value parameter is always an array (even with one value); a single-value parameter
        // errors if given more than one.
        let value = if allow_multiple {
            Value::Array(coerced)
        } else if coerced.len() == 1 {
            coerced.into_iter().next().unwrap_or(Value::Null)
        } else {
            return Err(InputsError::Params(format!(
                "parameter {key:?} is single-value but {} values were given",
                coerced.len()
            )));
        };
        resolved.push(ResolvedParam {
            name: key.clone(),
            type_name: kind_name(kind),
            display: format!("{value:?}"),
        });
        params.insert(key, value);
    }

    // A declared parameter the caller did not supply binds to its stored value — the engine's
    // current value if it has one, else its default value(s) — exactly as the engine does when a
    // report is run accepting defaults. Selection/grouping formulas then filter on the defaults
    // (and `HasValue({?Param})` reports true) rather than over-rendering every row. A parameter with
    // no stored value binds to `Null` so the reference still resolves (rather than erroring as an
    // unknown name) and `HasValue` reports false.
    for (fd, pf) in report.data_definition.parameter_fields() {
        let key = normalize_param_name(&fd.name);
        if params.contains_key(&key) {
            continue;
        }
        let value = stored_default(pf).unwrap_or(Value::Null);
        params.insert(key, value);
    }
    Ok(BuiltParams {
        params,
        resolved,
        warnings,
    })
}

/// The "this name is not declared" message: what was ignored, the closest declared name when one is
/// a plausible typo, and the full declared list.
fn undeclared_warning(report: &Report, key: &str) -> String {
    let declared: Vec<&str> = report
        .data_definition
        .parameter_fields()
        .map(|(fd, _)| fd.name.as_str())
        .collect();
    let mut msg =
        format!("parameter {key:?} is not declared by the report, so its value is ignored");
    match nearest(key, &declared) {
        Some(near) => msg.push_str(&format!(" — did you mean {near:?}?")),
        None => msg.push('.'),
    }
    msg.push_str(&match declared.len() {
        0 => " The report declares no parameters.".to_string(),
        _ => format!(" The report declares: {}.", declared.join(", ")),
    });
    msg
}

/// The value the engine binds for a declared parameter the caller left unset: its stored **current
/// value** if it has one, else its stored **default value(s)**. Returns `None` when the parameter
/// carries neither, so the reference stays `Null` (`HasValue` false). A multi-value parameter always
/// yields a [`Value::Array`]; a range default yields a [`Value::Range`]. Values that fail to coerce
/// to the declared kind are skipped (an all-unusable set yields `None`).
#[must_use]
pub fn stored_default(pf: &ParameterField) -> Option<Value> {
    let kind = pf.value_kind;
    // The current value (last-used) wins over the default pick list, matching the SDK's effective
    // -value resolution; fall back to the declared defaults.
    let source: &[ParameterValue] = if pf.has_current_value && !pf.current_values.is_empty() {
        &pf.current_values
    } else {
        &pf.default_values
    };
    if source.is_empty() {
        return None;
    }
    if pf.allow_multiple_values {
        let vals: Vec<Value> = source
            .iter()
            .filter_map(|pv| default_one(kind, pv))
            .collect();
        (!vals.is_empty()).then_some(Value::Array(vals))
    } else {
        source.first().and_then(|pv| default_one(kind, pv))
    }
}

/// Coerce one stored [`ParameterValue`] to a [`Value`] of the declared kind: a discrete value maps
/// through [`coerce`]; a range maps to a [`Value::Range`] with each end's inclusivity from its bound
/// type. A *bounded* end that fails to coerce makes the whole range unusable.
fn default_one(kind: Vk, pv: &ParameterValue) -> Option<Value> {
    match &pv.range {
        None => coerce(kind, &pv.value).ok(),
        Some(r) => Some(Value::Range {
            lo: Box::new(range_end(kind, &pv.value, r.lower_bound)?),
            hi: Box::new(range_end(kind, &r.end_value, r.upper_bound)?),
            lo_incl: matches!(r.lower_bound, RangeBoundType::BoundInclusive),
            hi_incl: matches!(r.upper_bound, RangeBoundType::BoundInclusive),
        }),
    }
}

/// One end of a stored range parameter value. `NoBound` is an **open** end: it stores no value, so
/// it yields [`Value::Null`], which the evaluator reads as unbounded. Coercing the empty string it
/// stores would fail for every non-string kind and discard the whole range — binding the parameter
/// to `Null`, against which the selection formula keeps no record at all.
fn range_end(kind: Vk, s: &str, bound: RangeBoundType) -> Option<Value> {
    match bound {
        RangeBoundType::NoBound => Some(Value::Null),
        _ => coerce(kind, s).ok(),
    }
}

/// The declared kind's short name: the data type a caller must supply.
#[must_use]
pub fn kind_name(kind: Vk) -> &'static str {
    match kind {
        Vk::NumberParameter => "Number",
        Vk::CurrencyParameter => "Currency",
        Vk::BooleanParameter => "Boolean",
        Vk::DateParameter => "Date",
        Vk::TimeParameter => "Time",
        Vk::DateTimeParameter => "DateTime",
        _ => "String",
    }
}

/// Coerce one string value to a [`Value`] of the parameter's declared kind.
///
/// # Errors
/// A message naming the value and the type it failed to parse as.
pub fn coerce(kind: Vk, s: &str) -> Result<Value, String> {
    let parse_num = |s: &str| {
        s.trim()
            .parse::<f64>()
            .map_err(|_| format!("{s:?} is not a number"))
    };
    match kind {
        Vk::NumberParameter => Ok(Value::Number(parse_num(s)?)),
        Vk::CurrencyParameter => Ok(Value::Currency(parse_num(s)?)),
        Vk::BooleanParameter => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" => Ok(Value::Bool(true)),
            "false" | "0" | "no" | "n" => Ok(Value::Bool(false)),
            _ => Err(format!("{s:?} is not a boolean (true/false/1/0/yes/no)")),
        },
        Vk::DateParameter => parse_date(s).map(Value::Date),
        Vk::TimeParameter => parse_time(s).map(Value::Time),
        Vk::DateTimeParameter => {
            let (d, t) = s
                .split_once(['T', ' '])
                .ok_or_else(|| format!("{s:?} is not a datetime (YYYY-MM-DDTHH:MM:SS)"))?;
            Ok(Value::DateTime(parse_date(d)?, parse_time(t)?))
        }
        // StringParameter and any Other kind: pass through verbatim.
        _ => Ok(Value::Str(s.to_string())),
    }
}

/// Parse an ISO `YYYY-MM-DD` date.
fn parse_date(s: &str) -> Result<Date, String> {
    let p: Vec<&str> = s.trim().split('-').collect();
    match p.as_slice() {
        [y, m, d] => Ok(Date::new(
            y.parse().map_err(|_| bad_date(s))?,
            m.parse().map_err(|_| bad_date(s))?,
            d.parse().map_err(|_| bad_date(s))?,
        )),
        _ => Err(bad_date(s)),
    }
}

/// Parse an ISO `HH:MM[:SS]` time.
fn parse_time(s: &str) -> Result<Time, String> {
    let p: Vec<&str> = s.trim().split(':').collect();
    let get = |i: usize| p.get(i).unwrap_or(&"0").parse().map_err(|_| bad_time(s));
    match p.len() {
        2 | 3 => Ok(Time::new(get(0)?, get(1)?, get(2)?)),
        _ => Err(bad_time(s)),
    }
}

fn bad_date(s: &str) -> String {
    format!("{s:?} is not a date (expected YYYY-MM-DD)")
}

fn bad_time(s: &str) -> String {
    format!("{s:?} is not a time (expected HH:MM:SS)")
}

/// The declared name closest to `given`, when one is close enough to be a plausible typo.
///
/// Compared case-insensitively (parameter names are normalized that way) and gated on a distance
/// proportional to the name's length, so a genuinely different name is not "corrected" into an
/// unrelated one — a wrong suggestion sends the caller down the wrong path, which is worse than no
/// suggestion.
fn nearest<'a>(given: &str, declared: &[&'a str]) -> Option<&'a str> {
    let given = given.to_lowercase();
    let budget = (given.chars().count() / 3).max(1);
    declared
        .iter()
        .map(|name| (edit_distance(&given, &name.to_lowercase()), *name))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, _)| *d)
        .map(|(_, name)| name)
}

/// Levenshtein distance, single-row DP.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod range_default_tests {
    use super::{default_one, stored_default, Vk};
    use rpt_formula::eval::{eval, MapContext, Value};
    use rpt_formula::{parse, RefKind, Syntax};
    use rpt_model::{ParameterField, ParameterRange, ParameterValue, RangeBoundType};

    fn range_value(
        lo: &str,
        hi: &str,
        lower: RangeBoundType,
        upper: RangeBoundType,
    ) -> ParameterValue {
        ParameterValue {
            value: lo.to_string(),
            description: None,
            range: Some(ParameterRange {
                end_value: hi.to_string(),
                lower_bound: lower,
                upper_bound: upper,
            }),
        }
    }

    fn bounds(v: &Value) -> (Value, Value, bool, bool) {
        match v {
            Value::Range {
                lo,
                hi,
                lo_incl,
                hi_incl,
            } => ((**lo).clone(), (**hi).clone(), *lo_incl, *hi_incl),
            other => panic!("expected a Range, got {other:?}"),
        }
    }

    /// An end declared `NoBound` carries no value, so it binds `Null` — the evaluator's open end —
    /// instead of discarding the whole range.
    #[test]
    fn an_open_range_end_binds_null_rather_than_discarding_the_range() {
        // "over 10,000": exclusive lower bound, open above.
        let over_10k = range_value(
            "10000",
            "",
            RangeBoundType::BoundExclusive,
            RangeBoundType::NoBound,
        );
        let v = default_one(Vk::NumberParameter, &over_10k).expect("half-open range is usable");
        assert_eq!(
            bounds(&v),
            (Value::Number(10_000.0), Value::Null, false, false)
        );

        // Open below, inclusive upper bound.
        let up_to_100 = range_value(
            "",
            "100",
            RangeBoundType::NoBound,
            RangeBoundType::BoundInclusive,
        );
        let v = default_one(Vk::NumberParameter, &up_to_100).expect("half-open range is usable");
        assert_eq!(bounds(&v), (Value::Null, Value::Number(100.0), false, true));

        // Open at both ends.
        let unbounded = range_value("", "", RangeBoundType::NoBound, RangeBoundType::NoBound);
        let v = default_one(Vk::NumberParameter, &unbounded).expect("unbounded range is usable");
        assert_eq!(bounds(&v), (Value::Null, Value::Null, false, false));
    }

    /// A fully bounded range is unchanged: both ends coerce to the declared kind and keep their
    /// inclusivity.
    #[test]
    fn a_closed_range_still_coerces_both_ends() {
        let closed = range_value(
            "5",
            "150",
            RangeBoundType::BoundInclusive,
            RangeBoundType::BoundInclusive,
        );
        let v = default_one(Vk::CurrencyParameter, &closed).expect("closed range is usable");
        assert_eq!(
            bounds(&v),
            (Value::Currency(5.0), Value::Currency(150.0), true, true)
        );

        // A bounded end that does not coerce still makes the range unusable — an unparseable bound
        // is not silently widened to "no limit".
        let bad = range_value(
            "not-a-number",
            "150",
            RangeBoundType::BoundInclusive,
            RangeBoundType::BoundInclusive,
        );
        assert_eq!(default_one(Vk::NumberParameter, &bad), None);
    }

    /// The stored current value of a half-open range parameter reaches the formula engine and keeps
    /// the records above the bound — the end-to-end shape of an unsupplied `{Field} = {?Param}`
    /// selection. Binding `Null` instead keeps no record at all.
    #[test]
    fn a_half_open_current_value_selects_records_above_the_bound() {
        let pf = ParameterField {
            value_kind: Vk::NumberParameter,
            has_current_value: true,
            current_values: vec![range_value(
                "10000",
                "",
                RangeBoundType::BoundExclusive,
                RangeBoundType::NoBound,
            )],
            ..ParameterField::default()
        };
        let param = stored_default(&pf).expect("a stored half-open range binds a value");

        let selects = |amount: f64| {
            let ctx = MapContext::default()
                .with_field(RefKind::Field, "orders.order amount", Value::Number(amount))
                .with_field(RefKind::Parameter, "order_amt_range", param.clone());
            let (ast, diags) = parse(
                "{orders.order amount} = {?order_amt_range}",
                Syntax::Crystal,
            );
            assert!(diags.is_empty(), "parse diagnostics: {diags:?}");
            eval(&ast, &ctx)
        };
        assert_eq!(selects(12_500.0), Ok(Value::Bool(true)));
        assert_eq!(selects(10_000.0), Ok(Value::Bool(false)));
        assert_eq!(selects(9_999.0), Ok(Value::Bool(false)));
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::{edit_distance, nearest};

    #[test]
    fn distance_counts_single_character_edits() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("abc", "ab"), 1);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    #[test]
    fn a_typo_is_matched_to_the_declared_name() {
        let declared = ["Order_Amt_Range", "Region"];
        assert_eq!(
            nearest("order_amt_rang", &declared),
            Some("Order_Amt_Range")
        );
        assert_eq!(nearest("regio", &declared), Some("Region"));
    }

    #[test]
    fn an_unrelated_name_gets_no_suggestion() {
        // A wrong suggestion is worse than none: it sends the caller after the wrong parameter.
        let declared = ["Order_Amt_Range", "Region"];
        assert_eq!(nearest("customer_id", &declared), None);
        assert_eq!(nearest("x", &declared), None);
    }

    #[test]
    fn no_declared_parameters_yields_no_suggestion() {
        assert_eq!(nearest("anything", &[]), None);
    }
}

#[cfg(test)]
mod coercion_tests {
    use super::{coerce, kind_name, Vk};
    use rpt_formula::eval::{Date, Time, Value};

    /// Each declared kind coerces the text a caller types into that kind's [`Value`].
    #[test]
    fn each_kind_coerces_its_own_text() {
        assert_eq!(
            coerce(Vk::NumberParameter, " 643227 "),
            Ok(Value::Number(643_227.0))
        );
        assert_eq!(
            coerce(Vk::CurrencyParameter, "12.50"),
            Ok(Value::Currency(12.5))
        );
        assert_eq!(coerce(Vk::BooleanParameter, "YES"), Ok(Value::Bool(true)));
        assert_eq!(coerce(Vk::BooleanParameter, "0"), Ok(Value::Bool(false)));
        assert_eq!(
            coerce(Vk::DateParameter, "2026-08-16"),
            Ok(Value::Date(Date::new(2026, 8, 16)))
        );
        assert_eq!(
            coerce(Vk::TimeParameter, "09:30"),
            Ok(Value::Time(Time::new(9, 30, 0)))
        );
        assert_eq!(
            coerce(Vk::DateTimeParameter, "2026-08-16T09:30:15"),
            Ok(Value::DateTime(
                Date::new(2026, 8, 16),
                Time::new(9, 30, 15)
            ))
        );
        assert_eq!(
            coerce(Vk::StringParameter, " kept as-is "),
            Ok(Value::Str(" kept as-is ".to_string()))
        );
    }

    /// Text that does not fit the declared kind is refused with a message naming the value — never
    /// silently coerced to a neighbouring type.
    #[test]
    fn text_that_does_not_fit_the_kind_is_refused() {
        assert!(coerce(Vk::NumberParameter, "abc").is_err());
        assert!(coerce(Vk::BooleanParameter, "maybe").is_err());
        assert!(coerce(Vk::DateParameter, "16/08/2026").is_err());
        assert!(coerce(Vk::TimeParameter, "morning").is_err());
        // A datetime needs both halves: a bare date has no time separator to split on.
        assert!(coerce(Vk::DateTimeParameter, "2026-08-16").is_err());
    }

    /// The type names a UI shows come from the declared kind, with String as the catch-all.
    #[test]
    fn kind_names_cover_every_declared_kind() {
        assert_eq!(kind_name(Vk::NumberParameter), "Number");
        assert_eq!(kind_name(Vk::CurrencyParameter), "Currency");
        assert_eq!(kind_name(Vk::BooleanParameter), "Boolean");
        assert_eq!(kind_name(Vk::DateParameter), "Date");
        assert_eq!(kind_name(Vk::TimeParameter), "Time");
        assert_eq!(kind_name(Vk::DateTimeParameter), "DateTime");
        assert_eq!(kind_name(Vk::StringParameter), "String");
    }
}
