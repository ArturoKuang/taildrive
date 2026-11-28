use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use taildrive_core::path::{DrivePath, parse_path};

use crate::{FfiError, FileType};

#[derive(uniffi::Record)]
pub struct SyncEntry {
    pub path: String,
    pub kind: FileType,
    pub sha256: Option<String>,
}

#[derive(Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncAction {
    Upload { path: String },
    Download { path: String },
    CreateRemoteFolder { path: String },
    CreateLocalFolder { path: String },
    DeleteRemote { path: String },
    DeleteLocal { path: String },
    Conflict { path: String },
}

#[uniffi::export]
pub fn plan_folder_sync(
    local_entries: Vec<SyncEntry>,
    remote_entries: Vec<SyncEntry>,
    baseline_entries: Vec<SyncEntry>,
) -> Result<Vec<SyncAction>, FfiError> {
    let local = index(&local_entries)?;
    let remote = index(&remote_entries)?;
    let baseline = index(&baseline_entries)?;
    let paths: BTreeSet<DrivePath> = local
        .keys()
        .chain(remote.keys())
        .chain(baseline.keys())
        .cloned()
        .collect();

    // Pass 1 (top-down): identify kind conflicts so their descendants are skipped.
    let mut conflicts = BTreeSet::new();
    let mut active_paths = Vec::with_capacity(paths.len());
    for path in &paths {
        if path
            .ancestors()
            .iter()
            .any(|parent| conflicts.contains(parent))
        {
            continue;
        }
        if let (Some(left), Some(right)) = (local.get(path), remote.get(path))
            && left.kind != right.kind
        {
            conflicts.insert(path.clone());
        }
        active_paths.push(path.clone());
    }

    // Pass 2 (bottom-up): decide each path's action so folder deletes only happen
    // when no descendant on that side is being kept.
    active_paths.sort_by(|a, b| {
        Reverse(depth(a))
            .cmp(&Reverse(depth(b)))
            .then_with(|| a.cmp(b))
    });

    let mut keep_remote_folders = BTreeSet::new();
    let mut keep_local_folders = BTreeSet::new();
    let mut forward_actions: BTreeMap<DrivePath, SyncAction> = BTreeMap::new();
    let mut delete_actions: Vec<(DrivePath, SyncAction)> = Vec::new();

    for path in active_paths {
        let name = path.to_string();
        let l = local.get(&path).copied();
        let r = remote.get(&path).copied();
        let b = baseline.get(&path).copied();

        let action = match (l, r, b) {
            (Some(left), Some(right), base) if left.kind != right.kind => {
                let _ = base;
                Some(SyncAction::Conflict { path: name })
            }
            (Some(left), Some(right), base) => {
                if left.sha256 == right.sha256 {
                    None
                } else {
                    let base_hash = base
                        .filter(|entry| entry.kind == FileType::File)
                        .and_then(|entry| entry.sha256.as_ref());
                    if base_hash == right.sha256.as_ref() {
                        Some(SyncAction::Upload { path: name })
                    } else if base_hash == left.sha256.as_ref() {
                        Some(SyncAction::Download { path: name })
                    } else {
                        Some(SyncAction::Conflict { path: name })
                    }
                }
            }
            (Some(entry), None, None) => match entry.kind {
                FileType::File => Some(SyncAction::Upload { path: name }),
                FileType::Folder => Some(SyncAction::CreateRemoteFolder { path: name }),
            },
            (None, Some(entry), None) => match entry.kind {
                FileType::File => Some(SyncAction::Download { path: name }),
                FileType::Folder => Some(SyncAction::CreateLocalFolder { path: name }),
            },
            (None, Some(entry), Some(base)) => match entry.kind {
                FileType::File => {
                    if base.kind == FileType::File && base.sha256 == entry.sha256 {
                        Some(SyncAction::DeleteRemote { path: name })
                    } else {
                        // Remote file was modified (or changed kind) since baseline: keep remote edit.
                        Some(SyncAction::Download { path: name })
                    }
                }
                FileType::Folder => {
                    if base.kind == FileType::Folder && !keep_remote_folders.contains(&path) {
                        Some(SyncAction::DeleteRemote { path: name })
                    } else {
                        // A remote descendant is being kept/downloaded (or kind changed): recreate locally.
                        Some(SyncAction::CreateLocalFolder { path: name })
                    }
                }
            },
            (Some(entry), None, Some(base)) => match entry.kind {
                FileType::File => {
                    if base.kind == FileType::File && base.sha256 == entry.sha256 {
                        Some(SyncAction::DeleteLocal { path: name })
                    } else {
                        // Local file was modified (or changed kind) since baseline: preserve local edit.
                        Some(SyncAction::Upload { path: name })
                    }
                }
                FileType::Folder => {
                    if base.kind == FileType::Folder && !keep_local_folders.contains(&path) {
                        Some(SyncAction::DeleteLocal { path: name })
                    } else {
                        // A local descendant is being kept/uploaded (or kind changed): recreate remotely.
                        Some(SyncAction::CreateRemoteFolder { path: name })
                    }
                }
            },
            (None, None, Some(_)) => None,
            (None, None, None) => unreachable!("path came from one of the indexes"),
        };

        let is_delete_remote = matches!(action, Some(SyncAction::DeleteRemote { .. }));
        let is_delete_local = matches!(action, Some(SyncAction::DeleteLocal { .. }));

        if r.is_some() && !is_delete_remote {
            for ancestor in path.ancestors() {
                keep_remote_folders.insert(ancestor);
            }
        }
        if l.is_some() && !is_delete_local {
            for ancestor in path.ancestors() {
                keep_local_folders.insert(ancestor);
            }
        }

        if let Some(action) = action {
            if is_delete_remote || is_delete_local {
                delete_actions.push((path, action));
            } else {
                forward_actions.insert(path, action);
            }
        }
    }

    // Pass 3: emit forward actions top-down, then delete actions bottom-up.
    let mut actions: Vec<SyncAction> = forward_actions.into_values().collect();
    actions.extend(delete_actions.into_iter().map(|(_, action)| action));
    Ok(actions)
}

fn depth(path: &DrivePath) -> usize {
    path.as_str().bytes().filter(|&b| b == b'/').count()
}

fn index(entries: &[SyncEntry]) -> Result<BTreeMap<DrivePath, &SyncEntry>, FfiError> {
    let mut index = BTreeMap::new();
    for entry in entries {
        let path = parse_path(&entry.path)?;
        if (entry.kind == FileType::File) != entry.sha256.is_some() {
            return Err(FfiError::InvalidPath {
                message: format!("expected a SHA-256 for files only: {path}"),
            });
        }
        if index.insert(path.clone(), entry).is_some() {
            return Err(FfiError::InvalidPath {
                message: format!("duplicate sync path: {path}"),
            });
        }
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, hash: &str) -> SyncEntry {
        SyncEntry {
            path: path.to_string(),
            kind: FileType::File,
            sha256: Some(hash.to_string()),
        }
    }

    fn folder(path: &str) -> SyncEntry {
        SyncEntry {
            path: path.to_string(),
            kind: FileType::Folder,
            sha256: None,
        }
    }

    #[test]
    fn local_non_empty_folder_delete_emits_bottom_up_remote_deletes() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let baseline = vec![
            folder("AI Notes"),
            file("AI Notes/note1.md", &h1),
            folder("AI Notes/sub"),
            file("AI Notes/sub/note2.md", &h2),
        ];
        let remote = vec![
            folder("AI Notes"),
            file("AI Notes/note1.md", &h1),
            folder("AI Notes/sub"),
            file("AI Notes/sub/note2.md", &h2),
        ];
        let actions = plan_folder_sync(vec![], remote, baseline).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::DeleteRemote {
                    path: "AI Notes/sub/note2.md".into(),
                },
                SyncAction::DeleteRemote {
                    path: "AI Notes/note1.md".into(),
                },
                SyncAction::DeleteRemote {
                    path: "AI Notes/sub".into(),
                },
                SyncAction::DeleteRemote {
                    path: "AI Notes".into(),
                },
            ]
        );
    }

    #[test]
    fn remote_non_empty_folder_delete_emits_bottom_up_local_deletes() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let baseline = vec![
            folder("AI Notes"),
            file("AI Notes/note1.md", &h1),
            folder("AI Notes/sub"),
            file("AI Notes/sub/note2.md", &h2),
        ];
        let local = vec![
            folder("AI Notes"),
            file("AI Notes/note1.md", &h1),
            folder("AI Notes/sub"),
            file("AI Notes/sub/note2.md", &h2),
        ];
        let actions = plan_folder_sync(local, vec![], baseline).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::DeleteLocal {
                    path: "AI Notes/sub/note2.md".into(),
                },
                SyncAction::DeleteLocal {
                    path: "AI Notes/note1.md".into(),
                },
                SyncAction::DeleteLocal {
                    path: "AI Notes/sub".into(),
                },
                SyncAction::DeleteLocal {
                    path: "AI Notes".into(),
                },
            ]
        );
    }

    #[test]
    fn fresh_client_with_empty_baseline_downloads_everything() {
        let h1 = "a".repeat(64);
        let remote = vec![folder("AI Notes"), file("AI Notes/note1.md", &h1)];
        let actions = plan_folder_sync(vec![], remote, vec![]).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::CreateLocalFolder {
                    path: "AI Notes".into(),
                },
                SyncAction::Download {
                    path: "AI Notes/note1.md".into(),
                },
            ]
        );
    }

    #[test]
    fn local_folder_delete_vs_remote_new_file_preserves_remote_addition() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let baseline = vec![folder("AI Notes"), file("AI Notes/old.md", &h1)];
        let remote = vec![
            folder("AI Notes"),
            file("AI Notes/old.md", &h1),
            file("AI Notes/new-on-b.md", &h2),
        ];
        let actions = plan_folder_sync(vec![], remote, baseline).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::CreateLocalFolder {
                    path: "AI Notes".into(),
                },
                SyncAction::Download {
                    path: "AI Notes/new-on-b.md".into(),
                },
                SyncAction::DeleteRemote {
                    path: "AI Notes/old.md".into(),
                },
            ]
        );
    }

    #[test]
    fn local_folder_delete_vs_remote_modified_file_preserves_remote_edit() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let baseline = vec![folder("AI Notes"), file("AI Notes/note1.md", &h1)];
        let remote = vec![folder("AI Notes"), file("AI Notes/note1.md", &h2)];
        let actions = plan_folder_sync(vec![], remote, baseline).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::CreateLocalFolder {
                    path: "AI Notes".into(),
                },
                SyncAction::Download {
                    path: "AI Notes/note1.md".into(),
                },
            ]
        );
    }

    #[test]
    fn remote_folder_delete_vs_local_modified_file_preserves_local_edit() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let baseline = vec![folder("AI Notes"), file("AI Notes/note1.md", &h1)];
        let local = vec![folder("AI Notes"), file("AI Notes/note1.md", &h2)];
        let actions = plan_folder_sync(local, vec![], baseline).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::CreateRemoteFolder {
                    path: "AI Notes".into(),
                },
                SyncAction::Upload {
                    path: "AI Notes/note1.md".into(),
                },
            ]
        );
    }

    #[test]
    fn remote_folder_delete_vs_local_new_file_preserves_local_addition() {
        let h1 = "a".repeat(64);
        let h2 = "b".repeat(64);
        let baseline = vec![folder("AI Notes"), file("AI Notes/old.md", &h1)];
        let local = vec![
            folder("AI Notes"),
            file("AI Notes/old.md", &h1),
            file("AI Notes/new-local.md", &h2),
        ];
        let actions = plan_folder_sync(local, vec![], baseline).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::CreateRemoteFolder {
                    path: "AI Notes".into(),
                },
                SyncAction::Upload {
                    path: "AI Notes/new-local.md".into(),
                },
                SyncAction::DeleteLocal {
                    path: "AI Notes/old.md".into(),
                },
            ]
        );
    }

    #[test]
    fn both_deleted_emits_no_actions() {
        let h1 = "a".repeat(64);
        let baseline = vec![folder("AI Notes"), file("AI Notes/old.md", &h1)];
        let actions = plan_folder_sync(vec![], vec![], baseline).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn both_sides_compare_to_baseline_entries() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let local = || {
            vec![
                file("conflict", &a),
                file("same", &a),
                folder("folder"),
                file("folder/new", &a),
                file("local-change", &a),
                file("remote-change", &b),
            ]
        };
        let remote = || {
            vec![
                file("remote-change", &a),
                file("same", &a),
                file("conflict", &b),
                file("local-change", &b),
                file("remote-only", &a),
            ]
        };
        let baseline = || {
            vec![
                file("conflict", &c),
                file("local-change", &b),
                file("remote-change", &b),
            ]
        };
        assert_eq!(
            plan_folder_sync(local(), remote(), baseline()).unwrap(),
            vec![
                SyncAction::Conflict {
                    path: "conflict".into(),
                },
                SyncAction::CreateRemoteFolder {
                    path: "folder".into(),
                },
                SyncAction::Upload {
                    path: "folder/new".into(),
                },
                SyncAction::Upload {
                    path: "local-change".into(),
                },
                SyncAction::Download {
                    path: "remote-change".into(),
                },
                SyncAction::Download {
                    path: "remote-only".into(),
                },
            ]
        );

        assert!(
            plan_folder_sync(
                vec![file("same", &a)],
                vec![file("same", &a)],
                vec![file("same", &c)],
            )
            .unwrap()
            .is_empty()
        );

        assert_eq!(
            plan_folder_sync(vec![file("note.md", &a)], vec![file("note.md", &b)], vec![]).unwrap(),
            vec![SyncAction::Conflict {
                path: "note.md".into(),
            }]
        );

        let mut remote_file_at_folder = remote();
        remote_file_at_folder.push(file("folder", &b));
        let actions = plan_folder_sync(local(), remote_file_at_folder, baseline()).unwrap();
        assert!(actions.iter().all(|action| !matches!(
            action,
            SyncAction::Upload { path } if path == "folder/new"
        )));
    }

    #[test]
    fn duplicate_paths_and_files_without_sha256_are_invalid() {
        let hash = "a".repeat(64);
        let duplicate = plan_folder_sync(
            vec![file("notes/a.md", &hash), file("notes/a.md", &hash)],
            vec![],
            vec![],
        );
        assert!(matches!(duplicate, Err(FfiError::InvalidPath { .. })));

        let missing_hash = plan_folder_sync(
            vec![SyncEntry {
                path: "notes/a.md".into(),
                kind: FileType::File,
                sha256: None,
            }],
            vec![],
            vec![],
        );
        assert!(matches!(missing_hash, Err(FfiError::InvalidPath { .. })));

        let folder_with_hash = plan_folder_sync(
            vec![SyncEntry {
                path: "notes".into(),
                kind: FileType::Folder,
                sha256: Some(hash),
            }],
            vec![],
            vec![],
        );
        assert!(matches!(
            folder_with_hash,
            Err(FfiError::InvalidPath { .. })
        ));
    }

    #[test]
    fn nfc_equivalent_paths_are_the_same_entry() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let composed = plan_folder_sync(
            vec![file("caf\u{e9}.txt", &a), file("cafe\u{301}.txt", &b)],
            vec![],
            vec![],
        );
        assert!(matches!(composed, Err(FfiError::InvalidPath { .. })));
    }

    #[test]
    fn recreate_after_delete_uploads_cleanly() {
        let h2 = "b".repeat(64);
        let local = vec![folder("AI Notes"), file("AI Notes/note1.md", &h2)];
        let actions = plan_folder_sync(local, vec![], vec![]).unwrap();
        assert_eq!(
            actions,
            vec![
                SyncAction::CreateRemoteFolder {
                    path: "AI Notes".into(),
                },
                SyncAction::Upload {
                    path: "AI Notes/note1.md".into(),
                },
            ]
        );
    }
}
