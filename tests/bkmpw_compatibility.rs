use minecraft_modpack_updater::{
    bkmpw,
    managed::{self, Manifest, Side},
    safe_fs::SafeRoot,
};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Release CI supplies the actual candidate, after SHA-256 verification.
#[test]
fn candidate_operations_match_updater_contract() {
    let Some(executable) = std::env::var_os("MMPU_TEST_BKMPW").map(PathBuf::from) else {
        return;
    };
    assert!(
        executable.is_file(),
        "candidate must exist; never fall back to a mock"
    );
    bkmpw::installed_version(&executable).unwrap();
    let fixture = minecraft_modpack_updater::safe_fs::temp_dir().unwrap();
    let source = SafeRoot::open(fixture.path()).unwrap();
    let payload: &[u8] = b"offline compatibility fixture";
    let digest = format!("{:x}", Sha256::digest(payload));
    source.write_atomic("pack.toml", b"name='Smoke'\npack-format='packwiz:1.1.0'\n[index]\nfile='index.toml'\nhash-format='sha256'\nhash=''\n[versions]\nminecraft='1.21.1'\n".as_slice()).unwrap();
    source.write_atomic(".pw/config.toml", b"[layout]\nmetadata-root='mods'\nmetadata-roots='mods,resourcepacks,shaderpacks'\njar-root='mods'\nclient-meta='mods/client'\nserver-meta='mods/server'\ncommon-meta='mods/common'\n[release]\ntemplate-dir='templates'\ntemplate-files='smoke.txt'\n".as_slice()).unwrap();
    source
        .write_atomic("templates/smoke.txt", b"rendered fixture".as_slice())
        .unwrap();
    source.write_atomic("mods/smoke.jar", payload).unwrap();
    source.write_atomic("mods/smoke.pw.toml", format!("name='Smoke'\nfilename='smoke.jar'\nside='both'\n[download]\nurl=''\nhash-format='sha256'\nhash='{digest}'\n").as_bytes()).unwrap();
    for side in ["client", "server"] {
        source
            .write_atomic(&format!("mods/{side}.jar"), payload)
            .unwrap();
        source.write_atomic(&format!("mods/{side}/{side}.pw.toml"), format!("name='{side}'\nfilename='{side}.jar'\nside='{side}'\n[download]\nurl=''\nhash-format='sha256'\nhash='{digest}'\n").as_bytes()).unwrap();
    }
    bkmpw::prepare_pack(&executable, fixture.path(), |_| {}).unwrap();
    assert_eq!(
        source.required_file("smoke.txt").unwrap(),
        b"rendered fixture"
    );
    for side in [Side::Client, Side::Server] {
        let destination = minecraft_modpack_updater::safe_fs::temp_dir().unwrap();
        let target = SafeRoot::open(destination.path()).unwrap();
        bkmpw::sync(
            &executable,
            fixture.path(),
            destination.path(),
            side,
            |_| {},
        )
        .unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&target.required_file("packwiz.json").unwrap()).unwrap();
        assert_eq!(manifest.side, side);
        let (included, excluded) = match side {
            Side::Client => ("client", "server"),
            Side::Server => ("server", "client"),
        };
        assert_eq!(manifest.files.len(), 2);
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.path == format!("mods/{included}.jar"))
        );
        assert!(
            !destination
                .path()
                .join(format!("mods/{excluded}.jar"))
                .exists()
        );
        assert_eq!(target.required_file("mods/smoke.jar").unwrap(), payload);
        assert!(
            managed::validate_manifest(&manifest, &target, side)
                .unwrap()
                .is_empty()
        );
        let metadata = fixture
            .path()
            .join(format!("mods/{included}/{included}.pw.toml"));
        let original = std::fs::read(&metadata).unwrap();
        std::fs::remove_file(&metadata).unwrap();
        bkmpw::sync(
            &executable,
            fixture.path(),
            destination.path(),
            side,
            |_| {},
        )
        .unwrap();
        let cleaned: Manifest =
            serde_json::from_slice(&target.required_file("packwiz.json").unwrap()).unwrap();
        assert_eq!(cleaned.files.len(), 1);
        assert_eq!(cleaned.files[0].path, "mods/smoke.jar");
        assert!(
            managed::validate_manifest(&cleaned, &target, side)
                .unwrap()
                .is_empty()
        );
        assert_eq!(target.required_file("mods/smoke.jar").unwrap(), payload);
        assert!(
            !destination
                .path()
                .join(format!("mods/{included}.jar"))
                .exists()
        );
        source
            .write_atomic(
                &format!("mods/{included}/{included}.pw.toml"),
                original.as_slice(),
            )
            .unwrap();
    }
}
