pub use crate::safe_fs::LocalFile;
use crate::safe_fs::{SafeRoot, portable_case_key};
use crate::version::ComponentCheck;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    AlreadyTarget,
    WriteTarget,
    Delete,
    KeepLocal,
    Merge,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceChange {
    Added,
    Modified,
    Deleted,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub path: String,
    pub source_change: SourceChange,
    pub action: Action,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub items: Vec<PlanItem>,
}

impl Plan {
    pub fn has_conflicts(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.action == Action::Conflict)
    }
}

pub fn decide(
    base: Option<&[u8]>,
    local: &LocalFile,
    target: Option<&[u8]>,
) -> Option<(Action, &'static str)> {
    if *local == LocalFile::Obstacle {
        return Some((Action::Conflict, "path_occupied_by_directory"));
    }
    let disk = match local {
        LocalFile::Missing => None,
        LocalFile::File(bytes) => Some(bytes.as_slice()),
        LocalFile::Obstacle => unreachable!(),
    };
    if disk == target {
        return if base == target {
            None
        } else {
            Some((Action::AlreadyTarget, "already_matches_target"))
        };
    }
    // Release ZIPs may omit tracked files, so a missing target file is restored even
    // when the source commit did not change it.
    if disk.is_none() && target.is_some() {
        return Some((Action::WriteTarget, "target_file_missing"));
    }
    if disk == base {
        return Some(if target.is_some() {
            (Action::WriteTarget, "target_changed")
        } else {
            (Action::Delete, "target_deleted")
        });
    }
    if base == target {
        return Some((Action::KeepLocal, "local_override"));
    }
    if let (Some(a), Some(l), Some(b)) = (base, disk, target) {
        if is_text(a) && is_text(l) && is_text(b) {
            let total = a.len().saturating_add(l.len()).saturating_add(b.len());
            let lines = [a, l, b]
                .into_iter()
                .map(|bytes| bytes.iter().filter(|byte| **byte == b'\n').count())
                .sum::<usize>();
            if total > 2 * 1024 * 1024 || lines > 30_000 {
                return Some((Action::Conflict, "text_merge_resource_limit"));
            }
            let (a, l, b) = (
                std::str::from_utf8(a).unwrap(),
                std::str::from_utf8(l).unwrap(),
                std::str::from_utf8(b).unwrap(),
            );
            if diffy::merge(a, l, b).is_ok() {
                return Some((Action::Merge, "clean_text_merge"));
            }
            return Some((Action::Conflict, "text_merge_conflict"));
        }
        return Some((Action::Conflict, "binary_change_conflict"));
    }
    Some((Action::Conflict, "add_or_delete_conflict"))
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes
        .iter()
        .any(|byte| *byte < 32 && !matches!(*byte, b'\n' | b'\r' | b'\t'))
        && std::str::from_utf8(bytes).is_ok()
}

fn source_change(base: Option<&[u8]>, target: Option<&[u8]>) -> SourceChange {
    match (base, target) {
        (None, Some(_)) => SourceChange::Added,
        (Some(_), None) => SourceChange::Deleted,
        (Some(a), Some(b)) if a != b => SourceChange::Modified,
        _ => SourceChange::Unchanged,
    }
}

pub fn plan_directories(base_dir: &Path, target_dir: &Path, instance_dir: &Path) -> Result<Plan> {
    let base = SafeRoot::open(base_dir)?;
    let target = SafeRoot::open(target_dir)?;
    let instance = SafeRoot::open(instance_dir)?;
    plan_roots(&base, &target, &instance)
}

pub fn plan_roots(
    base_root: &SafeRoot,
    target_root: &SafeRoot,
    instance_root: &SafeRoot,
) -> Result<Plan> {
    plan_roots_inner(base_root, target_root, instance_root, None)
}

pub fn plan_roots_checked(
    base_root: &SafeRoot,
    target_root: &SafeRoot,
    instance_root: &SafeRoot,
    check: &ComponentCheck,
) -> Result<Plan> {
    plan_roots_inner(base_root, target_root, instance_root, Some(check))
}

fn plan_roots_inner(
    base_root: &SafeRoot,
    target_root: &SafeRoot,
    instance_root: &SafeRoot,
    check: Option<&ComponentCheck>,
) -> Result<Plan> {
    let base = read_tree(base_root)?;
    let target = read_tree(target_root)?;
    if let Some(check) = check
        && (base.get("pack/pack.toml") != Some(&check.base_pack_hash)
            || target.get("pack/pack.toml") != Some(&check.target_pack_hash))
    {
        bail!("pack/pack.toml changed after component validation");
    }
    let paths: BTreeSet<_> = base.keys().chain(target.keys()).cloned().collect();
    let instance_collisions = instance_root.case_collisions(&paths)?;
    // Each commit tree must be portable on its own. A path may legitimately
    // change from a file to a directory (or only change case) across commits.
    check_collisions(&base.keys().cloned().collect())?;
    check_collisions(&target.keys().cloned().collect())?;
    let folded_base: BTreeMap<_, _> = base
        .keys()
        .map(|path| (portable_case_key(path), path))
        .collect();
    let folded_target: BTreeMap<_, _> = target
        .keys()
        .map(|path| (portable_case_key(path), path))
        .collect();
    let base_directories = folded_directories(&base);
    let target_directories = folded_directories(&target);
    let mut items = Vec::new();
    for path in paths {
        let a_hash = base.get(&path);
        let b_hash = target.get(&path);
        let case_renamed = if a_hash.is_some() && b_hash.is_none() {
            folded_target
                .get(&portable_case_key(&path))
                .is_some_and(|new| *new != &path)
        } else {
            a_hash.is_none()
                && b_hash.is_some()
                && folded_base
                    .get(&portable_case_key(&path))
                    .is_some_and(|old| *old != &path)
        } || path_prefixes(&path).any(|prefix| {
            let opposite = if a_hash.is_some() {
                &target_directories
            } else {
                &base_directories
            };
            opposite
                .get(&portable_case_key(prefix))
                .is_some_and(|other| *other != prefix)
        });
        if case_renamed {
            items.push(PlanItem {
                path,
                source_change: if a_hash.is_some() {
                    SourceChange::Deleted
                } else {
                    SourceChange::Added
                },
                action: Action::Conflict,
                reason: "case_only_rename",
            });
            continue;
        }
        let a_bytes = a_hash
            .map(|hash| read_checked(base_root, &path, hash))
            .transpose()?;
        let b_bytes = b_hash
            .map(|hash| read_checked(target_root, &path, hash))
            .transpose()?;
        let a = a_bytes.as_deref();
        let b = b_bytes.as_deref();
        let local = instance_root.read(&path)?;
        if instance_collisions.contains(&path) {
            items.push(PlanItem {
                path,
                source_change: source_change(a, b),
                action: Action::Conflict,
                reason: "instance_case_collision",
            });
            continue;
        }
        if a.is_some()
            && b.is_none()
            && local == LocalFile::Obstacle
            && target_replaces(&path, &target)
        {
            items.push(PlanItem {
                path,
                source_change: SourceChange::Deleted,
                action: Action::AlreadyTarget,
                reason: "file_directory_transition",
            });
            continue;
        }
        if let Some((action, reason)) = decide(a, &local, b) {
            items.push(PlanItem {
                path,
                source_change: source_change(a, b),
                action,
                reason,
            });
        }
    }
    Ok(Plan { items })
}

fn path_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(move |(index, _)| &path[..index])
}

fn folded_directories<T>(files: &BTreeMap<String, T>) -> BTreeMap<String, String> {
    files
        .keys()
        .flat_map(|path| path_prefixes(path))
        .map(|prefix| (portable_case_key(prefix), prefix.to_owned()))
        .collect()
}

fn target_replaces(path: &str, target: &BTreeMap<String, [u8; 32]>) -> bool {
    if path_prefixes(path).any(|prefix| target.contains_key(prefix)) {
        return true;
    }
    let prefix = format!("{path}/");
    target
        .range(prefix.clone()..)
        .next()
        .is_some_and(|(name, _)| name.starts_with(&prefix))
}

fn read_checked(root: &SafeRoot, path: &str, expected: &[u8; 32]) -> Result<Vec<u8>> {
    let bytes = root.required_file(path)?;
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if &actual != expected {
        bail!("snapshot changed during preview: {path}");
    }
    Ok(bytes)
}

fn read_tree(root: &SafeRoot) -> Result<BTreeMap<String, [u8; 32]>> {
    let files = root.hashes()?;
    for path in files.keys() {
        safe_relative(Path::new(path))?;
    }
    Ok(files)
}

fn safe_relative(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            bail!("unsafe snapshot path: {}", path.display());
        };
        let part = part.to_str().context("non-UTF-8 path in snapshot")?;
        if part.eq_ignore_ascii_case(".jsipack-updater") || part.eq_ignore_ascii_case(".git") {
            bail!("reserved path in snapshot: {}", path.display());
        }
        if part.ends_with([' ', '.'])
            || part
                .chars()
                .any(|c| c.is_control() || "<>:\"\\|?*".contains(c))
        {
            bail!("non-portable path in snapshot: {}", path.display());
        }
        let stem = part
            .split('.')
            .next()
            .unwrap()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let port_name = stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"));
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || port_name.is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        }) {
            bail!("reserved Windows name in snapshot: {}", path.display());
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

pub fn validate_source_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut collected = BTreeSet::new();
    for path in paths {
        if path.is_empty() || safe_relative(Path::new(path))? != path {
            bail!("unsafe source path: {path}");
        }
        collected.insert(path.to_owned());
    }
    check_collisions(&collected)
}

fn check_collisions(paths: &BTreeSet<String>) -> Result<()> {
    let mut folded = BTreeMap::<String, String>::new();
    let folded_files: BTreeSet<_> = paths.iter().map(|path| portable_case_key(path)).collect();
    for path in paths {
        let mut prefix = String::new();
        for part in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if let Some(previous) = folded.insert(portable_case_key(&prefix), prefix.clone())
                && previous != prefix
            {
                bail!("case-insensitive path collision: {previous} and {prefix}");
            }
        }
    }
    for path in paths {
        let mut prefix = String::new();
        for part in path.split('/').take(path.split('/').count() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if folded_files.contains(&portable_case_key(&prefix)) {
                bail!("file/directory path collision: {prefix} and {path}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_portability_hazards_without_host_filesystem_assumptions() {
        assert!(validate_source_paths([""]).is_err());
        let paths = ["Mods/A.txt", "mods/a.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(check_collisions(&paths).is_err());
        let directories = ["Mods/a.txt", "mods/b.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(check_collisions(&directories).is_err());
        let greek = ["mods/σ.txt", "mods/ς.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(check_collisions(&greek).is_err());
        assert!(safe_relative(Path::new("mods/COM¹.txt")).is_err());
        assert!(safe_relative(Path::new("config/NUL.txt")).is_err());
        assert!(safe_relative(Path::new("config/CONIN$.txt")).is_err());
        assert!(safe_relative(Path::new("config/NUL .txt")).is_err());
        assert!(safe_relative(Path::new("config/COM1 .cfg")).is_err());
    }
}
