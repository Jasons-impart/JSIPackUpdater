use crate::planner::validate_source_paths;
use crate::safe_fs::{LocalArtifact, SafeRoot};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::Path;

const STATE_DIR: &str = ".jsipack-updater";
const JOURNAL: &str = "active.json";
const MARKER: &str = "config/createdelight_release_info.json";

#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    /// SHA-256 of the instance file observed while preparing, or None if absent.
    pub expected: Option<String>,
    /// Replacement exists at the same path inside the prepared root.
    pub replacement: bool,
}

pub struct InstanceLock {
    _file: std::fs::File,
    root: std::path::PathBuf,
    instance: SafeRoot,
    state: SafeRoot,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
    }
}

pub fn lock_instance(instance_path: &Path) -> Result<InstanceLock> {
    let instance = SafeRoot::open(instance_path)?;
    instance.ensure_dir(STATE_DIR)?;
    let state = instance.open_dir(STATE_DIR)?;
    let file = state.open_lock_file("lock")?;
    file.try_lock_exclusive()
        .context("another update is using this instance")?;
    Ok(InstanceLock {
        _file: file,
        root: instance_path.canonicalize()?,
        instance,
        state,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    backup_prefix: String,
    files: Vec<JournalFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    path: String,
    old_hash: Option<String>,
    new_hash: Option<String>,
    #[serde(default)]
    old_readonly: Option<bool>,
}

pub fn pending(instance_path: &Path) -> Result<bool> {
    let instance = SafeRoot::open(instance_path)?;
    let state = instance_path.join(STATE_DIR);
    if !state.exists() {
        return Ok(false);
    }
    instance.ensure_dir(STATE_DIR)?;
    let state = SafeRoot::open(&state)?;
    Ok(state.open_regular(JOURNAL)?.is_some())
}

pub fn apply(instance_path: &Path, prepared: &SafeRoot, changes: &[Change]) -> Result<()> {
    let guard = lock_instance(instance_path)?;
    apply_locked(&guard, instance_path, prepared, changes, &BTreeMap::new())
}

pub fn apply_locked(
    guard: &InstanceLock,
    instance_path: &Path,
    prepared: &SafeRoot,
    changes: &[Change],
    verify_only: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    if instance_path.canonicalize()? != guard.root {
        bail!("instance lock belongs to another root");
    }
    apply_inner_roots(
        &guard.instance,
        &guard.state,
        prepared,
        changes,
        verify_only,
        None,
        None,
    )
}

#[cfg(test)]
fn apply_inner(
    instance_path: &Path,
    prepared: &SafeRoot,
    changes: &[Change],
    verify_only: &BTreeMap<String, Option<String>>,
    fail_after: Option<usize>,
    marker_hook: Option<&mut dyn FnMut()>,
) -> Result<()> {
    let instance = SafeRoot::open(instance_path)?;
    instance.ensure_dir(STATE_DIR)?;
    let state = instance.open_dir(STATE_DIR)?;
    apply_inner_roots(
        &instance,
        &state,
        prepared,
        changes,
        verify_only,
        fail_after,
        marker_hook,
    )
}

fn apply_inner_roots(
    instance: &SafeRoot,
    state: &SafeRoot,
    prepared: &SafeRoot,
    changes: &[Change],
    verify_only: &BTreeMap<String, Option<String>>,
    fail_after: Option<usize>,
    mut marker_hook: Option<&mut dyn FnMut()>,
) -> Result<()> {
    if changes.is_empty() {
        bail!("empty update transaction");
    }
    let paths: BTreeSet<_> = changes.iter().map(|change| change.path.as_str()).collect();
    if paths.len() != changes.len() {
        bail!("duplicate update path");
    }
    validate_change_paths(paths)?;
    validate_source_paths(verify_only.keys().map(String::as_str))?;
    if verify_only
        .keys()
        .any(|path| changes.iter().any(|change| change.path == *path))
    {
        bail!("verification-only path also has a planned write");
    }
    if changes
        .last()
        .is_none_or(|change| change.path != MARKER || !change.replacement)
    {
        bail!("release marker must be the final transaction write");
    }
    if state.open_regular(JOURNAL)?.is_some() {
        bail!("unfinished update exists; recover it before starting another");
    }
    let backup_prefix = format!(
        "backup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    state.ensure_dir(&backup_prefix)?;
    let mut journal = Journal {
        backup_prefix,
        files: Vec::new(),
    };
    let preparation = (|| -> Result<()> {
        for change in changes {
            let old_hash = hash(instance, &change.path)?;
            if old_hash != change.expected {
                bail!("instance changed after preview: {}", change.path);
            }
            let new_hash = if change.replacement {
                hash(prepared, &change.path)?
                    .with_context(|| format!("prepared replacement missing: {}", change.path))?
                    .into()
            } else {
                None
            };
            let old_readonly = if old_hash.is_some() {
                Some(
                    instance
                        .open_regular(&change.path)?
                        .with_context(|| format!("file vanished during backup: {}", change.path))?
                        .metadata()?
                        .permissions()
                        .readonly(),
                )
            } else {
                None
            };
            journal.files.push(JournalFile {
                path: change.path.clone(),
                old_hash: old_hash.clone(),
                new_hash,
                old_readonly,
            });
            if old_hash.is_some() {
                let copied = instance.copy_regular_to(
                    &change.path,
                    state,
                    &format!("{}/{}", journal.backup_prefix, change.path),
                )?;
                if !copied {
                    bail!("instance changed while backing up: {}", change.path);
                }
                let backup_hash =
                    hash(state, &format!("{}/{}", journal.backup_prefix, change.path))?;
                if backup_hash != old_hash {
                    bail!("instance changed while backing up: {}", change.path);
                }
            }
        }
        state.write_atomic(JOURNAL, Cursor::new(serde_json::to_vec_pretty(&journal)?))?;
        Ok(())
    })();
    if let Err(error) = preparation {
        if state.open_regular(JOURNAL)?.is_none() {
            cleanup_backups(state, &journal);
        }
        return Err(error);
    }
    for (index, change) in changes.iter().enumerate() {
        if fail_after == Some(index) {
            bail!("injected update failure after {index} writes");
        }
        let recorded = &journal.files[index];
        if change.path == MARKER {
            if let Some(hook) = marker_hook.as_mut() {
                hook();
            }
            for earlier in journal.files.iter().take(index) {
                if hash(instance, &earlier.path)? != earlier.new_hash {
                    bail!(
                        "applied file changed before release marker: {}",
                        earlier.path
                    );
                }
            }
            for (path, expected) in verify_only {
                if hash(instance, path)? != *expected {
                    bail!("retained file changed during update: {path}");
                }
            }
        }
        if hash(instance, &change.path)? != recorded.old_hash {
            bail!("instance changed during update: {}", change.path);
        }
        if change.replacement {
            prepared.copy_regular_to_checked(&change.path, instance, &change.path, || {
                if hash(instance, &change.path)? != recorded.old_hash {
                    bail!(
                        "instance changed while preparing replacement: {}",
                        change.path
                    );
                }
                Ok(())
            })?;
        } else {
            instance.remove_regular(&change.path)?;
        }
        if hash(instance, &change.path)? != recorded.new_hash {
            bail!("written file failed verification: {}", change.path);
        }
    }
    cleanup(state, &journal)?;
    Ok(())
}

/// Restore all files to their pre-update bytes, including a marker written before a crash.
pub fn rollback(instance_path: &Path) -> Result<()> {
    let guard = lock_instance(instance_path)?;
    let instance = &guard.instance;
    let state = &guard.state;
    let journal: Journal = serde_json::from_slice(&state.required_file(JOURNAL)?)
        .context("reading unfinished update journal")?;
    if !journal.backup_prefix.starts_with("backup-")
        || journal.backup_prefix.contains('/')
        || journal.backup_prefix.contains('\\')
    {
        bail!("invalid update backup prefix");
    }
    let paths: BTreeSet<_> = journal
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    if paths.len() != journal.files.len() {
        bail!("duplicate path in update journal");
    }
    validate_change_paths(paths)?;
    if journal.files.last().is_none_or(|file| file.path != MARKER) {
        bail!("invalid update journal marker order");
    }
    // Check every path before writing any, so a player's post-crash change is not lost.
    for file in &journal.files {
        let current = hash(instance, &file.path)?;
        if current != file.old_hash && current != file.new_hash {
            bail!("file changed after interrupted update: {}", file.path);
        }
        if file.old_hash.is_some()
            && hash(state, &format!("{}/{}", journal.backup_prefix, file.path))? != file.old_hash
        {
            bail!("update backup failed verification: {}", file.path);
        }
    }
    for file in journal.files.iter().rev() {
        let current = hash(instance, &file.path)?;
        if current != file.old_hash && current != file.new_hash {
            bail!("file changed during rollback: {}", file.path);
        }
        if current == file.old_hash {
            #[cfg(windows)]
            if let Some(readonly) = file.old_readonly {
                instance.set_readonly(&file.path, readonly)?;
            }
            continue;
        }
        if file.old_hash.is_some() {
            state.copy_regular_to_checked(
                &format!("{}/{}", journal.backup_prefix, file.path),
                instance,
                &file.path,
                || {
                    if hash(instance, &file.path)? != current {
                        bail!("file changed while preparing rollback: {}", file.path);
                    }
                    Ok(())
                },
            )?;
        } else {
            instance.remove_regular(&file.path)?;
        }
        if hash(instance, &file.path)? != file.old_hash {
            bail!("rollback failed verification: {}", file.path);
        }
        #[cfg(windows)]
        if let Some(readonly) = file.old_readonly {
            instance.set_readonly(&file.path, readonly)?;
        }
    }
    cleanup(state, &journal)
}

fn cleanup(state: &SafeRoot, journal: &Journal) -> Result<()> {
    // Remove the journal first: if cleanup is interrupted, backup files may remain,
    // but the already completed update will never be mistaken for an active one.
    state.remove_regular(JOURNAL)?;
    cleanup_backups(state, journal);
    Ok(())
}

fn cleanup_backups(state: &SafeRoot, journal: &Journal) {
    for file in &journal.files {
        let _ = state.remove_regular(&format!("{}/{}", journal.backup_prefix, file.path));
    }
    let mut directories = BTreeSet::new();
    for file in &journal.files {
        let mut path = file.path.as_str();
        while let Some((parent, _)) = path.rsplit_once('/') {
            directories.insert(format!("{}/{parent}", journal.backup_prefix));
            path = parent;
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    for path in directories {
        let _ = state.remove_empty_dir(&path);
    }
    let _ = state.remove_empty_dir(&journal.backup_prefix);
}

fn validate_change_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut ordinary = Vec::new();
    for path in paths {
        if path.starts_with(".jsipack-updater/") {
            if !matches!(
                path,
                ".jsipack-updater/side.json"
                    | ".jsipack-updater/overrides.json"
                    | ".jsipack-updater/generated.json"
            ) {
                bail!("invalid updater state path in transaction: {path}");
            }
        } else {
            ordinary.push(path);
        }
    }
    validate_source_paths(ordinary)
}

pub fn hash(root: &SafeRoot, path: &str) -> Result<Option<String>> {
    match root.hash_artifact(path)? {
        LocalArtifact::Missing => Ok(None),
        LocalArtifact::File(hashes) => Ok(Some(hashes.sha256)),
        LocalArtifact::Obstacle => bail!("path occupied by non-file: {path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn interrupted_update_rolls_back_files_and_marker() {
        let instance = tempfile::TempDir::new().unwrap();
        let prepared = tempfile::TempDir::new().unwrap();
        for root in [instance.path(), prepared.path()] {
            fs::create_dir_all(root.join("config")).unwrap();
        }
        fs::write(instance.path().join("old.txt"), b"old").unwrap();
        fs::write(instance.path().join(MARKER), b"A").unwrap();
        fs::write(prepared.path().join("old.txt"), b"new").unwrap();
        fs::write(prepared.path().join(MARKER), b"B").unwrap();
        let installed = SafeRoot::open(instance.path()).unwrap();
        let changes = vec![
            Change {
                path: "old.txt".into(),
                expected: hash(&installed, "old.txt").unwrap(),
                replacement: true,
            },
            Change {
                path: MARKER.into(),
                expected: hash(&installed, MARKER).unwrap(),
                replacement: true,
            },
        ];
        let prepared_root = SafeRoot::open(prepared.path()).unwrap();
        assert!(
            apply_inner(
                instance.path(),
                &prepared_root,
                &changes,
                &BTreeMap::new(),
                Some(1),
                None
            )
            .is_err()
        );
        assert_eq!(fs::read(instance.path().join("old.txt")).unwrap(), b"new");
        assert_eq!(fs::read(instance.path().join(MARKER)).unwrap(), b"A");
        assert!(pending(instance.path()).unwrap());
        rollback(instance.path()).unwrap();
        assert_eq!(fs::read(instance.path().join("old.txt")).unwrap(), b"old");
        assert_eq!(fs::read(instance.path().join(MARKER)).unwrap(), b"A");
        assert!(!pending(instance.path()).unwrap());
    }

    #[test]
    fn preparation_error_removes_unpublished_backups() {
        let instance = tempfile::TempDir::new().unwrap();
        let prepared = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(instance.path().join("config")).unwrap();
        fs::create_dir_all(prepared.path().join("config")).unwrap();
        fs::write(instance.path().join("a"), b"old").unwrap();
        fs::write(instance.path().join(MARKER), b"A").unwrap();
        fs::write(prepared.path().join("a"), b"new").unwrap();
        fs::write(prepared.path().join(MARKER), b"B").unwrap();
        let root = SafeRoot::open(instance.path()).unwrap();
        let changes = [
            Change {
                path: "a".into(),
                expected: hash(&root, "a").unwrap(),
                replacement: true,
            },
            Change {
                path: MARKER.into(),
                expected: Some("0".repeat(64)),
                replacement: true,
            },
        ];
        assert!(
            apply(
                instance.path(),
                &SafeRoot::open(prepared.path()).unwrap(),
                &changes
            )
            .is_err()
        );
        assert!(!pending(instance.path()).unwrap());
        let entries: Vec<_> = fs::read_dir(instance.path().join(STATE_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|name| name.starts_with("backup-")),
            "{entries:?}"
        );
        assert_eq!(fs::read(instance.path().join("a")).unwrap(), b"old");
    }

    #[test]
    fn changed_retained_artifact_blocks_completion() {
        let instance = tempfile::TempDir::new().unwrap();
        let prepared = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(instance.path().join("config")).unwrap();
        fs::create_dir_all(prepared.path().join("config")).unwrap();
        fs::write(instance.path().join("retained.jar"), b"changed").unwrap();
        fs::write(instance.path().join(MARKER), b"A").unwrap();
        fs::write(prepared.path().join(MARKER), b"B").unwrap();
        let root = SafeRoot::open(instance.path()).unwrap();
        let changes = [Change {
            path: MARKER.into(),
            expected: hash(&root, MARKER).unwrap(),
            replacement: true,
        }];
        let verify = BTreeMap::from([("retained.jar".into(), Some("0".repeat(64)))]);
        let guard = lock_instance(instance.path()).unwrap();
        let error = apply_locked(
            &guard,
            instance.path(),
            &SafeRoot::open(prepared.path()).unwrap(),
            &changes,
            &verify,
        )
        .unwrap_err();
        assert!(error.to_string().contains("retained file"));
        assert_eq!(fs::read(instance.path().join(MARKER)).unwrap(), b"A");
        drop(guard);
        rollback(instance.path()).unwrap();
    }

    #[test]
    fn lock_cannot_be_used_for_another_instance() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        let prepared = SafeRoot::open(first.path()).unwrap();
        let guard = lock_instance(first.path()).unwrap();
        let error =
            apply_locked(&guard, second.path(), &prepared, &[], &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("another root"));
    }

    #[cfg(unix)]
    #[test]
    fn moved_locked_instance_never_writes_replacement_directory() {
        let parent = tempfile::TempDir::new().unwrap();
        let original = parent.path().join("instance");
        let moved = parent.path().join("moved");
        let prepared_dir = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(original.join("config")).unwrap();
        fs::create_dir_all(prepared_dir.path().join("config")).unwrap();
        fs::write(original.join(MARKER), b"A").unwrap();
        fs::write(prepared_dir.path().join(MARKER), b"B").unwrap();
        let root = SafeRoot::open(&original).unwrap();
        let changes = [Change {
            path: MARKER.into(),
            expected: hash(&root, MARKER).unwrap(),
            replacement: true,
        }];
        let guard = lock_instance(&original).unwrap();
        fs::rename(&original, &moved).unwrap();
        fs::create_dir_all(original.join("config")).unwrap();
        fs::write(original.join(MARKER), b"A").unwrap();
        let prepared = SafeRoot::open(prepared_dir.path()).unwrap();
        let _ = apply_locked(&guard, &original, &prepared, &changes, &BTreeMap::new());
        assert_eq!(fs::read(original.join(MARKER)).unwrap(), b"A");
    }

    #[test]
    fn edit_after_an_earlier_write_blocks_release_marker() {
        let instance = tempfile::TempDir::new().unwrap();
        let prepared = tempfile::TempDir::new().unwrap();
        for root in [instance.path(), prepared.path()] {
            fs::create_dir_all(root.join("config")).unwrap();
        }
        fs::write(instance.path().join("config/settings.txt"), b"old").unwrap();
        fs::write(instance.path().join(MARKER), b"A").unwrap();
        fs::write(prepared.path().join("config/settings.txt"), b"new").unwrap();
        fs::write(prepared.path().join(MARKER), b"B").unwrap();
        let root = SafeRoot::open(instance.path()).unwrap();
        let changes = [
            Change {
                path: "config/settings.txt".into(),
                expected: hash(&root, "config/settings.txt").unwrap(),
                replacement: true,
            },
            Change {
                path: MARKER.into(),
                expected: hash(&root, MARKER).unwrap(),
                replacement: true,
            },
        ];
        let mut edit =
            || fs::write(instance.path().join("config/settings.txt"), b"external").unwrap();
        let error = apply_inner(
            instance.path(),
            &SafeRoot::open(prepared.path()).unwrap(),
            &changes,
            &BTreeMap::new(),
            None,
            Some(&mut edit),
        )
        .unwrap_err();
        assert!(error.to_string().contains("applied file changed"));
        assert_eq!(fs::read(instance.path().join(MARKER)).unwrap(), b"A");
        assert!(pending(instance.path()).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn rollback_skips_unchanged_readonly_file() {
        let instance = tempfile::TempDir::new().unwrap();
        let prepared = tempfile::TempDir::new().unwrap();
        for root in [instance.path(), prepared.path()] {
            fs::create_dir_all(root.join("config")).unwrap();
        }
        fs::write(instance.path().join("readonly.txt"), b"old").unwrap();
        fs::write(instance.path().join(MARKER), b"A").unwrap();
        fs::write(prepared.path().join("readonly.txt"), b"new").unwrap();
        fs::write(prepared.path().join(MARKER), b"B").unwrap();
        let root = SafeRoot::open(instance.path()).unwrap();
        let changes = [
            Change {
                path: "readonly.txt".into(),
                expected: hash(&root, "readonly.txt").unwrap(),
                replacement: true,
            },
            Change {
                path: MARKER.into(),
                expected: hash(&root, MARKER).unwrap(),
                replacement: true,
            },
        ];
        let mut permissions = fs::metadata(instance.path().join("readonly.txt"))
            .unwrap()
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(instance.path().join("readonly.txt"), permissions).unwrap();
        assert!(
            apply_inner(
                instance.path(),
                &SafeRoot::open(prepared.path()).unwrap(),
                &changes,
                &BTreeMap::new(),
                Some(0),
                None
            )
            .is_err()
        );
        root.set_readonly("readonly.txt", false).unwrap();
        rollback(instance.path()).unwrap();
        assert!(!pending(instance.path()).unwrap());
        assert!(
            fs::metadata(instance.path().join("readonly.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(
            apply_inner(
                instance.path(),
                &SafeRoot::open(prepared.path()).unwrap(),
                &changes,
                &BTreeMap::new(),
                Some(1),
                None
            )
            .is_err()
        );
        root.set_readonly("readonly.txt", false).unwrap();
        rollback(instance.path()).unwrap();
        assert_eq!(
            fs::read(instance.path().join("readonly.txt")).unwrap(),
            b"old"
        );
        assert!(
            fs::metadata(instance.path().join("readonly.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        apply(
            instance.path(),
            &SafeRoot::open(prepared.path()).unwrap(),
            &changes,
        )
        .unwrap();
        assert_eq!(
            fs::read(instance.path().join("readonly.txt")).unwrap(),
            b"new"
        );
        let state_entries: Vec<_> = fs::read_dir(instance.path().join(STATE_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!state_entries.iter().any(|name| name.starts_with("backup-")));
        root.remove_regular("readonly.txt").unwrap();
        assert!(!instance.path().join("readonly.txt").exists());
    }
}
