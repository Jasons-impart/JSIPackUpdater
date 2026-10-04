use super::{Metadata, Side, applies, destination};
use crate::safe_fs::SafeRoot;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModFile {
    pub name: String,
    pub path: String,
    pub optional: bool,
    pub default_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModChangeKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone)]
pub struct ModChange {
    pub kind: ModChangeKind,
    pub content_changed: bool,
    pub before: Option<ModFile>,
    pub after: Option<ModFile>,
}

#[derive(PartialEq, Eq)]
struct Entry {
    file: ModFile,
    hash: Option<(Option<String>, Option<String>)>,
}

fn catalog(root: &SafeRoot, side: Side) -> Result<BTreeMap<String, Entry>> {
    let mut entries = BTreeMap::new();
    for path in root
        .file_paths()?
        .into_iter()
        .filter(|path| path.starts_with("mods/") && path.ends_with(".pw.toml"))
    {
        let bytes = root.required_file(&path)?;
        let metadata: Metadata = toml::from_str(std::str::from_utf8(&bytes)?)
            .with_context(|| format!("parsing mod descriptor {path}"))?;
        if !applies(&path, &metadata, side)? {
            continue;
        }
        let target = destination(&path, &metadata.filename)?;
        if !target.starts_with("mods/") {
            continue;
        }
        let optional = metadata
            .option
            .as_ref()
            .is_some_and(|o| o.optional == Some(true));
        entries.insert(
            path,
            Entry {
                file: ModFile {
                    name: metadata.name,
                    path: target,
                    optional,
                    default_enabled: !optional
                        || metadata.option.is_some_and(|o| o.default == Some(true)),
                },
                hash: metadata.download.map(|d| {
                    (
                        d.hash_format.map(|f| f.to_ascii_lowercase()),
                        d.hash.map(|h| h.to_ascii_lowercase()),
                    )
                }),
            },
        );
    }
    Ok(entries)
}

/// Compare published mod catalogs, not the player's installed files. Descriptor
/// paths are the identity; renaming a descriptor is shown as removal + addition.
/// Optional entries include release defaults, independent of local selections.
pub fn mod_changes(base: &SafeRoot, target: &SafeRoot, side: Side) -> Result<Vec<ModChange>> {
    let before = catalog(base, side)?;
    let after = catalog(target, side)?;
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mut changes = Vec::new();
    for path in paths {
        let old = before.get(path);
        let new = after.get(path);
        if old == new {
            continue;
        }
        changes.push(ModChange {
            content_changed: old
                .zip(new)
                .is_some_and(|(a, b)| a.hash != b.hash || a.file.path != b.file.path),
            kind: match (old, new) {
                (None, Some(_)) => ModChangeKind::Added,
                (Some(_), None) => ModChangeKind::Removed,
                _ => ModChangeKind::Changed,
            },
            before: old.map(|e| e.file.clone()),
            after: new.map(|e| e.file.clone()),
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests;
