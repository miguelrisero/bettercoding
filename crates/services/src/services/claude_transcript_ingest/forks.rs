use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDagRecord {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub kind: String,
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

fn is_conversational_kind(kind: &str) -> bool {
    matches!(kind, "user" | "assistant")
}

/// Compute the first observed conversational fork in record order. Raw
/// bookkeeping nodes preserve the topology, but only child subtrees containing
/// user or assistant records can form branches. The common prefix includes the
/// raw fork parent as its anchor; branch nodes and leaves are conversational.
/// The leaf hint only chooses the default-expanded branch and never hides
/// another path.
pub fn compute_fork_view(
    records: &[NativeDagRecord],
    leaf_hint: Option<&str>,
) -> Option<NativeForkView> {
    let mut parents = HashMap::<String, Option<String>>::new();
    let mut children = HashMap::<String, Vec<String>>::new();
    let mut conversational = HashSet::new();
    let mut order = Vec::new();

    for record in records {
        if parents.contains_key(&record.uuid) {
            continue;
        }
        parents.insert(record.uuid.clone(), record.parent_uuid.clone());
        if is_conversational_kind(&record.kind) {
            conversational.insert(record.uuid.clone());
        }
        order.push(record.uuid.clone());
        if let Some(parent) = &record.parent_uuid {
            let siblings = children.entry(parent.clone()).or_default();
            if !siblings.contains(&record.uuid) {
                siblings.push(record.uuid.clone());
            }
        }
    }

    // Mark the raw ancestry of every conversational record. This lets a
    // bookkeeping chain remain transparent while retaining the exact raw UUID
    // at which two conversational paths diverged.
    let mut contains_conversation = HashSet::new();
    for uuid in &order {
        if !conversational.contains(uuid) {
            continue;
        }
        let mut cursor = Some(uuid.clone());
        while let Some(current) = cursor {
            if !contains_conversation.insert(current.clone()) {
                break;
            }
            cursor = parents.get(&current).cloned().flatten();
        }
    }

    let (fork_parent_uuid, branch_roots) = order.iter().find_map(|uuid| {
        let roots = children
            .get(uuid)
            .into_iter()
            .flatten()
            .filter(|child| contains_conversation.contains(*child))
            .cloned()
            .collect::<Vec<_>>();
        (roots.len() > 1).then(|| (uuid.clone(), roots))
    })?;

    let mut prefix_uuids = vec![fork_parent_uuid.clone()];
    let mut cursor = parents.get(&fork_parent_uuid).cloned().flatten();
    let mut seen = HashSet::new();
    while let Some(uuid) = cursor {
        if !seen.insert(uuid.clone()) {
            break;
        }
        if conversational.contains(&uuid) {
            prefix_uuids.push(uuid.clone());
        }
        cursor = parents.get(&uuid).cloned().flatten();
    }
    prefix_uuids.reverse();

    let branch_data = branch_roots
        .iter()
        .enumerate()
        .map(|(index, raw_root_uuid)| {
            let mut stack = vec![raw_root_uuid.clone()];
            let mut raw_node_uuids = HashSet::new();
            let mut node_uuids = Vec::new();
            let mut leaf_uuids = Vec::new();
            while let Some(uuid) = stack.pop() {
                if !raw_node_uuids.insert(uuid.clone()) {
                    continue;
                }
                if conversational.contains(&uuid) {
                    node_uuids.push(uuid.clone());
                    let has_conversational_child = children.get(&uuid).is_some_and(|next| {
                        next.iter()
                            .any(|child| contains_conversation.contains(child))
                    });
                    if !has_conversational_child {
                        leaf_uuids.push(uuid.clone());
                    }
                }
                if let Some(next) = children.get(&uuid) {
                    for child in next.iter().rev() {
                        stack.push(child.clone());
                    }
                }
            }
            let root_uuid = node_uuids
                .first()
                .expect("a branch root is known to contain conversation")
                .clone();
            (
                NativeForkBranch {
                    label: format!("Branch {}", index + 1),
                    root_uuid,
                    node_uuids,
                    leaf_uuids,
                },
                raw_node_uuids,
            )
        })
        .collect::<Vec<_>>();

    let default_branch = leaf_hint.and_then(|hint| {
        branch_data
            .iter()
            .position(|(_, raw_node_uuids)| raw_node_uuids.contains(hint))
    });
    let branches = branch_data.into_iter().map(|(branch, _)| branch).collect();

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
                    kind: metadata.kind.clone(),
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
    fn bookkeeping_and_user_siblings_are_not_a_fork() {
        let records = vec![
            NativeDagRecord {
                uuid: "cce1aaac-parent".to_string(),
                parent_uuid: None,
                kind: "assistant".to_string(),
            },
            NativeDagRecord {
                uuid: "bookkeeping-system".to_string(),
                parent_uuid: Some("cce1aaac-parent".to_string()),
                kind: "system".to_string(),
            },
            NativeDagRecord {
                uuid: "next-user".to_string(),
                parent_uuid: Some("cce1aaac-parent".to_string()),
                kind: "user".to_string(),
            },
        ];

        assert_eq!(compute_fork_view(&records, None), None);
    }

    #[test]
    fn t4_fork_has_conversational_prefix_branches_and_leaves() {
        // T4b ends with bookkeeping leaves on both paths. The fork view keeps
        // those raw nodes only for the resume hint and exposes the two
        // conversational paths ending at 8f96... and eb19....
        let (records, _) = fixture_graph(FIXTURE.lines().take(75));
        let fork = compute_fork_view(&records, Some("13cd5918-91e6-4297-8034-1794ae421c27"))
            .expect("fork must be detected");
        assert_eq!(
            fork.fork_parent_uuid,
            "e647b0e3-d826-4215-ab4c-737dcef52946"
        );
        assert_eq!(
            fork.prefix_uuids,
            [
                "8251139a-95b2-452b-ba23-6240712442df",
                "31bd503f-0e19-4c9b-9a77-828a2538474f",
                "cabb1166-e3e4-4e86-a35d-7a0286b0123a",
                "578f5d58-6ee8-4674-a763-273c7f80c805",
                "5480e4c6-9dea-43ea-aa4a-2c51e6dcceb8",
                "78a9c044-4897-4b3d-aecb-f290ee8dfbf6",
                "ada417d5-de2c-4138-a545-c7234b8dd0af",
                "e56c2f14-b87a-4b63-9d93-6c7e601be948",
                "7c763840-73a0-48b9-b873-1336c32ba6be",
                "a82737f3-fd28-44f8-82b7-890ae134c52b",
                "b77a051f-e12e-4a24-be4b-f97404a59060",
                "e647b0e3-d826-4215-ab4c-737dcef52946",
            ]
        );
        assert_eq!(fork.branches.len(), 2);
        assert_eq!(
            fork.branches[0],
            NativeForkBranch {
                label: "Branch 1".to_string(),
                root_uuid: "d46cae0d-6204-4ccf-9378-8b1f10ad6be3".to_string(),
                node_uuids: vec![
                    "d46cae0d-6204-4ccf-9378-8b1f10ad6be3".to_string(),
                    "5fcb98a5-8935-48ee-b6d2-f26c3cacb78e".to_string(),
                    "8f967648-225b-44bc-9018-3d8915487f31".to_string(),
                ],
                leaf_uuids: vec!["8f967648-225b-44bc-9018-3d8915487f31".to_string()],
            }
        );
        assert_eq!(
            fork.branches[1],
            NativeForkBranch {
                label: "Branch 2".to_string(),
                root_uuid: "3c1bead3-d7ea-4c53-98dc-028379e0b345".to_string(),
                node_uuids: vec![
                    "3c1bead3-d7ea-4c53-98dc-028379e0b345".to_string(),
                    "13232afe-3dbc-486a-a7f5-5684cd32c248".to_string(),
                    "eb19b8a4-8e07-4b06-b292-0996d05afb19".to_string(),
                ],
                leaf_uuids: vec!["eb19b8a4-8e07-4b06-b292-0996d05afb19".to_string()],
            }
        );
        assert_eq!(fork.default_branch, Some(1));

        let (full_records, full_hint) = fixture_graph(FIXTURE.lines());
        let full = compute_fork_view(&full_records, full_hint.as_deref()).unwrap();
        assert_eq!(full.default_branch, Some(1));
        assert_eq!(full.branches.len(), 2);
    }
}
