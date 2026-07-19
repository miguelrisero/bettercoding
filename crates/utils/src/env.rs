/// Read an environment variable, preferring its current name over its legacy
/// compatibility alias.
// TODO(bc-legacy-cleanup): drop legacy VK_ fallback.
pub fn env_var_with_legacy(new_name: &str, legacy_name: &str) -> Option<String> {
    std::env::var(new_name)
        .ok()
        .or_else(|| std::env::var(legacy_name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_vars(names: &[&str]) {
        // SAFETY: These test-only names are unique to each test case, so no
        // other test or application thread reads or writes them concurrently.
        unsafe {
            for name in names {
                std::env::remove_var(name);
            }
        }
    }

    fn set_var(name: &str, value: &str) {
        // SAFETY: Each test case uses distinct test-only environment names.
        unsafe {
            std::env::set_var(name, value);
        }
    }

    #[test]
    fn returns_legacy_value_when_only_legacy_is_set() {
        const NEW: &str = "BETTERCODING_TEST_ENV_HELPER_LEGACY_ONLY_NEW";
        const LEGACY: &str = "BETTERCODING_TEST_ENV_HELPER_LEGACY_ONLY_OLD";
        clear_vars(&[NEW, LEGACY]);
        set_var(LEGACY, "legacy");

        assert_eq!(env_var_with_legacy(NEW, LEGACY), Some("legacy".to_string()));

        clear_vars(&[NEW, LEGACY]);
    }

    #[test]
    fn returns_new_value_when_only_new_is_set() {
        const NEW: &str = "BETTERCODING_TEST_ENV_HELPER_NEW_ONLY_NEW";
        const LEGACY: &str = "BETTERCODING_TEST_ENV_HELPER_NEW_ONLY_OLD";
        clear_vars(&[NEW, LEGACY]);
        set_var(NEW, "new");

        assert_eq!(env_var_with_legacy(NEW, LEGACY), Some("new".to_string()));

        clear_vars(&[NEW, LEGACY]);
    }

    #[test]
    fn prefers_new_value_when_both_are_set() {
        const NEW: &str = "BETTERCODING_TEST_ENV_HELPER_BOTH_NEW";
        const LEGACY: &str = "BETTERCODING_TEST_ENV_HELPER_BOTH_OLD";
        clear_vars(&[NEW, LEGACY]);
        set_var(NEW, "new");
        set_var(LEGACY, "legacy");

        assert_eq!(env_var_with_legacy(NEW, LEGACY), Some("new".to_string()));

        clear_vars(&[NEW, LEGACY]);
    }

    #[test]
    fn returns_none_when_neither_is_set() {
        const NEW: &str = "BETTERCODING_TEST_ENV_HELPER_NEITHER_NEW";
        const LEGACY: &str = "BETTERCODING_TEST_ENV_HELPER_NEITHER_OLD";
        clear_vars(&[NEW, LEGACY]);

        assert_eq!(env_var_with_legacy(NEW, LEGACY), None);
    }
}
