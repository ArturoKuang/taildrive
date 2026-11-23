use std::collections::{BTreeMap, BTreeSet};

use taildrive_core::path::{DrivePath, parse_path};

use crate::{FfiError, FileType};

#[derive(uniffi::Record)]
pub struct SyncEntry {
    pub path: String,
    pub kind: FileType,
    pub sha256: Option<String>,
    /// Supply this on the local entry after a successful sync. It distinguishes
    /// one-sided edits from changes to both copies.
    pub last_synced_sha256: Option<String>,
}

#[derive(Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncAction {
    Upload { path: String },
    Download { path: String },
    CreateRemoteFolder { path: String },
    CreateLocalFolder { path: String },
    Conflict { path: String },
}

#[uniffi::export]
pub fn plan_folder_sync(
    local_entries: Vec<SyncEntry>,
    remote_entries: Vec<SyncEntry>,
) -> Result<Vec<SyncAction>, FfiError> {
    let local = index(&local_entries)?;
    let remote = index(&remote_entries)?;
    let paths: BTreeSet<_> = local.keys().chain(remote.keys()).collect();
    let mut conflicts = BTreeSet::new();
    let mut actions = Vec::new();
    for path in paths {
        if path
            .ancestors()
            .iter()
            .any(|parent| conflicts.contains(parent))
        {
            continue;
        }

        let name = path.to_string();
        let action = match (local.get(path), remote.get(path)) {
            (Some(left), Some(right)) if left.kind != right.kind => {
                conflicts.insert(path.clone());
                SyncAction::Conflict { path: name }
            }
            (Some(left), Some(right)) => {
                if left.sha256 == right.sha256 {
                    continue;
                }
                if left.last_synced_sha256 == right.sha256 {
                    SyncAction::Upload { path: name }
                } else if left.last_synced_sha256 == left.sha256 {
                    SyncAction::Download { path: name }
                } else {
                    SyncAction::Conflict { path: name }
                }
            }
            (Some(entry), None) => match entry.kind {
                FileType::File => SyncAction::Upload { path: name },
                FileType::Folder => SyncAction::CreateRemoteFolder { path: name },
            },
            (None, Some(entry)) => match entry.kind {
                FileType::File => SyncAction::Download { path: name },
                FileType::Folder => SyncAction::CreateLocalFolder { path: name },
            },
            (None, None) => unreachable!("path came from one of the indexes"),
        };
        actions.push(action);
    }
    Ok(actions)
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
