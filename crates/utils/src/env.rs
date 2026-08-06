use std::{ffi::OsString, path::PathBuf};

/// Read an environment variable, preferring its current name over its legacy
/// compatibility alias.
/// A current variable set to an empty string deliberately wins: `std::env::var`
/// still treats it as set, matching single-variable environment behaviour.
// TODO(bc-legacy-cleanup): drop legacy VK_ fallback.
pub fn env_var_with_legacy(new_name: &str, legacy_name: &str) -> Option<String> {
    select_env_var_with_legacy(|name| std::env::var(name).ok(), new_name, legacy_name)
}

/// Read and normalise a filesystem path override from the environment.
///
/// This deliberately differs from [`env_var_with_legacy`], where an empty
/// current string variable wins over its legacy alias. An empty or non-UTF-8
/// path can never be a valid persisted target, so path overrides treat either
/// value as unset and emit a warning instead.
pub fn env_path_override(name: &str) -> Option<PathBuf> {
    normalize_path_override(name, std::env::var_os(name))
}

/// Normalise an explicit path override value without reading or mutating the
/// named environment variable.
pub(crate) fn normalize_path_override(name: &str, value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        tracing::warn!(
            variable = name,
            "Ignoring empty path override; treating it as unset"
        );
        return None;
    }

    let Some(value) = value.to_str() else {
        tracing::warn!(
            variable = name,
            "Ignoring non-UTF-8 path override; treating it as unset"
        );
        return None;
    };

    let path = if value.starts_with('~') {
        crate::path::expand_tilde(value)
    } else {
        PathBuf::from(value)
    };
    if path.is_absolute() {
        return Some(path);
    }

    match std::path::absolute(&path) {
        Ok(absolute_path) => Some(absolute_path),
        Err(error) => {
            tracing::warn!(
                variable = name,
                path = %path.display(),
                error = %error,
                "Failed to make relative path override absolute; using it as-is"
            );
            Some(path)
        }
    }
}

/// Check a `DISABLE_*` style opt-out flag.
///
/// Any value — including `0`, `false` and the empty string — disables the gated
/// behaviour. That is deliberately unchanged from the original
/// `std::env::var(..).is_ok()` gate: several of these flags guard destructive
/// cleanup, so tightening them such that `DISABLE_X=0` means "enabled" would
/// silently switch worktree deletion back on for anyone relying on the previous
/// behaviour. The surprising case is reported instead of reinterpreted.
pub fn disable_flag_set(name: &str) -> bool {
    evaluate_disable_flag(name, std::env::var(name).ok())
}

/// Evaluate an opt-out flag without reading the environment, so the warning
/// behaviour is testable.
pub(crate) fn evaluate_disable_flag(name: &str, value: Option<String>) -> bool {
    let Some(value) = value else {
        return false;
    };

    if matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    ) {
        tracing::warn!(
            variable = name,
            value = %value,
            "Opt-out flag is set to a falsy-looking value but still DISABLES the \
             gated behaviour; unset the variable entirely to re-enable it"
        );
    }

    true
}

/// Check an `ENABLE_*` style opt-in flag.
///
/// The mirror image of [`disable_flag_set`]: absent means off, and only an
/// explicitly truthy value turns the gated behaviour on. Opt-in flags guard
/// features that are off by default precisely because they are not trusted yet,
/// so `ENABLE_X=0` must mean "off" rather than "present, therefore on" — the
/// opposite of the opt-out convention. An unrecognized value is treated as off
/// and reported, so a typo fails safe instead of silently enabling the feature.
pub fn enable_flag_set(name: &str) -> bool {
    evaluate_enable_flag(name, std::env::var(name).ok())
}

/// Evaluate an opt-in flag without reading the environment, so the warning
/// behaviour is testable.
pub(crate) fn evaluate_enable_flag(name: &str, value: Option<String>) -> bool {
    let Some(value) = value else {
        return false;
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "" | "0" | "false" | "no" | "off" => false,
        other => {
            tracing::warn!(
                variable = name,
                value = %other,
                "Opt-in flag is set to an unrecognized value and is treated as DISABLED; \
                 use 1/true/yes/on to enable it"
            );
            false
        }
    }
}

fn select_env_var_with_legacy<F>(lookup: F, new_name: &str, legacy_name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(new_name).or_else(|| lookup(legacy_name))
}

/// Read a positive `usize` tunable from the environment, falling back to
/// `default` when unset, unparseable, or zero.
pub fn env_usize(name: &str, default: usize) -> usize {
    evaluate_usize_override(name, std::env::var(name).ok(), default)
}

/// Resolve a `usize` tunable without reading the environment, so the fallback
/// and warning behaviour is testable. Companion to [`evaluate_disable_flag`].
///
/// Zero is rejected rather than honoured because callers use these values to
/// size buffers: a zero-byte budget evicts every item it is handed, and
/// `tokio::sync::broadcast::channel(0)` panics. A caller asking for zero has
/// almost certainly made a mistake, so the default is used and the surprise
/// reported rather than reinterpreted.
pub(crate) fn evaluate_usize_override(name: &str, value: Option<String>, default: usize) -> usize {
    let Some(raw) = value else {
        return default;
    };

    match raw.trim().parse::<usize>() {
        Ok(parsed) if parsed > 0 => parsed,
        _ => {
            tracing::warn!(
                variable = name,
                value = %raw,
                default,
                "Expected a positive integer; falling back to the default"
            );
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn select(vars: &[(&str, &str)], new_name: &str, legacy_name: &str) -> Option<String> {
        let vars = vars.iter().copied().collect::<HashMap<_, _>>();
        select_env_var_with_legacy(
            |name| vars.get(name).map(|value| (*value).to_string()),
            new_name,
            legacy_name,
        )
    }

    #[test]
    fn disable_flag_is_unset_when_variable_is_absent() {
        assert!(!evaluate_disable_flag("DISABLE_X", None));
    }

    #[test]
    fn disable_flag_is_set_for_a_truthy_value() {
        assert!(evaluate_disable_flag("DISABLE_X", Some("1".to_string())));
    }

    /// The whole point of the helper: falsy-looking values still disable, so a
    /// bug fix can never silently re-enable destructive cleanup.
    #[test]
    fn falsy_looking_values_still_disable() {
        for value in ["0", "false", "no", "off", "", "  FALSE  "] {
            assert!(
                evaluate_disable_flag("DISABLE_X", Some(value.to_string())),
                "{value:?} should still disable"
            );
        }
    }

    #[test]
    fn enable_flag_is_off_when_variable_is_absent() {
        assert!(!evaluate_enable_flag("ENABLE_X", None));
    }

    #[test]
    fn enable_flag_is_on_only_for_truthy_values() {
        for value in ["1", "true", "yes", "on", "  TRUE  "] {
            assert!(
                evaluate_enable_flag("ENABLE_X", Some(value.to_string())),
                "{value:?} should enable"
            );
        }
    }

    /// The inverse of the opt-out convention, and the reason this helper exists
    /// separately: an opt-in flag guards a feature that is off by default, so
    /// `ENABLE_X=0` must mean off rather than "present, therefore on".
    #[test]
    fn falsy_values_do_not_enable() {
        for value in ["0", "false", "no", "off", "", "  FALSE  "] {
            assert!(
                !evaluate_enable_flag("ENABLE_X", Some(value.to_string())),
                "{value:?} should not enable"
            );
        }
    }

    /// A typo must fail safe rather than switch the feature on.
    #[test]
    fn unrecognized_values_do_not_enable() {
        assert!(!evaluate_enable_flag("ENABLE_X", Some("ture".to_string())));
    }

    #[test]
    fn returns_legacy_value_when_only_legacy_is_set() {
        assert_eq!(
            select(&[("OLD_NAME", "legacy")], "NEW_NAME", "OLD_NAME"),
            Some("legacy".to_string())
        );
    }

    #[test]
    fn returns_new_value_when_only_new_is_set() {
        assert_eq!(
            select(&[("NEW_NAME", "new")], "NEW_NAME", "OLD_NAME"),
            Some("new".to_string())
        );
    }

    #[test]
    fn prefers_new_value_when_both_are_set() {
        assert_eq!(
            select(
                &[("NEW_NAME", "new"), ("OLD_NAME", "legacy")],
                "NEW_NAME",
                "OLD_NAME",
            ),
            Some("new".to_string())
        );
    }

    #[test]
    fn prefers_empty_new_value_when_both_are_set() {
        assert_eq!(
            select(
                &[("NEW_NAME", ""), ("OLD_NAME", "legacy")],
                "NEW_NAME",
                "OLD_NAME",
            ),
            Some(String::new())
        );
    }

    #[test]
    fn returns_none_when_neither_is_set() {
        assert_eq!(select(&[], "NEW_NAME", "OLD_NAME"), None);
    }

    #[test]
    fn empty_path_override_is_treated_as_unset() {
        assert_eq!(
            normalize_path_override("BC_TEST_PATH", Some(OsString::new())),
            None
        );
    }

    #[test]
    fn relative_path_override_is_made_absolute() {
        let relative = PathBuf::from("relative-worktree-base");
        let normalized =
            normalize_path_override("BC_TEST_PATH", Some(relative.clone().into_os_string()))
                .expect("normalise relative override");

        assert!(normalized.is_absolute());
        assert_eq!(
            normalized,
            std::path::absolute(relative).expect("make expected path absolute")
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_override_is_treated_as_unset() {
        let value = OsString::from_vec(vec![0xff]);

        assert_eq!(normalize_path_override("BC_TEST_PATH", Some(value)), None);
    }

    #[test]
    fn leading_tilde_in_path_override_is_expanded() {
        let raw = "~/bettercoding-test-override";
        let expected = crate::path::expand_tilde(raw);
        assert_ne!(expected, PathBuf::from(raw));

        assert_eq!(
            normalize_path_override("BC_TEST_PATH", Some(OsString::from(raw))),
            Some(expected)
        );
    }

    #[test]
    fn usize_override_unset_uses_the_default() {
        assert_eq!(evaluate_usize_override("BC_X", None, 4096), 4096);
    }

    #[test]
    fn usize_override_applies_a_positive_value() {
        assert_eq!(
            evaluate_usize_override("BC_X", Some("8192".to_string()), 4096),
            8192
        );
    }

    #[test]
    fn usize_override_tolerates_surrounding_whitespace() {
        assert_eq!(
            evaluate_usize_override("BC_X", Some("  8192\n".to_string()), 4096),
            8192
        );
    }

    #[test]
    fn usize_override_rejects_zero_rather_than_disabling_the_buffer() {
        // broadcast::channel(0) panics and a zero-byte budget evicts
        // everything, so zero must never reach a call site.
        assert_eq!(
            evaluate_usize_override("BC_X", Some("0".to_string()), 4096),
            4096
        );
    }

    #[test]
    fn usize_override_falls_back_on_unparseable_values() {
        for raw in ["", "not-a-number", "-1", "12.5"] {
            assert_eq!(
                evaluate_usize_override("BC_X", Some(raw.to_string()), 4096),
                4096,
                "expected {raw:?} to fall back"
            );
        }
    }
}
