/// Read an environment variable, preferring its current name over its legacy
/// compatibility alias.
/// A current variable set to an empty string deliberately wins: `std::env::var`
/// still treats it as set, matching single-variable environment behaviour.
// TODO(bc-legacy-cleanup): drop legacy VK_ fallback.
pub fn env_var_with_legacy(new_name: &str, legacy_name: &str) -> Option<String> {
    select_env_var_with_legacy(|name| std::env::var(name).ok(), new_name, legacy_name)
}

fn select_env_var_with_legacy<F>(lookup: F, new_name: &str, legacy_name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(new_name).or_else(|| lookup(legacy_name))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
}
