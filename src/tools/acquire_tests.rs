use super::*;
use std::{os::unix::fs::PermissionsExt, process::Command};

fn executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn bkmpw_script(version: &str) -> String {
    format!(
        "#!/bin/sh\nprintf '%s\\n' '{{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":1,\"event\":\"started\"}}' '{{\"protocolVersion\":1,\"command\":\"protocol-version\",\"sequence\":2,\"event\":\"completed\",\"ok\":true,\"data\":{{\"protocolVersion\":1,\"cliVersion\":\"{version}\"}}}}'\n"
    )
}

#[test]
fn npm_child() {
    let Some(instance) = std::env::var_os("MMPU_NPM_FIXTURE") else {
        return;
    };
    let instance = PathBuf::from(instance);
    let release: ToolRelease =
        serde_json::from_slice(&std::fs::read(instance.join("release.json")).unwrap()).unwrap();
    let error = install_release(&instance, &release, InstallChoice::Npm, |_| {}).unwrap_err();
    assert!(format!("{error:#}").contains("cleanup failed"));
}

#[test]
fn interrupted_npm_keeps_a_discoverable_backup_and_cleanup_failure_is_reported() {
    for case in ["interrupt", "cleanup"] {
        let temp = tempfile::TempDir::new().unwrap();
        let instance = temp.path().canonicalize().unwrap();
        let global = instance.join("global/node_modules");
        std::fs::create_dir_all(&global).unwrap();
        let native = global.join("@bro-know-my/packwiz-linux-x64/bkmpw");
        let script = bkmpw_script("0.2.4");
        if case == "interrupt" {
            executable(&native, &script);
            let package = global.join("@bro-know-my/packwiz");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("package.json"), br#"{"version":"0.2.4"}"#).unwrap();
        }
        let bad = instance.join("bad");
        executable(&bad, &bkmpw_script("0.2.5"));
        let bin = instance.join("mock-bin");
        executable(
            &bin.join("npm"),
            "#!/bin/sh\ncase \"$1\" in\nroot) echo \"$FAKE_NPM_ROOT\";;\ninstall) if [ \"$MOCK_CASE\" = interrupt ]; then rm -f \"$FAKE_NATIVE\"; kill -9 \"$PPID\"; exit 1; else mkdir -p \"$(dirname \"$FAKE_NATIVE\")\"; cp \"$BAD_BINARY\" \"$FAKE_NATIVE\"; fi;;\nuninstall) echo cleanup-failed >&2; exit 1;;\n*) echo 10.0.0;;\nesac\n",
        );
        let release = ToolRelease {
            tool: Tool::Bkmpw,
            version: "0.2.5".into(),
            platform: platform().unwrap().into(),
            url: format!(
                "https://github.com/{}/releases/download/v0.2.5/bkmpw-v0.2.5-{}",
                Tool::Bkmpw.repository(),
                platform().unwrap()
            ),
            sha256: "a".repeat(64),
            minimum_updater: None,
        };
        std::fs::write(
            instance.join("release.json"),
            serde_json::to_vec(&release).unwrap(),
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::acquire::npm_tests::npm_child",
                "--nocapture",
            ])
            .env("MMPU_NPM_FIXTURE", &instance)
            .env("MOCK_CASE", case)
            .env("FAKE_NPM_ROOT", &global)
            .env("FAKE_NATIVE", &native)
            .env("BAD_BINARY", &bad)
            .env(
                "PATH",
                std::env::join_paths([bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
                    .unwrap(),
            )
            .output()
            .unwrap();
        if case == "interrupt" {
            assert!(!result.status.success());
            let recovered = state::discover_unlocked(&instance).unwrap().unwrap();
            assert_eq!(recovered.version, "0.2.4");
            assert_eq!(recovered.ownership, Ownership::Local);
            assert!(!native.exists());
        } else {
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
        }
    }
}
