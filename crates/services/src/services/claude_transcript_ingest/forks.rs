use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDagRecord {
    pub uuid: String,
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct NativeForkBranch {
    pub label: String,
    pub root_uuid: String,
    pub node_uuids: Vec<String>,
    pub leaf_uuids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct NativeForkView {
    pub fork_parent_uuid: String,
    pub prefix_uuids: Vec<String>,
    pub branches: Vec<NativeForkBranch>,
    pub default_branch: Option<usize>,
}

/// Compute the first observed fork in record order. The common prefix includes
/// the fork parent; each branch contains its whole reachable subtree. The leaf
/// hint only chooses the default-expanded branch and never hides another path.
pub fn compute_fork_view(
    records: &[NativeDagRecord],
    leaf_hint: Option<&str>,
) -> Option<NativeForkView> {
    let mut parents = HashMap::<String, Option<String>>::new();
    let mut children = HashMap::<String, Vec<String>>::new();
    let mut order = Vec::new();

    for record in records {
        if parents.contains_key(&record.uuid) {
            continue;
        }
        parents.insert(record.uuid.clone(), record.parent_uuid.clone());
        order.push(record.uuid.clone());
        if let Some(parent) = &record.parent_uuid {
            let siblings = children.entry(parent.clone()).or_default();
            if !siblings.contains(&record.uuid) {
                siblings.push(record.uuid.clone());
            }
        }
    }

    let fork_parent_uuid = order
        .iter()
        .find(|uuid| children.get(*uuid).is_some_and(|items| items.len() > 1))?
        .clone();

    let mut prefix_uuids = vec![fork_parent_uuid.clone()];
    let mut cursor = parents.get(&fork_parent_uuid).cloned().flatten();
    let mut seen = HashSet::new();
    while let Some(uuid) = cursor {
        if !seen.insert(uuid.clone()) {
            break;
        }
        prefix_uuids.push(uuid.clone());
        cursor = parents.get(&uuid).cloned().flatten();
    }
    prefix_uuids.reverse();

    let branches = children
        .get(&fork_parent_uuid)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, root_uuid)| {
            let mut stack = vec![root_uuid.clone()];
            let mut node_uuids = Vec::new();
            let mut leaf_uuids = Vec::new();
            while let Some(uuid) = stack.pop() {
                node_uuids.push(uuid.clone());
                match children.get(&uuid) {
                    Some(next) if !next.is_empty() => {
                        for child in next.iter().rev() {
                            stack.push(child.clone());
                        }
                    }
                    _ => leaf_uuids.push(uuid),
                }
            }
            NativeForkBranch {
                label: format!("Branch {}", index + 1),
                root_uuid: root_uuid.clone(),
                node_uuids,
                leaf_uuids,
            }
        })
        .collect::<Vec<_>>();

    let default_branch = leaf_hint.and_then(|hint| {
        branches
            .iter()
            .position(|branch| branch.node_uuids.iter().any(|uuid| uuid == hint))
    });

    Some(NativeForkView {
        fork_parent_uuid,
        prefix_uuids,
        branches,
        default_branch,
    })
}

#[cfg(test)]
mod tests {
    use executors::executors::claude::native::adapt_native_claude_line;

    use super::*;

    const FIXTURE: &str = include_str!(
        "../../../../../docs/superpowers/specs/evidence/2026-07-20-cli-ui-seam/evidence-transcript.redacted.jsonl"
    );
    const SID: &str = "06a7eacd-664b-4d9c-83f3-d4774a6216a8";

    fn fixture_graph(
        lines: impl Iterator<Item = &'static str>,
    ) -> (Vec<NativeDagRecord>, Option<String>) {
        let mut records = Vec::new();
        let mut leaf_hint = None;
        for raw in lines {
            let line = adapt_native_claude_line(raw, SID).expect("fixture line must parse");
            let metadata = line.metadata();
            if let Some(uuid) = &metadata.uuid {
                records.push(NativeDagRecord {
                    uuid: uuid.clone(),
                    parent_uuid: metadata.parent_uuid.clone(),
                });
            }
            if let Some(leaf_uuid) = &metadata.leaf_uuid {
                leaf_hint = Some(leaf_uuid.clone());
            }
        }
        (records, leaf_hint)
    }

    #[test]
    fn every_committed_fixture_line_parses_tolerantly() {
        for (index, raw) in FIXTURE.lines().enumerate() {
            adapt_native_claude_line(raw, SID)
                .unwrap_or_else(|error| panic!("fixture line {} failed: {error}", index + 1));
        }
    }

    #[test]
    fn t4_fork_has_common_prefix_and_both_original_leaves() {
        // T4b ends at the second branch's 13cd... system leaf. T4c appends a
        // later continuation to the 975a... branch, so the exact empirical
        // two-leaf assertion intentionally uses the T4b prefix of the fixture.
        let (records, _) = fixture_graph(FIXTURE.lines().take(75));
        let fork = compute_fork_view(&records, Some("13cd5918-91e6-4297-8034-1794ae421c27"))
            .expect("fork must be detected");
        assert_eq!(
            fork.fork_parent_uuid,
            "e647b0e3-d826-4215-ab4c-737dcef52946"
        );
        let leaves = fork
            .branches
            .iter()
            .flat_map(|branch| branch.leaf_uuids.iter().map(String::as_str))
            .collect::<HashSet<_>>();
        assert_eq!(
            leaves,
            HashSet::from([
                "975a2278-7c3b-421c-91aa-02848ecb5e24",
                "13cd5918-91e6-4297-8034-1794ae421c27",
            ])
        );
        assert_eq!(fork.default_branch, Some(1));

        let (full_records, full_hint) = fixture_graph(FIXTURE.lines());
        let full = compute_fork_view(&full_records, full_hint.as_deref()).unwrap();
        assert_eq!(full.default_branch, Some(1));
        assert_eq!(full.branches.len(), 2);
    }
}
