//! Process-wide feature gates.
//!
//! These are read once per query from the environment rather than cached, so a
//! gate can be exercised directly in tests without a process restart. Every
//! call site is off a cold path (service startup, a config request), so the
//! lookup cost is irrelevant.

/// Opt-in switch for the CLI↔UI handover (the "seamless" transcript ingest and
/// collaboration routing landed in #39/#41).
pub const CLI_HANDOVER_ENV: &str = "ENABLE_CLI_HANDOVER";

/// Pre-existing opt-out switch, retained as a force-off override.
pub const CLI_TRANSCRIPT_INGEST_DISABLE_ENV: &str = "DISABLE_CLI_TRANSCRIPT_INGEST";

/// Pre-existing opt-out switch for the collaboration dispatch gate.
pub const CLI_COLLAB_ROUTING_DISABLE_ENV: &str = "DISABLE_CLI_COLLAB_ROUTING";

/// Whether the CLI↔UI handover is active.
///
/// The feature ships **dark**: it stays off unless `ENABLE_CLI_HANDOVER` is
/// explicitly truthy. It re-snapshots a session's entire transcript on every
/// appended line — an O(n²) cost in transcript length, paid per connected tab —
/// which is why it is not on by default.
///
/// Either legacy `DISABLE_*` variable still forces it off, so an existing
/// deployment that had already switched the feature off keeps that behaviour
/// even if someone later sets the opt-in flag. Force-off wins on purpose: the
/// safe direction for a gate on a known-expensive feature is off.
pub fn cli_handover_enabled() -> bool {
    if crate::env::disable_flag_set(CLI_TRANSCRIPT_INGEST_DISABLE_ENV)
        || crate::env::disable_flag_set(CLI_COLLAB_ROUTING_DISABLE_ENV)
    {
        return false;
    }
    crate::env::enable_flag_set(CLI_HANDOVER_ENV)
}
