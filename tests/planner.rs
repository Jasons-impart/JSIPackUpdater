use minecraft_modpack_updater::planner::{
    Action, LocalFile, SourceChange, decide, plan_directories,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn file(value: &str) -> LocalFile {
    LocalFile::File(value.as_bytes().to_vec())
}

#[test]
fn three_way_matrix() {
    macro_rules! check {
        ($name:expr, $a:expr, $l:expr, $b:expr, $expected:expr) => {
            assert_eq!(
                decide($a.map(str::as_bytes), &$l, $b.map(str::as_bytes)).map(|(action, _)| action),
                $expected,
                $name
            );
        };
    }
    check!("unchanged", Some("a"), file("a"), Some("a"), None);
    check!(
        "target edit",
        Some("a"),
        file("a"),
        Some("b"),
        Some(Action::WriteTarget)
    );
    check!(
        "target delete",
        Some("a"),
        file("a"),
        None,
        Some(Action::Delete)
    );
    check!(
        "target add",
        None,
        LocalFile::Missing,
        Some("b"),
        Some(Action::WriteTarget)
    );
    check!(
        "local override",
        Some("a"),
        file("local"),
        Some("a"),
        Some(Action::KeepLocal)
    );
    check!(
        "already target",
        Some("a"),
        file("b"),
        Some("b"),
        Some(Action::AlreadyTarget)
    );
    check!(
        "missing tracked",
        Some("a"),
        LocalFile::Missing,
        Some("a"),
        Some(Action::WriteTarget)
    );
    check!(
        "changed missing",
        Some("a"),
        LocalFile::Missing,
        Some("b"),
        Some(Action::WriteTarget)
    );
    check!(
        "both add same",
        None,
        file("b"),
        Some("b"),
        Some(Action::AlreadyTarget)
    );
    check!(
        "both add different",
        None,
        file("local"),
        Some("b"),
        Some(Action::Conflict)
    );
    check!(
        "local edit delete",
        Some("a"),
        file("local"),
        None,
        Some(Action::Conflict)
    );
    check!(
        "obstacle",
        None,
        LocalFile::Obstacle,
        Some("b"),
        Some(Action::Conflict)
    );
}

#[test]
fn merges_only_disjoint_text_changes() {
    let base = b"one\ntwo\nthree\n";
    assert_eq!(
        decide(
            Some(base),
            &LocalFile::File(b"one\ntwo\nlocal\n".to_vec()),
            Some(b"target\ntwo\nthree\n")
        )
        .unwrap()
        .0,
        Action::Merge
    );
    assert_eq!(
        decide(
            Some(base),
            &LocalFile::File(b"local\ntwo\nthree\n".to_vec()),
            Some(b"target\ntwo\nthree\n")
        )
        .unwrap()
        .0,
        Action::Conflict
    );
    assert_eq!(
        decide(
            Some(b"\0old"),
            &LocalFile::File(b"\0local".to_vec()),
            Some(b"\0target")
        )
        .unwrap()
        .0,
        Action::Conflict
    );
}

#[test]
fn large_text_merge_is_reported_as_conflict() {
    let base = "a\n".repeat(16_000);
    let local = "b\n".repeat(16_000);
    let target = "c\n".repeat(16_000);
    let decision = decide(
        Some(base.as_bytes()),
        &LocalFile::File(local.into_bytes()),
        Some(target.as_bytes()),
    )
    .unwrap();
    assert_eq!(decision, (Action::Conflict, "text_merge_resource_limit"));
}

fn write(root: &Path, path: &str, contents: &str) {
    let destination = root.join(path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(destination, contents).unwrap();
}

#[test]
fn fixture_plan_includes_missing_files_and_private_collisions() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    for root in [&base, &target, &instance] {
        fs::create_dir(root).unwrap();
    }
    write(&base, "config/missing.txt", "tracked");
    write(&target, "config/missing.txt", "tracked");
    write(&base, "config/local.txt", "old");
    write(&target, "config/local.txt", "old");
    write(&instance, "config/local.txt", "custom");
    write(&base, "config/deleted.txt", "remove");
    write(&instance, "config/deleted.txt", "remove");
    write(&target, "config/new.txt", "added");
    write(&target, "mods/private.pw.toml", "new metadata");
    write(&instance, "mods/private.pw.toml", "private metadata");
    write(&instance, "saves/world/data", "do not touch");
    let plan = plan_directories(&base, &target, &instance).unwrap();
    let actions: Vec<_> = plan
        .items
        .iter()
        .map(|item| (item.path.as_str(), item.action))
        .collect();
    assert_eq!(
        actions,
        [
            ("config/deleted.txt", Action::Delete),
            ("config/local.txt", Action::KeepLocal),
            ("config/missing.txt", Action::WriteTarget),
            ("config/new.txt", Action::WriteTarget),
            ("mods/private.pw.toml", Action::Conflict),
        ]
    );
    assert_eq!(plan.items[3].source_change, SourceChange::Added);
    assert!(
        !plan
            .items
            .iter()
            .any(|item| item.path.starts_with("saves/"))
    );
    assert_eq!(
        fs::read_to_string(instance.join("config/deleted.txt")).unwrap(),
        "remove"
    );
}

#[test]
fn rejects_case_collisions_and_reserved_paths() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    for root in [&base, &target, &instance] {
        fs::create_dir(root).unwrap();
    }
    write(&target, ".jsipack-updater/state.json", "unsafe");
    assert!(
        plan_directories(&base, &target, &instance)
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );
}

#[test]
fn file_to_directory_transition_is_planned_as_conflict() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    write(&base, "config", "old");
    write(&target, "config/options.txt", "new");
    write(&instance, "config", "old");
    let plan = plan_directories(&base, &target, &instance).unwrap();
    assert_eq!(plan.items[0].action, Action::Delete);
    assert_eq!(plan.items[1].action, Action::Conflict);
}

#[test]
fn fully_applied_file_directory_transitions_need_no_conflict() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    write(&base, "config", "old");
    write(&target, "config/options.txt", "new");
    write(&instance, "config/options.txt", "new");
    assert!(
        !plan_directories(&base, &target, &instance)
            .unwrap()
            .has_conflicts()
    );
    fs::remove_dir_all(&base).unwrap();
    fs::remove_dir_all(&target).unwrap();
    fs::remove_dir_all(&instance).unwrap();
    write(&base, "config/options.txt", "old");
    write(&target, "config", "new");
    write(&instance, "config", "new");
    assert!(
        !plan_directories(&base, &target, &instance)
            .unwrap()
            .has_conflicts()
    );
}

#[test]
fn case_only_directory_rename_is_a_conflict() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    write(&base, "Mods/old.txt", "old");
    write(&target, "mods/new.txt", "new");
    write(&instance, "Mods/old.txt", "old");
    let plan = plan_directories(&base, &target, &instance).unwrap();
    assert_eq!(plan.items.len(), 2);
    assert!(
        plan.items
            .iter()
            .all(|item| item.action == Action::Conflict)
    );
}

#[test]
fn private_directory_case_collision_is_a_conflict() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    fs::create_dir(&base).unwrap();
    write(&target, "Mods/new.txt", "new");
    write(&instance, "mods/private.txt", "private");
    let plan = plan_directories(&base, &target, &instance).unwrap();
    assert_eq!(plan.items[0].reason, "instance_case_collision");
}

#[test]
fn deleted_path_with_different_local_case_is_a_conflict() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    write(&base, "config/a.txt", "old");
    fs::create_dir(&target).unwrap();
    write(&instance, "config/A.txt", "old");
    let plan = plan_directories(&base, &target, &instance).unwrap();
    assert_eq!(plan.items[0].reason, "instance_case_collision");
}

#[test]
fn unicode_case_equivalent_instance_name_conflicts() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    fs::create_dir(&base).unwrap();
    write(&target, "mods/σ.txt", "new");
    write(&instance, "mods/ς.txt", "private");
    let plan = plan_directories(&base, &target, &instance).unwrap();
    assert_eq!(plan.items[0].reason, "instance_case_collision");
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_instance_path() {
    use std::os::unix::fs::symlink;
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    for root in [&base, &target, &instance] {
        fs::create_dir(root).unwrap();
    }
    write(&target, "config/options.txt", "new");
    symlink(&target, instance.join("config")).unwrap();
    assert!(
        plan_directories(&base, &target, &instance)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
}

#[test]
fn cli_json_preview_reports_conflicts_without_writing() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    for root in [&base, &target, &instance] {
        fs::create_dir(root).unwrap();
    }
    write(&base, "config/a.txt", "base");
    write(&target, "config/a.txt", "target");
    write(&instance, "config/a.txt", "local");
    let pack = "[versions]\nminecraft = \"1.21.1\"\nneoforge = \"21.1.242\"\n";
    write(&base, "pack/pack.toml", pack);
    write(&target, "pack/pack.toml", pack);
    write(&instance, "pack/pack.toml", pack);
    write(
        &instance,
        "config/createdelight_release_info.json",
        &format!(
            "{{\"schemaVersion\":1,\"version\":\"v1\",\"commitId\":\"{}\",\"minecraftVersion\":\"1.21.1\",\"neoforgeVersion\":\"21.1.242\"}}",
            "a".repeat(40)
        ),
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_minecraft-modpack-updater"))
        .args(["preview", "--base-commit", &"a".repeat(40), "--base-tree"])
        .arg(&base)
        .arg("--target-tree")
        .arg(&target)
        .arg("--instance")
        .arg(&instance)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["items"][0]["action"], "conflict");
    assert_eq!(fs::read(instance.join("config/a.txt")).unwrap(), b"local");
}

#[test]
fn cli_rejects_mismatched_base_commit_with_same_component_versions() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    let pack = "[versions]\nminecraft='1.21.1'\nneoforge='21.1.242'";
    write(&base, "pack/pack.toml", pack);
    write(&target, "pack/pack.toml", pack);
    write(
        &instance,
        "config/createdelight_release_info.json",
        &format!(
            "{{\"schemaVersion\":1,\"version\":\"v1\",\"commitId\":\"{}\",\"minecraftVersion\":\"1.21.1\",\"neoforgeVersion\":\"21.1.242\"}}",
            "a".repeat(40)
        ),
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_minecraft-modpack-updater"))
        .args(["preview", "--base-commit", &"b".repeat(40), "--base-tree"])
        .arg(&base)
        .arg("--target-tree")
        .arg(&target)
        .arg("--instance")
        .arg(&instance)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("base snapshot commit"));
}

#[test]
fn component_change_stops_preview() {
    use minecraft_modpack_updater::version::check_components;
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    write(
        &base,
        "pack/pack.toml",
        "[versions]\nminecraft='1.21.1'\nneoforge='21.1.242'",
    );
    write(
        &target,
        "pack/pack.toml",
        "[versions]\nminecraft='1.21.2'\nneoforge='21.1.242'",
    );
    write(
        &instance,
        "config/createdelight_release_info.json",
        &format!(
            "{{\"schemaVersion\":1,\"version\":\"v1\",\"commitId\":\"{}\",\"minecraftVersion\":\"1.21.1\",\"neoforgeVersion\":\"21.1.242\"}}",
            "a".repeat(40)
        ),
    );
    assert!(
        check_components(&base, &target, &instance)
            .unwrap_err()
            .to_string()
            .contains("create a new instance")
    );
}

#[test]
fn descriptor_change_after_validation_invalidates_plan() {
    use minecraft_modpack_updater::planner::plan_roots_checked;
    use minecraft_modpack_updater::safe_fs::SafeRoot;
    use minecraft_modpack_updater::version::check_components_roots;
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    let pack = "[versions]\nminecraft='1.21.1'\nneoforge='21.1.242'";
    write(&base, "pack/pack.toml", pack);
    write(&target, "pack/pack.toml", pack);
    write(
        &instance,
        "config/createdelight_release_info.json",
        &format!(
            "{{\"schemaVersion\":1,\"version\":\"v1\",\"commitId\":\"{}\",\"minecraftVersion\":\"1.21.1\",\"neoforgeVersion\":\"21.1.242\"}}",
            "a".repeat(40)
        ),
    );
    let base_root = SafeRoot::open(&base).unwrap();
    let target_root = SafeRoot::open(&target).unwrap();
    let instance_root = SafeRoot::open(&instance).unwrap();
    let check = check_components_roots(&base_root, &target_root, &instance_root).unwrap();
    write(
        &target,
        "pack/pack.toml",
        "[versions]\nminecraft='1.21.2'\nneoforge='21.1.242'",
    );
    assert!(
        plan_roots_checked(&base_root, &target_root, &instance_root, &check)
            .unwrap_err()
            .to_string()
            .contains("changed after")
    );
}

#[test]
fn oversized_file_fails_before_allocation() {
    use minecraft_modpack_updater::safe_fs::SafeRoot;
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("large.bin");
    fs::File::create(&path)
        .unwrap()
        .set_len(65 * 1024 * 1024)
        .unwrap();
    let root = SafeRoot::open(fixture.path()).unwrap();
    assert!(
        root.required_file("large.bin")
            .unwrap_err()
            .to_string()
            .contains("64 MiB")
    );
}

#[test]
fn oversized_snapshot_fails_before_hashing() {
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&target).unwrap();
    fs::create_dir(&instance).unwrap();
    fs::File::create(base.join("large.bin"))
        .unwrap()
        .set_len(65 * 1024 * 1024)
        .unwrap();
    assert!(
        plan_directories(&base, &target, &instance)
            .unwrap_err()
            .to_string()
            .contains("64 MiB")
    );
}

#[cfg(unix)]
#[test]
fn metadata_symlink_is_rejected_before_reading() {
    use minecraft_modpack_updater::version::check_components;
    use std::os::unix::fs::symlink;
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    write(
        &base,
        "pack/pack.toml",
        "[versions]\nminecraft='1'\nneoforge='1'",
    );
    write(
        &target,
        "pack/pack.toml",
        "[versions]\nminecraft='1'\nneoforge='1'",
    );
    fs::create_dir_all(instance.join("config")).unwrap();
    symlink(
        "/dev/zero",
        instance.join("config/createdelight_release_info.json"),
    )
    .unwrap();
    assert!(
        check_components(&base, &target, &instance)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
}

#[cfg(unix)]
#[test]
fn symlinked_root_is_rejected_with_trailing_components() {
    use minecraft_modpack_updater::safe_fs::SafeRoot;
    use std::os::unix::fs::symlink;
    let fixture = TempDir::new().unwrap();
    let target = fixture.path().join("target");
    fs::create_dir(&target).unwrap();
    let link = fixture.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(SafeRoot::open(&link).is_err());
    assert!(SafeRoot::open(&link.join("")).is_err());
    assert!(SafeRoot::open(&link.join(".")).is_err());
    let nested = link.join("nested");
    fs::create_dir(target.join("nested")).unwrap();
    assert!(SafeRoot::open(&nested).is_err());
}

#[cfg(unix)]
#[test]
fn pinned_root_reads_original_directory_after_rename() {
    use minecraft_modpack_updater::safe_fs::SafeRoot;
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("root");
    write(&root, "value.txt", "old");
    let pinned = SafeRoot::open(&root).unwrap();
    fs::rename(&root, fixture.path().join("moved")).unwrap();
    write(&root, "value.txt", "replacement");
    assert_eq!(pinned.required_file("value.txt").unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn unrelated_non_utf8_instance_file_does_not_block_preview() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = TempDir::new().unwrap();
    let base = fixture.path().join("base");
    let target = fixture.path().join("target");
    let instance = fixture.path().join("instance");
    fs::create_dir(&base).unwrap();
    write(&target, "config/options.txt", "new");
    fs::create_dir(&instance).unwrap();
    fs::write(
        instance.join(std::ffi::OsString::from_vec(vec![0xff])),
        b"private",
    )
    .unwrap();
    assert!(
        !plan_directories(&base, &target, &instance)
            .unwrap()
            .has_conflicts()
    );
}
