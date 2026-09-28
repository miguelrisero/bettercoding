//! Process-wide feature gates.
//!
//! These are read once per query from the environment rather than cached, so a
//! gate can be exercised directly in tests without a process restart. Every
//! call site is off a cold path (service startup, a config request), so the
//! lookup cost is irrelevant.

use crate::env::{evaluate_disable_flag, evaluate_enable_flag};

/// Opt-in switch for CLI collaboration routing (landed in #41).
pub const CLI_HANDOVER_ENV: &str = "ENABLE_CLI_HANDOVER";

/// Opt-out switch for the native transcript ingest and its chat feed. It also
/// forces collaboration routing off, because routing observes the ingest.
pub const CLI_TRANSCRIPT_INGEST_DISABLE_ENV: &str = "DISABLE_CLI_TRANSCRIPT_INGEST";

/// Opt-out switch for collaboration routing only.
pub const CLI_COLLAB_ROUTING_DISABLE_ENV: &str = "DISABLE_CLI_COLLAB_ROUTING";

/// Whether the native Claude transcript ingest (the CLI→chat feed) runs.
///
/// On by default. Only `DISABLE_CLI_TRANSCRIPT_INGEST` turns it off; the
/// collaboration routing flags never affect it.
pub fn cli_transcript_ingest_enabled() -> bool {
    ingest_gate(&|name| std::env::var(name).ok())
}

/// Whether CLI collaboration routing (auto-dispatch between the CLI pane and
/// the executor) is active.
///
/// Ships **dark**: off unless `ENABLE_CLI_HANDOVER` is truthy. Either
/// `DISABLE_*` variable forces it off, and force-off wins over the opt-in.
pub fn cli_handover_enabled() -> bool {
    collab_routing_gate(&|name| std::env::var(name).ok())
}

type Lookup<'a> = dyn Fn(&str) -> Option<String> + 'a;

fn ingest_gate(lookup: &Lookup) -> bool {
    !evaluate_disable_flag(
        CLI_TRANSCRIPT_INGEST_DISABLE_ENV,
        lookup(CLI_TRANSCRIPT_INGEST_DISABLE_ENV),
    )
}

fn collab_routing_gate(lookup: &Lookup) -> bool {
    ingest_gate(lookup)
        && !evaluate_disable_flag(
            CLI_COLLAB_ROUTING_DISABLE_ENV,
            lookup(CLI_COLLAB_ROUTING_DISABLE_ENV),
        )
        && evaluate_enable_flag(CLI_HANDOVER_ENV, lookup(CLI_HANDOVER_ENV))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates(vars: &[(&str, &str)]) -> (bool, bool) {
        let lookup = |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        };
        (ingest_gate(&lookup), collab_routing_gate(&lookup))
    }

    #[test]
    fn ingest_is_on_and_routing_is_dark_by_default() {
        assert_eq!(gates(&[]), (true, false));
    }

    #[test]
    fn routing_flags_never_change_the_ingest_gate() {
        assert_eq!(gates(&[(CLI_HANDOVER_ENV, "1")]), (true, true));
        assert_eq!(
            gates(&[(CLI_COLLAB_ROUTING_DISABLE_ENV, "1")]),
            (true, false)
        );
        assert_eq!(
            gates(&[
                (CLI_HANDOVER_ENV, "1"),
                (CLI_COLLAB_ROUTING_DISABLE_ENV, "1")
            ]),
            (true, false)
        );
    }

    #[test]
    fn ingest_opt_out_turns_off_both_gates() {
        assert_eq!(
            gates(&[(CLI_TRANSCRIPT_INGEST_DISABLE_ENV, "1")]),
            (false, false)
        );
        assert_eq!(
            gates(&[
                (CLI_TRANSCRIPT_INGEST_DISABLE_ENV, "1"),
                (CLI_HANDOVER_ENV, "1")
            ]),
            (false, false)
        );
    }
}
