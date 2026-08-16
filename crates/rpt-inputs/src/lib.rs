//! A report's **external inputs**: the parameters it declares and the data sources it reads from.
//!
//! Both answer the same question from opposite sides — *what does this report need from outside
//! itself before it can run?* — and both were previously locked inside the `rpt-render` binary,
//! where nothing else could reach them.
//!
//! * [`params`] — the declared parameter list and the coercion of caller-supplied strings to
//!   [`Value`](rpt_formula::eval::Value)s of each parameter's declared type.
//! * [`datasource`] — the distinct connections a report (and its subreports) draw tables from.
//!
//! Neither module contacts a database or reads a file: they describe what a report *wants*. The
//! live-fetch machinery that acts on that description stays in the caller.

pub mod datasource;
pub mod params;

/// What can go wrong turning caller-supplied text into a report's declared inputs.
#[derive(Debug, thiserror::Error)]
pub enum InputsError {
    /// A supplied parameter value does not fit the parameter's declared type or arity.
    #[error("parameter error: {0}")]
    Params(String),
}
