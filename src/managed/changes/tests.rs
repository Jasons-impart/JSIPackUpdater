use super::*;
use tempfile::TempDir;

fn descriptor(root: &SafeRoot, path: &str, filename: &str, hash: &str, extra: &str) {
    root.write_atomic(path, format!(
        "name = 'Example'\nfilename = '{filename}'\n[download]\nhash-format = 'sha256'\nhash = '{hash}'\n{extra}"
    ).as_bytes()).unwrap();
}

#[test]
fn classifies_release_changes_and_detects_same_filename_content_change() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let a = SafeRoot::open(a.path()).unwrap();
    let b = SafeRoot::open(b.path()).unwrap();
    for root in [&a, &b] {
        descriptor(
            root,
            "mods/common/unchanged.pw.toml",
            "same.jar",
            "ABCD",
            "",
        );
    }
    descriptor(&b, "mods/common/unchanged.pw.toml", "same.jar", "abcd", "");
    descriptor(&a, "mods/common/version.pw.toml", "old.jar", "a", "");
    descriptor(&b, "mods/common/version.pw.toml", "new.jar", "a", "");
    descriptor(&a, "mods/common/content.pw.toml", "content.jar", "a", "");
    descriptor(&b, "mods/common/content.pw.toml", "content.jar", "b", "");
    descriptor(&a, "mods/common/removed.pw.toml", "removed.jar", "a", "");
    descriptor(&b, "mods/common/added.pw.toml", "added.jar", "a", "");
    let changes = mod_changes(&a, &b, Side::Client).unwrap();
    assert_eq!(changes.len(), 4);
    assert_eq!(
        changes
            .iter()
            .filter(|c| c.kind == ModChangeKind::Changed)
            .count(),
        2
    );
    let replaced = changes
        .iter()
        .find(|c| c.after.as_ref().is_some_and(|f| f.path == "mods/new.jar"))
        .unwrap();
    assert_eq!(replaced.before.as_ref().unwrap().path, "mods/old.jar");
    assert!(replaced.content_changed);
    let content = changes
        .iter()
        .find(|c| {
            c.after
                .as_ref()
                .is_some_and(|f| f.path == "mods/content.jar")
        })
        .unwrap();
    assert!(content.content_changed);
    assert_eq!(content.before, content.after);
    assert!(
        changes
            .iter()
            .any(|c| c.kind == ModChangeKind::Added && c.before.is_none())
    );
    assert!(
        changes
            .iter()
            .any(|c| c.kind == ModChangeKind::Removed && c.after.is_none())
    );
}

#[test]
fn filters_side_and_non_mod_files_and_reports_optional_defaults() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let a = SafeRoot::open(a.path()).unwrap();
    let b = SafeRoot::open(b.path()).unwrap();
    descriptor(&b, "mods/client/client.pw.toml", "client.jar", "a", "");
    descriptor(&b, "mods/server/server.pw.toml", "server.jar", "a", "");
    descriptor(&b, "resourcepacks/pack.pw.toml", "pack.zip", "a", "");
    descriptor(
        &a,
        "mods/common/optional.pw.toml",
        "optional.jar",
        "a",
        "[option]\noptional = true\ndefault = false",
    );
    descriptor(
        &b,
        "mods/common/optional.pw.toml",
        "optional.jar",
        "a",
        "[option]\noptional = true\ndefault = true",
    );
    for (side, expected) in [
        (Side::Client, "mods/client.jar"),
        (Side::Server, "mods/server.jar"),
    ] {
        let changes = mod_changes(&a, &b, side).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .any(|c| c.after.as_ref().unwrap().path == expected)
        );
        let optional = changes
            .iter()
            .find(|c| c.kind == ModChangeKind::Changed)
            .unwrap();
        assert!(optional.before.as_ref().unwrap().optional);
        assert!(!optional.before.as_ref().unwrap().default_enabled);
        assert!(optional.after.as_ref().unwrap().default_enabled);
        assert!(!optional.content_changed);
    }
}

#[test]
fn descriptor_renames_are_not_guessed_from_non_unique_display_names() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let a = SafeRoot::open(a.path()).unwrap();
    let b = SafeRoot::open(b.path()).unwrap();
    descriptor(&a, "mods/old.pw.toml", "old.jar", "a", "");
    descriptor(&b, "mods/new.pw.toml", "new.jar", "b", "");
    let changes = mod_changes(&a, &b, Side::Client).unwrap();
    assert_eq!(changes.len(), 2);
    assert!(!changes.iter().any(|c| c.kind == ModChangeKind::Changed));
    b.write_atomic("mods/new.pw.toml", &b"invalid toml"[..])
        .unwrap();
    assert!(mod_changes(&a, &b, Side::Client).is_err());
}
