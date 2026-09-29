use super::*;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use std::fs;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for name in ["base", "target", "instance"] {
            fs::create_dir_all(root.path().join(name).join("pack")).unwrap();
            fs::create_dir_all(root.path().join(name).join("mods/common")).unwrap();
        }
        let base_pack = "version = 'test1'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n";
        let target_pack =
            "version = 'test2'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n";
        fs::write(root.path().join("base/pack/pack.toml"), base_pack).unwrap();
        fs::write(root.path().join("target/pack/pack.toml"), target_pack).unwrap();
        fs::write(root.path().join("instance/mods/old.jar"), b"old").unwrap();
        fs::write(root.path().join("instance/mods/private.jar"), b"private").unwrap();
        fs::write(root.path().join("base/mods/common/old.pw.toml"), format!("name = 'Old'\nfilename = 'old.jar'\n[download]\nhash-format = 'sha1'\nhash = '{:x}'\n", Sha1::digest(b"old"))).unwrap();
        fs::write(
            root.path().join("target/mods/common/new.pw.toml"),
            "name = 'New'\nfilename = 'new.jar'\n",
        )
        .unwrap();
        fs::create_dir_all(root.path().join("instance/config")).unwrap();
        let marker = ReleaseInfo {
            schema_version: 1,
            version: "test1".into(),
            commit_id: "a".repeat(40),
            minecraft_version: "1.21.1".into(),
            neoforge_version: "21.1".into(),
        };
        fs::write(
            root.path().join("instance").join(MARKER),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let new_hash = format!("{:x}", Sha256::digest(b"new"));
        let old_hash = format!("{:x}", Sha256::digest(b"old"));
        let optional_hash = format!("{:x}", Sha256::digest(b"optional"));
        let script = format!(
            r##"#!/bin/sh
set -e
case "$2" in
  protocol-version)
    printf '%s\n' '{{"protocolVersion":1,"command":"protocol-version","sequence":1,"event":"started"}}' '{{"protocolVersion":1,"command":"protocol-version","sequence":2,"event":"completed","ok":true,"data":{{"protocolVersion":1}}}}'
    ;;
  prepare-pack)
    cp "$3/pack/pack.toml" "$3/pack.toml"
    cp "$3/pack/pack.toml" "$3/index.toml"
    if [ -f "$3/mods/common/old.pw.toml" ]; then
      hash=$(shasum -a 256 "$3/mods/common/old.pw.toml") || exit 1
      printf '%s\n' "$hash" | cut -d ' ' -f1 >> "$3/index.toml"
    fi
    if [ -f "$3/mods/common/optional.pw.toml" ]; then
      hash=$(shasum -a 256 "$3/mods/common/optional.pw.toml") || exit 1
      printf '%s\n' "$hash" | cut -d ' ' -f1 >> "$3/index.toml"
    fi
    printf '%s\n' '{{"protocolVersion":1,"command":"prepare-pack","sequence":1,"event":"started"}}' '{{"protocolVersion":1,"command":"prepare-pack","sequence":2,"event":"completed","ok":true}}'
    ;;
  sync)
    mkdir -p "$4/mods"
    files=''
    if [ -f "$3/mods/common/new.pw.toml" ]; then
      printf 'new' > "$4/mods/new.jar"
      files='{{"name":"New","path":"mods/new.jar","sha256":"{new_hash}"}}'
    fi
    if [ -f "$3/mods/common/old.pw.toml" ]; then
      printf 'old' > "$4/mods/old.jar"
      if [ -n "$files" ]; then files="$files,"; fi
      files="$files{{\"name\":\"Old\",\"path\":\"mods/old.jar\",\"sha256\":\"{old_hash}\"}}"
    fi
    if [ -f "$3/mods/common/optional.pw.toml" ] && grep -q 'default = true' "$3/mods/common/optional.pw.toml"; then
      optional_name=$(grep '^filename' "$3/mods/common/optional.pw.toml" | cut -d= -f2 | tr -d " '\"")
      printf 'optional' > "$4/mods/$optional_name"
      if [ -n "$files" ]; then files="$files,"; fi
      files="$files{{\"name\":\"Optional\",\"path\":\"mods/$optional_name\",\"sha256\":\"{optional_hash}\"}}"
    fi
    printf '{{"format":"bkmpw:1","side":"client","files":[%s]}}\n' "$files" > "$4/packwiz.json"
    printf '%s\n' '{{"protocolVersion":1,"command":"sync","sequence":1,"event":"started"}}' '{{"protocolVersion":1,"command":"sync","sequence":2,"event":"completed","ok":true}}'
    ;;
esac
"##
        );
        let path = root.path().join("fake-bkmpw");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn run(&self) -> Result<UpdateReport> {
        self.run_with(ConflictPolicy::Stop, BTreeMap::new())
    }

    fn run_with(
        &self,
        policy: ConflictPolicy,
        resolutions: BTreeMap<String, ConflictResolution>,
    ) -> Result<UpdateReport> {
        update(UpdateRequest {
            base_path: &self.root.path().join("base"),
            target_path: &self.root.path().join("target"),
            instance_path: &self.root.path().join("instance"),
            base_commit: &"a".repeat(40),
            target_commit: &"b".repeat(40),
            side: Side::Client,
            bkmpw_executable: &self.root.path().join("fake-bkmpw"),
            policy,
            resolutions,
        })
    }
}

#[test]
fn seeds_verified_managed_files_without_copying_local_modifications() {
    let root = TempDir::new().unwrap();
    let instance_path = root.path().join("instance");
    let artifacts_path = root.path().join("artifacts");
    fs::create_dir_all(instance_path.join("mods")).unwrap();
    fs::create_dir_all(&artifacts_path).unwrap();
    fs::write(instance_path.join("mods/good.jar"), b"good").unwrap();
    fs::write(instance_path.join("mods/changed.jar"), b"local change").unwrap();
    let manifest = Manifest {
        format: "bkmpw:1".into(),
        side: Side::Server,
        files: vec![
            crate::managed::ManagedFile {
                name: "good".into(),
                path: "mods/good.jar".into(),
                sha256: format!("{:x}", Sha256::digest(b"good")),
            },
            crate::managed::ManagedFile {
                name: "changed".into(),
                path: "mods/changed.jar".into(),
                sha256: format!("{:x}", Sha256::digest(b"original")),
            },
        ],
    };
    let conflicts = [crate::managed::AdoptionConflict {
        path: "mods/changed.jar".into(),
        reason: "managed_file_changed",
    }];
    let instance = SafeRoot::open(&instance_path).unwrap();
    let artifacts = SafeRoot::open(&artifacts_path).unwrap();
    seed_managed_artifacts(&instance, &artifacts, &manifest, &conflicts).unwrap();
    assert_eq!(
        fs::read(artifacts_path.join("mods/good.jar")).unwrap(),
        b"good"
    );
    assert!(!artifacts_path.join("mods/changed.jar").exists());
    let seeded: Manifest =
        serde_json::from_slice(&fs::read(artifacts_path.join("packwiz.json")).unwrap()).unwrap();
    assert_eq!(seeded.files.len(), 2);
}

#[test]
fn upgrades_and_removes_managed_jar_without_touching_private_jar() {
    let fixture = Fixture::new();
    let report = fixture.run().unwrap();
    assert_eq!(report.managed_files, 1);
    let instance = fixture.root.path().join("instance");
    assert_eq!(fs::read(instance.join("mods/new.jar")).unwrap(), b"new");
    assert!(!instance.join("mods/old.jar").exists());
    assert_eq!(
        fs::read(instance.join("mods/private.jar")).unwrap(),
        b"private"
    );
    let marker: ReleaseInfo =
        serde_json::from_slice(&fs::read(instance.join(MARKER)).unwrap()).unwrap();
    assert_eq!(marker.commit_id, "b".repeat(40));
    assert!(!transaction::pending(&instance).unwrap());
}

#[test]
fn private_target_collision_stops_before_changing_instance() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::write(instance.join("mods/new.jar"), b"private new").unwrap();
    assert!(
        fixture
            .run()
            .unwrap_err()
            .to_string()
            .contains("mods/new.jar")
    );
    assert_eq!(fs::read(instance.join("mods/old.jar")).unwrap(), b"old");
    let marker: ReleaseInfo =
        serde_json::from_slice(&fs::read(instance.join(MARKER)).unwrap()).unwrap();
    assert_eq!(marker.commit_id, "a".repeat(40));
}

#[test]
fn private_target_collision_can_be_kept_out_of_manifest() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::write(instance.join("mods/new.jar"), b"private new").unwrap();
    let choices = BTreeMap::from([("mods/new.jar".into(), ConflictResolution::KeepLocal)]);
    let report = fixture.run_with(ConflictPolicy::Stop, choices).unwrap();
    assert_eq!(report.managed_files, 0);
    assert_eq!(
        fs::read(instance.join("mods/new.jar")).unwrap(),
        b"private new"
    );
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(instance.join("packwiz.json")).unwrap()).unwrap();
    assert!(manifest.files.is_empty());
}

#[test]
fn explicit_artifact_choice_applies_without_detected_conflict() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([("mods/new.jar".into(), ConflictResolution::KeepLocal)]),
        )
        .unwrap();
    assert!(!instance.join("mods/new.jar").exists());
    let installed: Manifest =
        serde_json::from_slice(&fs::read(instance.join("packwiz.json")).unwrap()).unwrap();
    assert!(installed.files.is_empty());
}

#[test]
fn explicit_manual_choice_applies_to_unchanged_old_artifact() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let manual = fixture.root.path().join("manual-old.jar");
    fs::write(&manual, b"manual old").unwrap();
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([("mods/old.jar".into(), ConflictResolution::Manual(manual))]),
        )
        .unwrap();
    assert_eq!(
        fs::read(instance.join("mods/old.jar")).unwrap(),
        b"manual old"
    );
}

#[test]
fn missing_old_managed_artifact_agrees_with_target_removal() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let manifest = Manifest {
        format: "bkmpw:1".into(),
        side: Side::Client,
        files: vec![crate::managed::ManagedFile {
            name: "Old".into(),
            path: "mods/old.jar".into(),
            sha256: format!("{:x}", Sha256::digest(b"old")),
        }],
    };
    fs::write(
        instance.join("packwiz.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::remove_file(instance.join("mods/old.jar")).unwrap();
    fixture.run().unwrap();
    assert!(!instance.join("mods/old.jar").exists());
    assert!(instance.join("mods/new.jar").exists());
}

#[test]
fn stale_artifact_resolution_is_rejected() {
    let fixture = Fixture::new();
    let error = fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([("mods/typo.jar".into(), ConflictResolution::UseTarget)]),
        )
        .unwrap_err();
    assert!(error.to_string().contains("mods/typo.jar"));
    let marker: ReleaseInfo = serde_json::from_slice(
        &fs::read(fixture.root.path().join("instance").join(MARKER)).unwrap(),
    )
    .unwrap();
    assert_eq!(marker.commit_id, "a".repeat(40));
}

#[test]
fn modified_managed_jar_requires_choice_before_removal() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::write(instance.join("mods/old.jar"), b"player edit").unwrap();
    assert!(
        fixture
            .run()
            .unwrap_err()
            .to_string()
            .contains("mods/old.jar")
    );
    let choices = BTreeMap::from([("mods/old.jar".into(), ConflictResolution::KeepLocal)]);
    fixture.run_with(ConflictPolicy::Stop, choices).unwrap();
    assert_eq!(
        fs::read(instance.join("mods/old.jar")).unwrap(),
        b"player edit"
    );
}

#[test]
fn missing_managed_jar_can_stay_uninstalled() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::copy(
        fixture.root.path().join("base/mods/common/old.pw.toml"),
        fixture.root.path().join("target/mods/common/old.pw.toml"),
    )
    .unwrap();
    let manifest = Manifest {
        format: "bkmpw:1".into(),
        side: Side::Client,
        files: vec![crate::managed::ManagedFile {
            name: "Old".into(),
            path: "mods/old.jar".into(),
            sha256: format!("{:x}", Sha256::digest(b"old")),
        }],
    };
    fs::write(
        instance.join("packwiz.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::remove_file(instance.join("mods/old.jar")).unwrap();
    let choices = BTreeMap::from([("mods/old.jar".into(), ConflictResolution::KeepLocal)]);
    fixture.run_with(ConflictPolicy::Stop, choices).unwrap();
    assert!(!instance.join("mods/old.jar").exists());
    let installed: Manifest =
        serde_json::from_slice(&fs::read(instance.join("packwiz.json")).unwrap()).unwrap();
    assert!(
        !installed
            .files
            .iter()
            .any(|file| file.path == "mods/old.jar")
    );
    let third = fixture.root.path().join("third");
    fs::create_dir_all(third.join("pack")).unwrap();
    fs::create_dir_all(third.join("mods/common")).unwrap();
    fs::write(
        third.join("pack/pack.toml"),
        "version = 'test3'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n",
    )
    .unwrap();
    for descriptor in ["old", "new"] {
        fs::copy(
            fixture
                .root
                .path()
                .join(format!("target/mods/common/{descriptor}.pw.toml")),
            third.join(format!("mods/common/{descriptor}.pw.toml")),
        )
        .unwrap();
    }
    update(UpdateRequest {
        base_path: &fixture.root.path().join("target"),
        target_path: &third,
        instance_path: &instance,
        base_commit: &"b".repeat(40),
        target_commit: &"c".repeat(40),
        side: Side::Client,
        bkmpw_executable: &fixture.root.path().join("fake-bkmpw"),
        policy: ConflictPolicy::Stop,
        resolutions: BTreeMap::new(),
    })
    .unwrap();
    assert!(!instance.join("mods/old.jar").exists());
}

#[test]
fn saved_absence_can_be_explicitly_reinstalled() {
    for manual in [false, true] {
        let fixture = Fixture::new();
        let instance = fixture.root.path().join("instance");
        fs::copy(
            fixture.root.path().join("base/mods/common/old.pw.toml"),
            fixture.root.path().join("target/mods/common/old.pw.toml"),
        )
        .unwrap();
        let manifest = Manifest {
            format: "bkmpw:1".into(),
            side: Side::Client,
            files: vec![crate::managed::ManagedFile {
                name: "Old".into(),
                path: "mods/old.jar".into(),
                sha256: format!("{:x}", Sha256::digest(b"old")),
            }],
        };
        fs::write(
            instance.join("packwiz.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::remove_file(instance.join("mods/old.jar")).unwrap();
        fixture
            .run_with(
                ConflictPolicy::Stop,
                BTreeMap::from([("mods/old.jar".into(), ConflictResolution::KeepLocal)]),
            )
            .unwrap();
        let third = fixture.root.path().join("third");
        fs::create_dir_all(third.join("pack")).unwrap();
        fs::create_dir_all(third.join("mods/common")).unwrap();
        fs::write(
            third.join("pack/pack.toml"),
            "version = 'test3'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n",
        )
        .unwrap();
        for descriptor in ["old", "new"] {
            fs::copy(
                fixture
                    .root
                    .path()
                    .join(format!("target/mods/common/{descriptor}.pw.toml")),
                third.join(format!("mods/common/{descriptor}.pw.toml")),
            )
            .unwrap();
        }
        let replacement = fixture.root.path().join("manual.jar");
        fs::write(&replacement, b"manual replacement").unwrap();
        let choice = if manual {
            ConflictResolution::Manual(replacement)
        } else {
            ConflictResolution::UseTarget
        };
        update(UpdateRequest {
            base_path: &fixture.root.path().join("target"),
            target_path: &third,
            instance_path: &instance,
            base_commit: &"b".repeat(40),
            target_commit: &"c".repeat(40),
            side: Side::Client,
            bkmpw_executable: &fixture.root.path().join("fake-bkmpw"),
            policy: ConflictPolicy::Stop,
            resolutions: BTreeMap::from([("mods/old.jar".into(), choice)]),
        })
        .unwrap();
        assert_eq!(
            fs::read(instance.join("mods/old.jar")).unwrap(),
            if manual {
                b"manual replacement".as_slice()
            } else {
                b"old".as_slice()
            }
        );
    }
}

#[test]
fn already_installed_target_artifact_needs_no_conflict_choice() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::write(instance.join("mods/new.jar"), b"new").unwrap();
    let manifest = Manifest {
        format: "bkmpw:1".into(),
        side: Side::Client,
        files: vec![crate::managed::ManagedFile {
            name: "New".into(),
            path: "mods/new.jar".into(),
            sha256: format!("{:x}", Sha256::digest(b"old")),
        }],
    };
    fs::write(
        instance.join("packwiz.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fixture.run().unwrap();
    assert_eq!(fs::read(instance.join("mods/new.jar")).unwrap(), b"new");
}

#[test]
fn unverified_first_install_artifact_can_be_removed_or_manually_replaced() {
    for manual in [false, true] {
        let fixture = Fixture::new();
        let instance = fixture.root.path().join("instance");
        fs::write(instance.join("mods/old.jar"), b"player edit").unwrap();
        let replacement = fixture.root.path().join("manual.jar");
        fs::write(&replacement, b"manual jar").unwrap();
        let choice = if manual {
            ConflictResolution::Manual(replacement)
        } else {
            ConflictResolution::UseTarget
        };
        fixture
            .run_with(
                ConflictPolicy::Stop,
                BTreeMap::from([("mods/old.jar".into(), choice)]),
            )
            .unwrap();
        if manual {
            assert_eq!(
                fs::read(instance.join("mods/old.jar")).unwrap(),
                b"manual jar"
            );
        } else {
            assert!(!instance.join("mods/old.jar").exists());
        }
    }
}

#[test]
fn manual_deleted_descriptor_drives_generated_pack() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let old = fs::read_to_string(fixture.root.path().join("base/mods/common/old.pw.toml")).unwrap();
    fs::write(
        instance.join("mods/common/old.pw.toml"),
        format!("{old}\n# local\n"),
    )
    .unwrap();
    let manual = fixture.root.path().join("manual.pw.toml");
    let merged = format!("{old}\n# manual\n");
    fs::write(&manual, &merged).unwrap();
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([(
                "mods/common/old.pw.toml".into(),
                ConflictResolution::Manual(manual),
            )]),
        )
        .unwrap();
    assert_eq!(
        fs::read(instance.join("mods/common/old.pw.toml")).unwrap(),
        merged.as_bytes()
    );
    let index = fs::read_to_string(instance.join("index.toml")).unwrap();
    assert!(index.contains(&format!("{:x}", Sha256::digest(merged.as_bytes()))));
    assert!(instance.join("mods/old.jar").exists());
}

#[test]
fn missing_target_descriptor_can_be_kept_absent() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([(
                "mods/common/new.pw.toml".into(),
                ConflictResolution::KeepLocal,
            )]),
        )
        .unwrap();
    assert!(!instance.join("mods/common/new.pw.toml").exists());
    assert!(!instance.join("mods/new.jar").exists());
    let third = fixture.root.path().join("third");
    fs::create_dir_all(third.join("pack")).unwrap();
    fs::create_dir_all(third.join("mods/common")).unwrap();
    fs::write(
        third.join("pack/pack.toml"),
        "version = 'test3'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n",
    )
    .unwrap();
    fs::copy(
        fixture.root.path().join("target/mods/common/new.pw.toml"),
        third.join("mods/common/new.pw.toml"),
    )
    .unwrap();
    update(UpdateRequest {
        base_path: &fixture.root.path().join("target"),
        target_path: &third,
        instance_path: &instance,
        base_commit: &"b".repeat(40),
        target_commit: &"c".repeat(40),
        side: Side::Client,
        bkmpw_executable: &fixture.root.path().join("fake-bkmpw"),
        policy: ConflictPolicy::Stop,
        resolutions: BTreeMap::new(),
    })
    .unwrap();
    assert!(!instance.join("mods/common/new.pw.toml").exists());
    assert!(!instance.join("mods/new.jar").exists());
}

#[test]
fn manual_deleted_descriptor_works_when_local_file_is_missing() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let old = fs::read_to_string(fixture.root.path().join("base/mods/common/old.pw.toml")).unwrap();
    let manual = fixture.root.path().join("manual-old.pw.toml");
    fs::write(&manual, format!("{old}\n# manual missing\n")).unwrap();
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([(
                "mods/common/old.pw.toml".into(),
                ConflictResolution::Manual(manual),
            )]),
        )
        .unwrap();
    assert!(
        fs::read_to_string(instance.join("mods/common/old.pw.toml"))
            .unwrap()
            .contains("# manual missing")
    );
    assert!(instance.join("mods/old.jar").exists());
    assert!(
        fs::read_to_string(instance.join("index.toml"))
            .unwrap()
            .contains(&format!(
                "{:x}",
                Sha256::digest(fs::read(instance.join("mods/common/old.pw.toml")).unwrap())
            ))
    );
}

#[test]
fn effective_local_pack_cannot_change_minecraft_components() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::create_dir_all(instance.join("pack")).unwrap();
    fs::write(
        instance.join("pack/pack.toml"),
        "version = 'local'\n[versions]\nminecraft = '1.21.2'\nneoforge = '21.1'\n",
    )
    .unwrap();
    let error = fixture
        .run_with(ConflictPolicy::KeepLocal, BTreeMap::new())
        .unwrap_err();
    assert!(format!("{error:#}").contains("effective pack changes"));
    let marker: ReleaseInfo =
        serde_json::from_slice(&fs::read(instance.join(MARKER)).unwrap()).unwrap();
    assert_eq!(marker.commit_id, "a".repeat(40));
}

#[test]
fn unresolved_source_conflict_exposes_paths_for_interactive_ui() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::create_dir_all(instance.join("pack")).unwrap();
    fs::write(
        instance.join("pack/pack.toml"),
        "version = 'player'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n",
    )
    .unwrap();
    let error = fixture.run().unwrap_err();
    let conflicts = error.downcast_ref::<UnresolvedConflicts>().unwrap();
    assert!(conflicts.paths.contains(&"pack/pack.toml".to_owned()));
}

#[test]
fn successive_update_replaces_unmodified_generated_files() {
    let fixture = Fixture::new();
    fixture.run().unwrap();
    let third = fixture.root.path().join("third");
    fs::create_dir_all(third.join("pack")).unwrap();
    fs::create_dir_all(third.join("mods/common")).unwrap();
    fs::write(
        third.join("pack/pack.toml"),
        "version = 'test3'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n",
    )
    .unwrap();
    fs::copy(
        fixture.root.path().join("target/mods/common/new.pw.toml"),
        third.join("mods/common/new.pw.toml"),
    )
    .unwrap();
    update(UpdateRequest {
        base_path: &fixture.root.path().join("target"),
        target_path: &third,
        instance_path: &fixture.root.path().join("instance"),
        base_commit: &"b".repeat(40),
        target_commit: &"c".repeat(40),
        side: Side::Client,
        bkmpw_executable: &fixture.root.path().join("fake-bkmpw"),
        policy: ConflictPolicy::Stop,
        resolutions: BTreeMap::new(),
    })
    .unwrap();
    let instance = fixture.root.path().join("instance");
    assert!(
        fs::read_to_string(instance.join("pack.toml"))
            .unwrap()
            .contains("test3")
    );
    let marker: ReleaseInfo =
        serde_json::from_slice(&fs::read(instance.join(MARKER)).unwrap()).unwrap();
    assert_eq!(marker.commit_id, "c".repeat(40));
}

#[test]
fn first_update_recognizes_generated_pack_from_installer() {
    let fixture = Fixture::new();
    fs::copy(
        fixture.root.path().join("base/pack/pack.toml"),
        fixture.root.path().join("instance/pack.toml"),
    )
    .unwrap();
    fixture.run().unwrap();
    assert!(
        fs::read_to_string(fixture.root.path().join("instance/pack.toml"))
            .unwrap()
            .contains("test2")
    );
}

#[path = "updater_more_tests.rs"]
mod more;
