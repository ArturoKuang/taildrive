use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use taildrive_core::path::{DrivePath, conflict_path, parse_path};
use taildrive_ffi::{FileType, SyncAction, SyncEntry, plan_folder_sync};

#[derive(Clone, Debug, PartialEq, Eq)]
enum TreeNode {
    File(String),
    Folder,
}

type Tree = BTreeMap<DrivePath, TreeNode>;

const HASHES: [&str; 4] = [
    "1111111111111111111111111111111111111111111111111111111111111111",
    "2222222222222222222222222222222222222222222222222222222222222222",
    "3333333333333333333333333333333333333333333333333333333333333333",
    "4444444444444444444444444444444444444444444444444444444444444444",
];

/// Fixed nested universe of paths covering depths 0..=3, prefix-adjacent siblings (`a` vs `a-2`),
/// dotfiles (`.env`), and Unicode NFC paths (`café/note`).
struct PathSpec {
    path: &'static str,
    children: &'static [PathSpec],
}

const UNIVERSE: &[PathSpec] = &[
    PathSpec {
        path: "a",
        children: &[
            PathSpec {
                path: "a/x",
                children: &[],
            },
            PathSpec {
                path: "a/y",
                children: &[],
            },
            PathSpec {
                path: "a/sub",
                children: &[
                    PathSpec {
                        path: "a/sub/y",
                        children: &[],
                    },
                    PathSpec {
                        path: "a/sub/deep",
                        children: &[PathSpec {
                            path: "a/sub/deep/f",
                            children: &[],
                        }],
                    },
                ],
            },
        ],
    },
    PathSpec {
        path: "a-2",
        children: &[],
    },
    PathSpec {
        path: "b",
        children: &[
            PathSpec {
                path: "b/z",
                children: &[],
            },
            PathSpec {
                path: "b/w",
                children: &[],
            },
        ],
    },
    PathSpec {
        path: ".env",
        children: &[],
    },
    PathSpec {
        path: "caf\u{00e9}",
        children: &[PathSpec {
            path: "caf\u{00e9}/note",
            children: &[],
        }],
    },
];

const fn count_nodes(specs: &[PathSpec]) -> usize {
    let mut total = 0;
    let mut i = 0;
    while i < specs.len() {
        total += 1 + count_nodes(specs[i].children);
        i += 1;
    }
    total
}

const NODE_COUNT: usize = count_nodes(UNIVERSE);

/// Per-node choice: 0..=1 Absent, 2..=5 File(hash), 6..=8 Folder (for branch nodes).
#[derive(Clone, Copy, Debug)]
enum NodeChoice {
    Absent,
    File(usize),
    Folder,
}

fn arb_node_choice() -> impl Strategy<Value = NodeChoice> {
    prop_oneof![
        3 => Just(NodeChoice::Absent),
        4 => (0usize..HASHES.len()).prop_map(NodeChoice::File),
        4 => Just(NodeChoice::Folder),
    ]
}

fn build_tree_from_choices(choices: &[NodeChoice]) -> Tree {
    fn visit(
        specs: &[PathSpec],
        choices: &[NodeChoice],
        idx: &mut usize,
        out: &mut Tree,
        include: bool,
    ) {
        for spec in specs {
            let choice = choices[*idx];
            *idx += 1;
            let recurse = if include {
                let p = parse_path(spec.path).expect("valid universe path");
                match choice {
                    NodeChoice::Absent => false,
                    NodeChoice::File(h) => {
                        out.insert(p, TreeNode::File(HASHES[h].to_owned()));
                        false
                    }
                    NodeChoice::Folder => {
                        out.insert(p, TreeNode::Folder);
                        true
                    }
                }
            } else {
                false
            };
            visit(spec.children, choices, idx, out, recurse);
        }
    }

    let mut tree = Tree::new();
    let mut idx = 0;
    visit(UNIVERSE, choices, &mut idx, &mut tree, true);
    assert_eq!(idx, NODE_COUNT);
    tree
}

fn arb_choices() -> BoxedStrategy<Vec<NodeChoice>> {
    prop::collection::vec(arb_node_choice(), NODE_COUNT).boxed()
}

fn arb_well_formed_tree() -> BoxedStrategy<Tree> {
    arb_choices()
        .prop_map(|choices| build_tree_from_choices(&choices))
        .boxed()
}

/// Generates `(local, remote, baseline)` triples mixing both completely independent trees
/// and baseline-derived mutated trees (including empty baseline for fresh clients).
fn arb_sync_scenario() -> BoxedStrategy<(Tree, Tree, Tree)> {
    let independent = (
        arb_well_formed_tree(),
        arb_well_formed_tree(),
        arb_well_formed_tree(),
    )
        .boxed();
    let fresh_client = (
        arb_well_formed_tree(),
        arb_well_formed_tree(),
        Just(Tree::new()),
    )
        .boxed();
    let baseline_derived = (
        arb_choices(),
        prop::collection::vec(prop::option::of(arb_node_choice()), NODE_COUNT),
        prop::collection::vec(prop::option::of(arb_node_choice()), NODE_COUNT),
    )
        .prop_map(|(base_c, local_mut, remote_mut)| {
            let mut lc = base_c.clone();
            let mut rc = base_c.clone();
            for i in 0..NODE_COUNT {
                if let Some(m) = local_mut[i] {
                    lc[i] = m;
                }
                if let Some(m) = remote_mut[i] {
                    rc[i] = m;
                }
            }
            (
                build_tree_from_choices(&lc),
                build_tree_from_choices(&rc),
                build_tree_from_choices(&base_c),
            )
        })
        .boxed();

    prop_oneof![
        3 => independent,
        1 => fresh_client,
        3 => baseline_derived,
    ]
    .boxed()
}

fn to_entries(tree: &Tree) -> Vec<SyncEntry> {
    tree.iter()
        .map(|(path, node)| match node {
            TreeNode::File(sha) => SyncEntry {
                path: path.to_string(),
                kind: FileType::File,
                sha256: Some(sha.clone()),
            },
            TreeNode::Folder => SyncEntry {
                path: path.to_string(),
                kind: FileType::Folder,
                sha256: None,
            },
        })
        .collect()
}

fn is_well_formed(tree: &Tree) -> Result<(), String> {
    for path in tree.keys() {
        for anc in path.ancestors() {
            match tree.get(&anc) {
                Some(TreeNode::Folder) => {}
                Some(TreeNode::File(_)) => {
                    return Err(format!(
                        "ill-formed tree: entry `{path}` lives under file `{anc}`"
                    ));
                }
                None => {
                    return Err(format!(
                        "ill-formed tree: entry `{path}` is missing ancestor folder `{anc}`"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn action_path(action: &SyncAction) -> DrivePath {
    let raw = match action {
        SyncAction::Upload { path }
        | SyncAction::Download { path }
        | SyncAction::CreateRemoteFolder { path }
        | SyncAction::CreateLocalFolder { path }
        | SyncAction::DeleteRemote { path }
        | SyncAction::DeleteLocal { path }
        | SyncAction::Conflict { path } => path,
    };
    parse_path(raw).expect("action emitted valid DrivePath")
}

fn is_delete(action: &SyncAction) -> bool {
    matches!(
        action,
        SyncAction::DeleteRemote { .. } | SyncAction::DeleteLocal { .. }
    )
}

fn is_proper_ancestor(anc: &DrivePath, desc: &DrivePath) -> bool {
    desc.as_str().len() > anc.as_str().len()
        && desc.as_str().starts_with(anc.as_str())
        && desc.as_str().as_bytes()[anc.as_str().len()] == b'/'
}

fn has_children(tree: &Tree, dir: &DrivePath) -> bool {
    tree.keys().any(|p| is_proper_ancestor(dir, p))
}

fn kind_of(node: &TreeNode) -> FileType {
    match node {
        TreeNode::File(_) => FileType::File,
        TreeNode::Folder => FileType::Folder,
    }
}

/// Checks ALL structural, topological, safety, and liveness invariants of `plan_folder_sync`.
fn verify_plan_invariants(
    local: &Tree,
    remote: &Tree,
    baseline: &Tree,
    actions: &[SyncAction],
) -> Result<(), String> {
    is_well_formed(local)?;
    is_well_formed(remote)?;
    is_well_formed(baseline)?;

    // 1. Unique action per path and phase separation (all forward actions before all deletes).
    let mut seen_paths: BTreeMap<DrivePath, (usize, &SyncAction)> = BTreeMap::new();
    let mut seen_delete = false;
    for (idx, action) in actions.iter().enumerate() {
        let p = action_path(action);
        if seen_paths.insert(p.clone(), (idx, action)).is_some() {
            return Err(format!("duplicate action for path `{p}`"));
        }
        if is_delete(action) {
            seen_delete = true;
        } else if seen_delete {
            return Err(format!(
                "forward action {action:?} at index {idx} appeared after a delete action"
            ));
        }
    }

    // 2. Topological ordering invariants.
    for (idx, action) in actions.iter().enumerate() {
        let p = action_path(action);
        match action {
            SyncAction::CreateRemoteFolder { .. } => {
                // Must precede any remote creation/upload of descendants.
                for (other_idx, other) in actions.iter().enumerate() {
                    if matches!(
                        other,
                        SyncAction::Upload { .. } | SyncAction::CreateRemoteFolder { .. }
                    ) {
                        let q = action_path(other);
                        if is_proper_ancestor(&p, &q) && idx >= other_idx {
                            return Err(format!(
                                "CreateRemoteFolder(`{p}`) at {idx} must precede {other:?} at {other_idx}"
                            ));
                        }
                    }
                }
            }
            SyncAction::CreateLocalFolder { .. } => {
                // Must precede any local creation/download of descendants.
                for (other_idx, other) in actions.iter().enumerate() {
                    if matches!(
                        other,
                        SyncAction::Download { .. } | SyncAction::CreateLocalFolder { .. }
                    ) {
                        let q = action_path(other);
                        if is_proper_ancestor(&p, &q) && idx >= other_idx {
                            return Err(format!(
                                "CreateLocalFolder(`{p}`) at {idx} must precede {other:?} at {other_idx}"
                            ));
                        }
                    }
                }
            }
            SyncAction::DeleteRemote { .. } => {
                // Must appear AFTER all DeleteRemote of its descendants.
                for (other_idx, other) in actions.iter().enumerate() {
                    if matches!(other, SyncAction::DeleteRemote { .. }) {
                        let q = action_path(other);
                        if is_proper_ancestor(&p, &q) && idx <= other_idx {
                            return Err(format!(
                                "DeleteRemote(`{p}`) at {idx} must appear AFTER descendant DeleteRemote(`{q}`) at {other_idx}"
                            ));
                        }
                    }
                }
            }
            SyncAction::DeleteLocal { .. } => {
                // Must appear AFTER all DeleteLocal of its descendants.
                for (other_idx, other) in actions.iter().enumerate() {
                    if matches!(other, SyncAction::DeleteLocal { .. }) {
                        let q = action_path(other);
                        if is_proper_ancestor(&p, &q) && idx <= other_idx {
                            return Err(format!(
                                "DeleteLocal(`{p}`) at {idx} must appear AFTER descendant DeleteLocal(`{q}`) at {other_idx}"
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 3. Kind-conflict subtree isolation.
    let mut top_kind_conflicts: BTreeSet<DrivePath> = BTreeSet::new();
    let all_paths: BTreeSet<DrivePath> = local
        .keys()
        .chain(remote.keys())
        .chain(baseline.keys())
        .cloned()
        .collect();
    for p in &all_paths {
        if p.ancestors()
            .iter()
            .any(|anc| top_kind_conflicts.contains(anc))
        {
            continue;
        }
        if let (Some(l), Some(r)) = (local.get(p), remote.get(p))
            && kind_of(l) != kind_of(r)
        {
            top_kind_conflicts.insert(p.clone());
        }
    }
    for kc in &top_kind_conflicts {
        match seen_paths.get(kc) {
            Some((_, SyncAction::Conflict { .. })) => {}
            other => {
                return Err(format!(
                    "kind conflict at `{kc}` did not emit Conflict (got {other:?})"
                ));
            }
        }
        for action in actions {
            let q = action_path(action);
            if is_proper_ancestor(kc, &q) {
                return Err(format!(
                    "descendant `{q}` of kind-conflict `{kc}` leaked into actions: {action:?}"
                ));
            }
        }
    }

    // 4. Zero silent edit loss.
    for (p, node) in local {
        if let TreeNode::File(h_l) = node {
            let unmodified = matches!(baseline.get(p), Some(TreeNode::File(h_b)) if h_b == h_l);
            if !unmodified {
                if matches!(seen_paths.get(p), Some((_, SyncAction::DeleteLocal { .. }))) {
                    return Err(format!(
                        "silent local edit loss: DeleteLocal emitted for modified/new file `{p}`"
                    ));
                }
                for anc in p.ancestors() {
                    if matches!(
                        seen_paths.get(&anc),
                        Some((_, SyncAction::DeleteLocal { .. }))
                    ) {
                        return Err(format!(
                            "silent local edit loss: DeleteLocal emitted for ancestor `{anc}` of modified file `{p}`"
                        ));
                    }
                }
            }
        }
    }
    for (p, node) in remote {
        if let TreeNode::File(h_r) = node {
            let unmodified = matches!(baseline.get(p), Some(TreeNode::File(h_b)) if h_b == h_r);
            if !unmodified {
                if matches!(
                    seen_paths.get(p),
                    Some((_, SyncAction::DeleteRemote { .. }))
                ) {
                    return Err(format!(
                        "silent remote edit loss: DeleteRemote emitted for modified/new file `{p}`"
                    ));
                }
                for anc in p.ancestors() {
                    if matches!(
                        seen_paths.get(&anc),
                        Some((_, SyncAction::DeleteRemote { .. }))
                    ) {
                        return Err(format!(
                            "silent remote edit loss: DeleteRemote emitted for ancestor `{anc}` of modified file `{p}`"
                        ));
                    }
                }
            }
        }
    }

    // 5. No resurrection of clean delete (outside kind-conflict subtrees).
    for (p, base_node) in baseline {
        let TreeNode::File(h_b) = base_node else {
            continue;
        };
        if p.ancestors()
            .iter()
            .any(|anc| top_kind_conflicts.contains(anc))
            || top_kind_conflicts.contains(p)
        {
            continue;
        }
        if !local.contains_key(p)
            && matches!(remote.get(p), Some(TreeNode::File(h_r)) if h_r == h_b)
            && !matches!(
                seen_paths.get(p),
                Some((_, SyncAction::DeleteRemote { .. }))
            )
        {
            return Err(format!(
                "clean local delete of `{p}` failed to emit DeleteRemote (got {:?})",
                seen_paths.get(p)
            ));
        }
        if !remote.contains_key(p)
            && matches!(local.get(p), Some(TreeNode::File(h_l)) if h_l == h_b)
            && !matches!(seen_paths.get(p), Some((_, SyncAction::DeleteLocal { .. })))
        {
            return Err(format!(
                "clean remote delete of `{p}` failed to emit DeleteLocal (got {:?})",
                seen_paths.get(p)
            ));
        }
    }

    Ok(())
}

/// Applies `actions` in exact emitted order to simulated `(local, remote)` trees, enforcing
/// strict filesystem preconditions at every step.
///
/// On `Conflict { path }`:
/// - Local's entry (or subtree, for a folder-vs-file kind conflict) is preserved at a conflict
///   sibling computed via `taildrive_core::path::conflict_path`, written locally (and optionally
///   mirrored to remote when `mirror_conflicts_immediately` is true).
/// - Remote's entry (or subtree, for a file-vs-folder kind conflict) is adopted at `path`.
fn execute_plan(
    initial_local: &Tree,
    initial_remote: &Tree,
    actions: &[SyncAction],
    mirror_conflicts_immediately: bool,
) -> Result<(Tree, Tree, BTreeMap<DrivePath, DrivePath>), String> {
    let mut sim_local = initial_local.clone();
    let mut sim_remote = initial_remote.clone();
    let mut conflict_siblings: BTreeMap<DrivePath, DrivePath> = BTreeMap::new();

    for (idx, action) in actions.iter().enumerate() {
        let p = action_path(action);
        match action {
            SyncAction::CreateRemoteFolder { .. } => {
                if !p.parent().is_root()
                    && !matches!(sim_remote.get(&p.parent()), Some(TreeNode::Folder))
                {
                    return Err(format!(
                        "step {idx}: CreateRemoteFolder(`{p}`) missing remote parent folder `{}`",
                        p.parent()
                    ));
                }
                if matches!(sim_remote.get(&p), Some(TreeNode::File(_))) {
                    return Err(format!(
                        "step {idx}: CreateRemoteFolder(`{p}`) collided with remote file"
                    ));
                }
                sim_remote.insert(p, TreeNode::Folder);
            }
            SyncAction::CreateLocalFolder { .. } => {
                if !p.parent().is_root()
                    && !matches!(sim_local.get(&p.parent()), Some(TreeNode::Folder))
                {
                    return Err(format!(
                        "step {idx}: CreateLocalFolder(`{p}`) missing local parent folder `{}`",
                        p.parent()
                    ));
                }
                if matches!(sim_local.get(&p), Some(TreeNode::File(_))) {
                    return Err(format!(
                        "step {idx}: CreateLocalFolder(`{p}`) collided with local file"
                    ));
                }
                sim_local.insert(p, TreeNode::Folder);
            }
            SyncAction::Upload { .. } => {
                let Some(TreeNode::File(hash)) = sim_local.get(&p).cloned() else {
                    return Err(format!("step {idx}: Upload(`{p}`) source not a local file"));
                };
                if !p.parent().is_root()
                    && !matches!(sim_remote.get(&p.parent()), Some(TreeNode::Folder))
                {
                    return Err(format!(
                        "step {idx}: Upload(`{p}`) missing remote parent folder `{}`",
                        p.parent()
                    ));
                }
                if matches!(sim_remote.get(&p), Some(TreeNode::Folder)) {
                    return Err(format!(
                        "step {idx}: Upload(`{p}`) target is a remote folder"
                    ));
                }
                sim_remote.insert(p, TreeNode::File(hash));
            }
            SyncAction::Download { .. } => {
                let Some(TreeNode::File(hash)) = sim_remote.get(&p).cloned() else {
                    return Err(format!(
                        "step {idx}: Download(`{p}`) source not a remote file"
                    ));
                };
                if !p.parent().is_root()
                    && !matches!(sim_local.get(&p.parent()), Some(TreeNode::Folder))
                {
                    return Err(format!(
                        "step {idx}: Download(`{p}`) missing local parent folder `{}`",
                        p.parent()
                    ));
                }
                if matches!(sim_local.get(&p), Some(TreeNode::Folder)) {
                    return Err(format!(
                        "step {idx}: Download(`{p}`) target is a local folder"
                    ));
                }
                sim_local.insert(p, TreeNode::File(hash));
            }
            SyncAction::DeleteRemote { .. } => {
                let Some(existing) = sim_remote.get(&p).cloned() else {
                    return Err(format!("step {idx}: DeleteRemote(`{p}`) missing on remote"));
                };
                if existing == TreeNode::Folder && has_children(&sim_remote, &p) {
                    return Err(format!(
                        "step {idx}: DeleteRemote(`{p}`) attempted on non-empty remote folder"
                    ));
                }
                sim_remote.remove(&p);
            }
            SyncAction::DeleteLocal { .. } => {
                let Some(existing) = sim_local.get(&p).cloned() else {
                    return Err(format!("step {idx}: DeleteLocal(`{p}`) missing on local"));
                };
                if existing == TreeNode::Folder && has_children(&sim_local, &p) {
                    return Err(format!(
                        "step {idx}: DeleteLocal(`{p}`) attempted on non-empty local folder"
                    ));
                }
                sim_local.remove(&p);
            }
            SyncAction::Conflict { .. } => {
                if !sim_local.contains_key(&p) || !sim_remote.contains_key(&p) {
                    return Err(format!(
                        "step {idx}: Conflict(`{p}`) requires both local and remote entries"
                    ));
                }
                let sibling = conflict_path(&p, "local-device", "20261128T000000Z");
                assert_ne!(sibling, p);
                assert_eq!(sibling.parent(), p.parent());
                conflict_siblings.insert(p.clone(), sibling.clone());

                // Extract local entry + any local descendants under `p` and relocate to `sibling`.
                let local_subtree: Vec<(DrivePath, TreeNode)> = sim_local
                    .iter()
                    .filter(|(k, _)| *k == &p || is_proper_ancestor(&p, k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (k, _) in &local_subtree {
                    sim_local.remove(k);
                }
                for (k, v) in local_subtree {
                    let relocated = if k == p {
                        sibling.clone()
                    } else {
                        let suffix = &k.as_str()[p.as_str().len()..];
                        parse_path(&format!("{}{suffix}", sibling.as_str()))
                            .expect("relocated conflict descendant is valid")
                    };
                    sim_local.insert(relocated.clone(), v.clone());
                    if mirror_conflicts_immediately {
                        sim_remote.insert(relocated, v);
                    }
                }

                // Adopt remote entry (and remote subtree if remote is a folder) at `p`.
                let remote_subtree: Vec<(DrivePath, TreeNode)> = sim_remote
                    .iter()
                    .filter(|(k, _)| *k == &p || is_proper_ancestor(&p, k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (k, v) in remote_subtree {
                    sim_local.insert(k, v);
                }
            }
        }
    }

    Ok((sim_local, sim_remote, conflict_siblings))
}

/// Checks that no modified file content from either `local` or `remote` was lost in the post-sync tree.
fn verify_no_content_lost(
    initial_local: &Tree,
    initial_remote: &Tree,
    baseline: &Tree,
    post_local: &Tree,
    post_remote: &Tree,
    conflict_siblings: &BTreeMap<DrivePath, DrivePath>,
) -> Result<(), String> {
    // Check every local file that was new or modified vs baseline.
    for (p, node) in initial_local {
        let TreeNode::File(h_l) = node else {
            continue;
        };
        let unmodified = matches!(baseline.get(p), Some(TreeNode::File(h_b)) if h_b == h_l);
        if unmodified {
            continue;
        }
        // Either still at `p`, or preserved at a conflict sibling for `p` or an ancestor of `p`.
        let at_original = matches!(post_local.get(p), Some(TreeNode::File(h)) if h == h_l);
        let at_conflict = conflict_siblings.iter().any(|(cp, sib)| {
            let expected_path = if cp == p {
                Some(sib.clone())
            } else if is_proper_ancestor(cp, p) {
                let suffix = &p.as_str()[cp.as_str().len()..];
                parse_path(&format!("{}{suffix}", sib.as_str())).ok()
            } else {
                None
            };
            expected_path.is_some_and(|ep| {
                matches!(post_local.get(&ep), Some(TreeNode::File(h)) if h == h_l)
                    && matches!(post_remote.get(&ep), Some(TreeNode::File(h)) if h == h_l)
            })
        });
        if !at_original && !at_conflict {
            return Err(format!(
                "lost modified local file `{p}` (hash {h_l}); post_local={post_local:?}"
            ));
        }
    }

    // Check every remote file that was new or modified vs baseline.
    for (p, node) in initial_remote {
        let TreeNode::File(h_r) = node else {
            continue;
        };
        let unmodified = matches!(baseline.get(p), Some(TreeNode::File(h_b)) if h_b == h_r);
        if unmodified {
            continue;
        }
        // Remote wins conflicts at `p`, so modified remote files MUST be at `p` on both sides.
        let on_local = matches!(post_local.get(p), Some(TreeNode::File(h)) if h == h_r);
        let on_remote = matches!(post_remote.get(p), Some(TreeNode::File(h)) if h == h_r);
        if !on_local || !on_remote {
            return Err(format!(
                "lost modified remote file `{p}` (hash {h_r}): local={:?}, remote={:?}",
                post_local.get(p),
                post_remote.get(p)
            ));
        }
    }

    Ok(())
}

fn formal_cases(default_cases: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .or_else(|| std::env::var("FORMAL_CASES").ok())
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default_cases)
}

fn formal_config(default_cases: u32) -> ProptestConfig {
    let mut cfg = ProptestConfig {
        cases: formal_cases(default_cases),
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    if let Ok(seed_str) = std::env::var("FORMAL_SEED")
        && let Ok(seed) = seed_str.parse::<u64>()
    {
        cfg.rng_seed = proptest::test_runner::RngSeed::Fixed(seed);
    }
    cfg
}

fn flip_action(action: &SyncAction) -> SyncAction {
    match action {
        SyncAction::Upload { path } => SyncAction::Download { path: path.clone() },
        SyncAction::Download { path } => SyncAction::Upload { path: path.clone() },
        SyncAction::CreateRemoteFolder { path } => {
            SyncAction::CreateLocalFolder { path: path.clone() }
        }
        SyncAction::CreateLocalFolder { path } => {
            SyncAction::CreateRemoteFolder { path: path.clone() }
        }
        SyncAction::DeleteRemote { path } => SyncAction::DeleteLocal { path: path.clone() },
        SyncAction::DeleteLocal { path } => SyncAction::DeleteRemote { path: path.clone() },
        SyncAction::Conflict { path } => SyncAction::Conflict { path: path.clone() },
    }
}

fn to_nfd_entries(tree: &Tree) -> Vec<SyncEntry> {
    tree.iter()
        .map(|(path, node)| {
            let nfd_path = path.as_str().replace('\u{00e9}', "e\u{0301}");
            match node {
                TreeNode::File(sha) => SyncEntry {
                    path: nfd_path,
                    kind: FileType::File,
                    sha256: Some(sha.clone()),
                },
                TreeNode::Folder => SyncEntry {
                    path: nfd_path,
                    kind: FileType::Folder,
                    sha256: None,
                },
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(formal_config(512))]

    #[test]
    fn plan_folder_sync_invariants_and_refinement_convergence(
        (local, remote, baseline) in arb_sync_scenario()
    ) {
        let actions = plan_folder_sync(
            to_entries(&local),
            to_entries(&remote),
            to_entries(&baseline),
        )
        .expect("well-formed trees must plan cleanly");

        // Verify all formal plan invariants.
        prop_assert_eq!(
            verify_plan_invariants(&local, &remote, &baseline, &actions),
            Ok(())
        );

        // Mirror symmetry: swapping local & remote must emit the exact dual action sequence.
        let dual_actions = plan_folder_sync(
            to_entries(&remote),
            to_entries(&local),
            to_entries(&baseline),
        )
        .expect("dual trees must plan cleanly");
        let expected_dual: Vec<SyncAction> = actions.iter().map(flip_action).collect();
        prop_assert_eq!(dual_actions, expected_dual);

        // NFC/NFD normalization invariance: macOS NFD local paths plan identically to NFC paths.
        let nfd_actions = plan_folder_sync(
            to_nfd_entries(&local),
            to_entries(&remote),
            to_entries(&baseline),
        )
        .expect("NFD local entries must plan identically to NFC");
        prop_assert_eq!(&nfd_actions, &actions);

        // Execute plan with conflict copies mirrored to remote (single-pass convergence).
        let (post_local, post_remote, siblings) =
            execute_plan(&local, &remote, &actions, true)
                .expect("plan actions must execute cleanly in emitted order");

        // (a) Post-sync trees are well-formed and identical.
        prop_assert_eq!(is_well_formed(&post_local), Ok(()));
        prop_assert_eq!(is_well_formed(&post_remote), Ok(()));
        prop_assert_eq!(&post_local, &post_remote);

        // (b) Zero modified file content from either side was lost.
        prop_assert_eq!(
            verify_no_content_lost(&local, &remote, &baseline, &post_local, &post_remote, &siblings),
            Ok(())
        );

        // (c) Fixed point / idempotency: running plan_folder_sync again emits empty actions
        // both with updated baseline AND with old baseline.
        let second_with_new_base = plan_folder_sync(
            to_entries(&post_local),
            to_entries(&post_remote),
            to_entries(&post_local),
        )
        .expect("post-sync plan with new baseline");
        prop_assert!(
            second_with_new_base.is_empty(),
            "expected fixed point (empty actions), got {second_with_new_base:?}"
        );

        let second_with_old_base = plan_folder_sync(
            to_entries(&post_local),
            to_entries(&post_remote),
            to_entries(&baseline),
        )
        .expect("post-sync plan with old baseline");
        prop_assert!(
            second_with_old_base.is_empty(),
            "expected fixed point even with old baseline, got {second_with_old_base:?}"
        );

        // Realistic two-pass client check: when Conflict only saves the conflict copy locally
        // (without immediately uploading it), a follow-up sync pass uploads ONLY the conflict copies
        // and then reaches an exact empty fixed point!
        if !siblings.is_empty() {
            let (mid_local, mid_remote, _) =
                execute_plan(&local, &remote, &actions, false)
                    .expect("local-only conflict preservation must succeed");
            prop_assert_eq!(is_well_formed(&mid_local), Ok(()));
            prop_assert_eq!(is_well_formed(&mid_remote), Ok(()));

            let pass2 = plan_folder_sync(
                to_entries(&mid_local),
                to_entries(&mid_remote),
                to_entries(&mid_remote),
            )
            .expect("second pass planning");
            for act in &pass2 {
                prop_assert!(
                    matches!(act, SyncAction::Upload { .. } | SyncAction::CreateRemoteFolder { .. }),
                    "pass2 should only upload preserved conflict copies, got {act:?}"
                );
            }
            let (final_local, final_remote, _) =
                execute_plan(&mid_local, &mid_remote, &pass2, false)
                    .expect("second pass execution");
            prop_assert_eq!(&final_local, &final_remote);
            prop_assert_eq!(is_well_formed(&final_local), Ok(()));

            let pass3 = plan_folder_sync(
                to_entries(&final_local),
                to_entries(&final_remote),
                to_entries(&final_local),
            )
            .expect("third pass fixed point");
            prop_assert!(pass3.is_empty(), "expected fixed point after pass2, got {pass3:?}");
        }
    }
}

/// Canary self-tests proving the invariant checker rejects broken plans.
#[test]
fn canary_invariant_checker_catches_broken_plans() {
    let p = |s: &str| parse_path(s).unwrap();
    let h1 = HASHES[0].to_owned();
    let h2 = HASHES[1].to_owned();

    // 1. Inverted topological order (Upload before CreateRemoteFolder)
    let mut local = Tree::new();
    local.insert(p("a"), TreeNode::Folder);
    local.insert(p("a/x"), TreeNode::File(h1.clone()));
    let bad_order = vec![
        SyncAction::Upload { path: "a/x".into() },
        SyncAction::CreateRemoteFolder { path: "a".into() },
    ];
    assert!(
        verify_plan_invariants(&local, &Tree::new(), &Tree::new(), &bad_order).is_err(),
        "canary must reject Upload before CreateRemoteFolder"
    );

    // 2. Inverted delete order (DeleteRemote parent before child)
    let mut remote = Tree::new();
    remote.insert(p("a"), TreeNode::Folder);
    remote.insert(p("a/x"), TreeNode::File(h1.clone()));
    let bad_delete_order = vec![
        SyncAction::DeleteRemote { path: "a".into() },
        SyncAction::DeleteRemote { path: "a/x".into() },
    ];
    assert!(
        verify_plan_invariants(&Tree::new(), &remote, &remote, &bad_delete_order).is_err(),
        "canary must reject parent DeleteRemote before child"
    );

    // 3. Silent edit loss (DeleteLocal on modified local file)
    let mut baseline = Tree::new();
    baseline.insert(p("a"), TreeNode::File(h1.clone()));
    let mut modified_local = Tree::new();
    modified_local.insert(p("a"), TreeNode::File(h2.clone()));
    let bad_silent_loss = vec![SyncAction::DeleteLocal { path: "a".into() }];
    assert!(
        verify_plan_invariants(&modified_local, &Tree::new(), &baseline, &bad_silent_loss).is_err(),
        "canary must reject DeleteLocal on a modified local file"
    );

    // 4. Kind-conflict descendant leakage
    let mut kc_local = Tree::new();
    kc_local.insert(p("a"), TreeNode::Folder);
    kc_local.insert(p("a/x"), TreeNode::File(h1.clone()));
    let mut kc_remote = Tree::new();
    kc_remote.insert(p("a"), TreeNode::File(h2.clone()));
    let bad_leak = vec![
        SyncAction::Conflict { path: "a".into() },
        SyncAction::Upload { path: "a/x".into() },
    ];
    assert!(
        verify_plan_invariants(&kc_local, &kc_remote, &Tree::new(), &bad_leak).is_err(),
        "canary must reject descendant action under a kind conflict"
    );

    // 5. Resurrection of clean delete (missing DeleteRemote)
    assert!(
        verify_plan_invariants(&Tree::new(), &baseline, &baseline, &[]).is_err(),
        "canary must reject omitting DeleteRemote for a clean local delete"
    );
}
