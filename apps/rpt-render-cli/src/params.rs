//! The CLI's half of parameter handling: log what each `--param Name=Value` pair resolved to, and
//! say what the report expects when none were supplied.
//!
//! The coercion itself lives in [`rpt_inputs::params`], so the CLI and the other apps agree on what
//! a typed value means.

pub use rpt_inputs::params::{build, declared, DeclaredParam};

/// The multi-line "no parameters supplied" warning: the report declares parameters but none were
/// passed, so the render uses each parameter's default. Lists every declared parameter with its type
/// and any `optional`/`multi-valued` flags. `declared` must be non-empty (the caller checks first).
/// The logger indents the continuation lines under the message column.
pub fn missing_values_warning(declared: &[DeclaredParam]) -> String {
    let mut msg = format!(
        "no parameters supplied; the report declares {} — rendering with defaults \
         (set with -p Name=Value):",
        declared.len()
    );
    for d in declared {
        let mut flags = Vec::new();
        if d.optional {
            flags.push("optional");
        }
        if d.multi {
            flags.push("multi-valued");
        }
        let suffix = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        msg.push_str(&format!("\n  - {} : {}{suffix}", d.name, d.type_name));
    }
    msg
}
