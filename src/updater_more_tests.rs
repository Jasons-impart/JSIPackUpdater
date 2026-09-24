use super::*;

#[test]
fn deleted_generated_file_honors_explicit_choice() {
    for manual in [false, true] {
        let fixture = Fixture::new();
        fixture.run().unwrap();
        let instance = fixture.root.path().join("instance");
        fs::remove_file(instance.join("index.toml")).unwrap();
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
        let merged = fixture.root.path().join("manual-index.toml");
        fs::write(&merged, b"manual index").unwrap();
        let choice = if manual {
            ConflictResolution::Manual(merged)
        } else {
            ConflictResolution::KeepLocal
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
            resolutions: BTreeMap::from([("index.toml".into(), choice)]),
        })
        .unwrap();
        if manual {
            assert_eq!(
                fs::read(instance.join("index.toml")).unwrap(),
                b"manual index"
            );
        } else {
            assert!(!instance.join("index.toml").exists());
        }
    }
}

#[test]
fn deleted_generated_file_requires_choice_when_target_changes() {
    let fixture = Fixture::new();
    fixture.run().unwrap();
    let instance = fixture.root.path().join("instance");
    fs::remove_file(instance.join("index.toml")).unwrap();
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
    let error = update(UpdateRequest {
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
    .unwrap_err();
    assert!(
        error
            .downcast_ref::<UnresolvedConflicts>()
            .unwrap()
            .paths
            .contains(&"index.toml".to_owned())
    );
}

#[test]
fn optional_selection_survives_unmanaged_local_artifact() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let descriptor = format!(
        "name = 'Optional'\nfilename = 'optional.jar'\n[option]\noptional = true\ndefault = true\n[download]\nhash-format = 'sha1'\nhash = '{:x}'\n",
        Sha1::digest(b"optional")
    );
    fs::write(
        fixture
            .root
            .path()
            .join("target/mods/common/optional.pw.toml"),
        &descriptor,
    )
    .unwrap();
    fs::write(instance.join("mods/optional.jar"), b"private optional").unwrap();
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([("mods/optional.jar".into(), ConflictResolution::KeepLocal)]),
        )
        .unwrap();
    let after_first: Manifest =
        serde_json::from_slice(&fs::read(instance.join("packwiz.json")).unwrap()).unwrap();
    assert!(
        !after_first
            .files
            .iter()
            .any(|file| file.path == "mods/optional.jar")
    );
    let third = fixture.root.path().join("third");
    fs::create_dir_all(third.join("pack")).unwrap();
    fs::create_dir_all(third.join("mods/common")).unwrap();
    fs::write(
        third.join("pack/pack.toml"),
        "version = 'test3'\n[versions]\nminecraft = '1.21.1'\nneoforge = '21.1'\n",
    )
    .unwrap();
    for name in ["new", "optional"] {
        fs::copy(
            fixture
                .root
                .path()
                .join("target/mods/common")
                .join(format!("{name}.pw.toml")),
            third.join("mods/common").join(format!("{name}.pw.toml")),
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
        resolutions: BTreeMap::from([("mods/optional.jar".into(), ConflictResolution::UseTarget)]),
    })
    .unwrap();
    assert_eq!(
        fs::read(instance.join("mods/optional.jar")).unwrap(),
        b"optional"
    );
    let installed: Manifest =
        serde_json::from_slice(&fs::read(instance.join("packwiz.json")).unwrap()).unwrap();
    assert!(
        installed
            .files
            .iter()
            .any(|file| file.path == "mods/optional.jar")
    );
}

#[test]
fn first_adoption_modified_optional_can_choose_target() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let descriptor = format!(
        "name = 'Optional'\nfilename = 'optional.jar'\n[option]\noptional = true\ndefault = false\n[download]\nhash-format = 'sha1'\nhash = '{:x}'\n",
        Sha1::digest(b"optional")
    );
    for tree in ["base", "target"] {
        fs::write(
            fixture
                .root
                .path()
                .join(tree)
                .join("mods/common/optional.pw.toml"),
            &descriptor,
        )
        .unwrap();
    }
    fs::write(instance.join("mods/optional.jar"), b"player edit").unwrap();
    fixture
        .run_with(
            ConflictPolicy::Stop,
            BTreeMap::from([("mods/optional.jar".into(), ConflictResolution::UseTarget)]),
        )
        .unwrap();
    assert_eq!(
        fs::read(instance.join("mods/optional.jar")).unwrap(),
        b"optional"
    );
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(instance.join("packwiz.json")).unwrap()).unwrap();
    assert!(
        manifest
            .files
            .iter()
            .any(|file| file.path == "mods/optional.jar")
    );
}

#[test]
fn retained_optional_choice_matches_installed_index_metadata() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let descriptor = format!(
        "name = 'Optional'\nfilename = 'optional.jar'\n[option]\noptional = true\ndefault = false\n[download]\nhash-format = 'sha1'\nhash = '{:x}'\n",
        Sha1::digest(b"optional")
    );
    fs::write(
        fixture
            .root
            .path()
            .join("base/mods/common/optional.pw.toml"),
        &descriptor,
    )
    .unwrap();
    fs::write(
        fixture
            .root
            .path()
            .join("target/mods/common/optional.pw.toml"),
        &descriptor,
    )
    .unwrap();
    fs::write(instance.join("mods/common/optional.pw.toml"), &descriptor).unwrap();
    fs::write(instance.join("mods/optional.jar"), b"optional").unwrap();
    let report = fixture.run().unwrap();
    assert_eq!(report.managed_files, 2);
    assert_eq!(
        fs::read(instance.join("mods/optional.jar")).unwrap(),
        b"optional"
    );
    let installed = fs::read(instance.join("mods/common/optional.pw.toml")).unwrap();
    assert!(String::from_utf8_lossy(&installed).contains("default = true"));
    let index = fs::read_to_string(instance.join("index.toml")).unwrap();
    assert!(index.contains(&format!("{:x}", Sha256::digest(&installed))));
}

#[test]
fn optional_choice_rewrites_retained_local_descriptor_consistently() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let descriptor = format!(
        "name = 'Optional'\nfilename = 'optional.jar'\n[option]\noptional = true\ndefault = true\n[download]\nhash-format = 'sha1'\nhash = '{:x}'\n",
        Sha1::digest(b"optional")
    );
    for tree in ["base", "target"] {
        fs::write(
            fixture
                .root
                .path()
                .join(tree)
                .join("mods/common/optional.pw.toml"),
            &descriptor,
        )
        .unwrap();
    }
    fs::write(
        instance.join("mods/common/optional.pw.toml"),
        descriptor.replace("default = true", "default = false"),
    )
    .unwrap();
    fs::write(instance.join("mods/optional.jar"), b"optional").unwrap();
    fixture.run().unwrap();
    let installed = fs::read(instance.join("mods/common/optional.pw.toml")).unwrap();
    assert!(String::from_utf8_lossy(&installed).contains("default = true"));
    let index = fs::read_to_string(instance.join("index.toml")).unwrap();
    assert!(index.contains(&format!("{:x}", Sha256::digest(&installed))));
}

#[test]
fn selected_optional_follows_renamed_artifact_across_updates() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let descriptor = |name: &str| {
        format!(
            "name = 'Optional'\nfilename = '{name}'\n[option]\noptional = true\ndefault = false\n[download]\nhash-format = 'sha1'\nhash = '{:x}'\n",
            Sha1::digest(b"optional")
        )
    };
    fs::write(
        fixture
            .root
            .path()
            .join("base/mods/common/optional.pw.toml"),
        descriptor("optional-v1.jar"),
    )
    .unwrap();
    fs::write(
        fixture
            .root
            .path()
            .join("target/mods/common/optional.pw.toml"),
        descriptor("optional-v2.jar"),
    )
    .unwrap();
    fs::write(instance.join("mods/optional-v1.jar"), b"optional").unwrap();
    fixture.run().unwrap();
    assert!(!instance.join("mods/optional-v1.jar").exists());
    assert_eq!(
        fs::read(instance.join("mods/optional-v2.jar")).unwrap(),
        b"optional"
    );
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
    fs::write(
        third.join("mods/common/optional.pw.toml"),
        descriptor("optional-v3.jar"),
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
        policy: ConflictPolicy::UseTarget,
        resolutions: BTreeMap::new(),
    })
    .unwrap();
    assert!(!instance.join("mods/optional-v2.jar").exists());
    assert_eq!(
        fs::read(instance.join("mods/optional-v3.jar")).unwrap(),
        b"optional"
    );
}

#[test]
fn keeping_local_removed_descriptor_retains_its_artifact() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    let old = fs::read_to_string(fixture.root.path().join("base/mods/common/old.pw.toml")).unwrap();
    let edited = format!("{old}\n# player edit\n");
    fs::write(instance.join("mods/common/old.pw.toml"), &edited).unwrap();
    let report = update(UpdateRequest {
        base_path: &fixture.root.path().join("base"),
        target_path: &fixture.root.path().join("target"),
        instance_path: &instance,
        base_commit: &"a".repeat(40),
        target_commit: &"b".repeat(40),
        side: Side::Client,
        bkmpw_executable: &fixture.root.path().join("fake-bkmpw"),
        policy: ConflictPolicy::KeepLocal,
        resolutions: BTreeMap::new(),
    })
    .unwrap();
    assert_eq!(report.managed_files, 2);
    assert_eq!(fs::read(instance.join("mods/old.jar")).unwrap(), b"old");
    assert_eq!(
        fs::read(instance.join("mods/common/old.pw.toml")).unwrap(),
        edited.as_bytes()
    );
    let index = fs::read_to_string(instance.join("index.toml")).unwrap();
    assert!(index.contains(&format!("{:x}", Sha256::digest(edited.as_bytes()))));
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
    assert_eq!(fs::read(instance.join("mods/old.jar")).unwrap(), b"old");
    assert_eq!(
        fs::read(instance.join("mods/common/old.pw.toml")).unwrap(),
        edited.as_bytes()
    );
    let overrides: OverrideState = serde_json::from_slice(
        &fs::read(instance.join(".jsipack-updater/overrides.json")).unwrap(),
    )
    .unwrap();
    assert!(overrides.paths.contains(&"mods/common/old.pw.toml".into()));
}

#[test]
fn saved_descriptor_override_honors_later_target_or_manual_choice() {
    for manual in [false, true] {
        let fixture = Fixture::new();
        let instance = fixture.root.path().join("instance");
        let descriptor = "mods/common/old.pw.toml";
        let old = fs::read_to_string(fixture.root.path().join("base").join(descriptor)).unwrap();
        fs::write(instance.join(descriptor), format!("{old}\n# local\n")).unwrap();
        fixture
            .run_with(ConflictPolicy::KeepLocal, BTreeMap::new())
            .unwrap();
        if manual {
            fs::remove_file(instance.join(descriptor)).unwrap();
        }
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
        let merged = fixture.root.path().join("manual-old.pw.toml");
        fs::write(&merged, format!("{old}\n# manual\n")).unwrap();
        let choice = if manual {
            ConflictResolution::Manual(merged)
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
            resolutions: BTreeMap::from([(descriptor.into(), choice)]),
        })
        .unwrap();
        if manual {
            assert!(
                fs::read_to_string(instance.join(descriptor))
                    .unwrap()
                    .contains("# manual")
            );
            assert!(instance.join("mods/old.jar").exists());
        } else {
            assert!(!instance.join(descriptor).exists());
            assert!(!instance.join("mods/old.jar").exists());
        }
    }
}

#[test]
fn saved_tracked_override_can_be_reset_to_target() {
    let fixture = Fixture::new();
    let instance = fixture.root.path().join("instance");
    fs::create_dir_all(instance.join("pack")).unwrap();
    let old = fs::read_to_string(fixture.root.path().join("base/pack/pack.toml")).unwrap();
    fs::write(
        instance.join("pack/pack.toml"),
        format!("{old}\n# player\n"),
    )
    .unwrap();
    fixture
        .run_with(ConflictPolicy::KeepLocal, BTreeMap::new())
        .unwrap();
    let third = fixture.root.path().join("third");
    fs::create_dir_all(third.join("pack")).unwrap();
    fs::create_dir_all(third.join("mods/common")).unwrap();
    fs::copy(
        fixture.root.path().join("target/pack/pack.toml"),
        third.join("pack/pack.toml"),
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
        resolutions: BTreeMap::from([("pack/pack.toml".into(), ConflictResolution::UseTarget)]),
    })
    .unwrap();
    assert_eq!(
        fs::read(instance.join("pack/pack.toml")).unwrap(),
        fs::read(fixture.root.path().join("target/pack/pack.toml")).unwrap()
    );
}
