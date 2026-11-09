use std::{collections::HashMap, path::PathBuf};

use git::GitService;
use tokio::process::Command;

use crate::command::CmdOverrides;

/// Repository context for executor operations
#[derive(Debug, Clone, Default)]
pub struct RepoContext {
    pub workspace_root: PathBuf,
    /// Names of repositories in the workspace (subdirectory names)
    pub repo_names: Vec<String>,
}

impl RepoContext {
    pub fn new(workspace_root: PathBuf, repo_names: Vec<String>) -> Self {
        Self {
            workspace_root,
            repo_names,
        }
    }

    pub fn repo_paths(&self) -> Vec<PathBuf> {
        self.repo_names
            .iter()
            .map(|name| self.workspace_root.join(name))
            .collect()
    }

    /// Check all repos for uncommitted changes.
    /// Returns a formatted string describing any uncommitted changes found,
    /// or an empty string if all repos are clean.
    pub async fn check_uncommitted_changes(&self) -> String {
        let repo_paths = self.repo_paths();
        if repo_paths.is_empty() {
            return String::new();
        }

        tokio::task::spawn_blocking(move || {
            let git = GitService::new();
            let mut all_status = String::new();

            for repo_path in &repo_paths {
                // Skip if not a git repository
                if !repo_path.join(".git").exists() {
                    continue;
                }

                match git.get_worktree_status(repo_path) {
                    Ok(status) if !status.entries.is_empty() => {
                        let mut status_output = String::new();
                        for entry in &status.entries {
                            status_output.push(entry.staged);
                            status_output.push(entry.unstaged);
                            status_output.push(' ');
                            status_output.push_str(&String::from_utf8_lossy(&entry.path));
                            status_output.push('\n');
                        }
                        all_status.push_str(&format!(
                            "\n{}:\n{}",
                            repo_path.display(),
                            status_output
                        ));
                    }
                    _ => {}
                }
            }

            all_status
        })
        .await
        .unwrap_or_default()
    }
}

/// Environment variables to inject into executor processes
#[derive(Debug, Clone)]
pub struct ExecutionEnv {
    pub vars: HashMap<String, String>,
    short_tmp: Option<PathBuf>,
    pub repo_context: RepoContext,
    pub commit_reminder: bool,
    pub commit_reminder_prompt: String,
}

impl ExecutionEnv {
    pub fn new(
        repo_context: RepoContext,
        commit_reminder: bool,
        commit_reminder_prompt: String,
    ) -> Self {
        Self {
            vars: HashMap::new(),
            short_tmp: None,
            repo_context,
            commit_reminder,
            commit_reminder_prompt,
        }
    }

    /// Insert an environment variable
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    /// Merge additional vars into this env. Incoming keys overwrite existing ones.
    pub fn merge(&mut self, other: &HashMap<String, String>) {
        self.vars
            .extend(other.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    /// Return a new env with overrides applied. Overrides take precedence.
    pub fn with_overrides(mut self, overrides: &HashMap<String, String>) -> Self {
        self.merge(overrides);
        self
    }

    /// Return a new env with profile env from CmdOverrides merged in.
    pub fn with_profile(self, cmd: &CmdOverrides) -> Self {
        if let Some(ref profile_env) = cmd.env {
            self.with_overrides(profile_env)
        } else {
            self
        }
    }

    /// Set the validated workspace resource independently of profile overrides.
    pub fn set_short_tmp(&mut self, path: Option<&std::path::Path>) {
        self.short_tmp = path.map(std::path::Path::to_path_buf);
    }

    /// Apply ordinary variables first, then enforce the workspace resource.
    pub fn apply_to_command(&self, command: &mut Command) {
        for (key, value) in &self.vars {
            command.env(key, value);
        }
        if let Some(path) = &self.short_tmp {
            command.env("BC_SHORT_TMP", path);
        } else {
            command.env_remove("BC_SHORT_TMP");
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.vars.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_tmp_export_uses_the_validated_path_after_profile_overrides() {
        let mut env = ExecutionEnv::new(RepoContext::default(), false, String::new());
        env.set_short_tmp(Some(std::path::Path::new("/fixture/private-short")));
        let profile = HashMap::from([
            ("BC_SHORT_TMP".to_string(), "/fixture/unsafe".to_string()),
            ("TMPDIR".to_string(), "/fixture/existing-temp".to_string()),
        ]);
        let mut command = Command::new("unused-test-program");
        env.with_overrides(&profile).apply_to_command(&mut command);
        let values = command.as_std().get_envs().collect::<HashMap<_, _>>();
        assert_eq!(values.get(std::ffi::OsStr::new("BC_SHORT_TMP")), Some(&Some(std::ffi::OsStr::new("/fixture/private-short"))));
        assert_eq!(values.get(std::ffi::OsStr::new("TMPDIR")), Some(&Some(std::ffi::OsStr::new("/fixture/existing-temp"))));
    }

    #[test]
    fn unavailable_short_tmp_removes_inherited_and_profile_values() {
        let mut env = ExecutionEnv::new(RepoContext::default(), false, String::new());
        env.set_short_tmp(None);
        let profile = HashMap::from([
            ("BC_SHORT_TMP".to_string(), "/fixture/unsafe".to_string()),
            ("TMPDIR".to_string(), "/fixture/existing-temp".to_string()),
        ]);
        let mut command = Command::new("unused-test-program");
        command.env("BC_SHORT_TMP", "/fixture/inherited");
        env.with_overrides(&profile).apply_to_command(&mut command);
        let values = command.as_std().get_envs().collect::<HashMap<_, _>>();
        assert_eq!(values.get(std::ffi::OsStr::new("BC_SHORT_TMP")), Some(&None));
        assert_eq!(values.get(std::ffi::OsStr::new("TMPDIR")), Some(&Some(std::ffi::OsStr::new("/fixture/existing-temp"))));
    }

    #[test]
    fn profile_overrides_runtime_env() {
        let mut base = ExecutionEnv::new(RepoContext::default(), false, String::new());
        base.insert("BC_PROJECT_NAME", "runtime");
        base.insert("FOO", "runtime");

        let mut profile = HashMap::new();
        profile.insert("FOO".to_string(), "profile".to_string());
        profile.insert("BAR".to_string(), "profile".to_string());

        let merged = base.with_overrides(&profile);

        assert_eq!(merged.vars.get("BC_PROJECT_NAME").unwrap(), "runtime");
        assert_eq!(merged.vars.get("FOO").unwrap(), "profile"); // overrides
        assert_eq!(merged.vars.get("BAR").unwrap(), "profile");
    }
}
