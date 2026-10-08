#!/usr/bin/env python3
"""Verify tested native assets and produce the updater's tool catalog."""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import tomllib

TARGETS = (
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
)
REPOSITORIES = {"bkmpw": "bro-know-my-org/BroKnowMyPackwiz", "updater": "Jasons-impart/JSIPackUpdater"}

def version(value):
    value = value.removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError("expected a stable three-part version")
    return value

def tested_updater_version():
    return version(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["package"]["version"])

def validate_release(args):
    metadata = json.loads(args.metadata.read_text())
    if (args.tag != "v" + version(args.tag) or metadata.get("tagName") != args.tag
            or metadata.get("isDraft") is not False or metadata.get("isPrerelease") is not False):
        raise ValueError("official published stable release required")

def entry(tool, tag, target, digest, minimum_updater=None):
    tag = version(tag)
    if target not in TARGETS or not re.fullmatch(r"[a-fA-F0-9]{64}", digest):
        raise ValueError("invalid platform or SHA-256")
    name = "bkmpw" if tool == "bkmpw" else "minecraft-modpack-updater"
    if tool == "bkmpw" and minimum_updater is None:
        raise ValueError("bkmpw entries must declare the tested updater version")
    asset = f'{name}-v{tag}-{target}' + (".exe" if "windows" in target else "")
    return dict(tool=tool, version=tag, platform=target, sha256=digest.lower(),
                url=f'https://github.com/{REPOSITORIES[tool]}/releases/download/v{tag}/{asset}', minimumUpdater=version(minimum_updater or "0.1.0"))

def verify_bkmpw(args):
    digest = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    if digest != args.sha256.lower():
        raise ValueError("bkmpw SHA-256 mismatch")
    output = subprocess.check_output([str(args.binary.resolve()), "--jsonl", "protocol-version"], timeout=15)
    events = [json.loads(line) for line in output.splitlines() if line]
    completed = [e for e in events if e.get("event") == "completed" and e.get("ok") is True]
    if len(completed) != 1 or completed[0].get("data", {}).get("protocolVersion") != 1:
        raise ValueError("bkmpw protocol is incompatible")
    if version(completed[0]["data"]["cliVersion"]) != version(args.version):
        raise ValueError("bkmpw version mismatch")
    args.output.write_text(json.dumps(entry("bkmpw", args.version, args.target, digest, tested_updater_version()), indent=2) + "\n")

def catalog(args):
    releases = []
    for target in TARGETS:
        name = f'minecraft-modpack-updater-v{version(args.version)}-{target}' + (".exe" if "windows" in target else "")
        binary = args.assets / name
        digest = hashlib.sha256(binary.read_bytes()).hexdigest()
        if (args.assets / (name + ".sha256")).read_text().split()[0].lower() != digest:
            raise ValueError(f"updater SHA-256 mismatch: {target}")
        releases.append(entry("updater", args.version, target, digest))
        tested = json.loads((args.assets / f"bkmpw-{target}.json").read_text())
        expected = entry("bkmpw", tested["version"], target, tested["sha256"], args.version)
        if tested != expected:
            raise ValueError(f"invalid tested bkmpw entry: {target}")
        releases.append(expected)
    if len({r["version"] for r in releases if r["tool"] == "bkmpw"}) != 1:
        raise ValueError("all platforms must use the same tested bkmpw version")
    args.output.write_text(json.dumps(dict(schemaVersion=1, releases=releases), indent=2) + "\n")

def refresh(args):
    original = json.loads(args.existing.read_text())
    if original.get("schemaVersion") != 1:
        raise ValueError("unsupported catalog schema")
    releases = []
    updater_versions = set()
    bkmpw_versions = set()
    for target in TARGETS:
        installed = [r for r in original["releases"] if r.get("tool") == "updater" and r.get("platform") == target]
        if len(installed) != 1:
            raise ValueError(f"expected one updater entry: {target}")
        item = installed[0]
        if item != entry("updater", item["version"], target, item["sha256"]):
            raise ValueError(f"invalid updater entry: {target}")
        updater_versions.add(item["version"])
        tested = json.loads((args.assets / f"bkmpw-{target}.json").read_text())
        if tested != entry("bkmpw", tested["version"], target, tested["sha256"], item["version"]):
            raise ValueError(f"invalid tested bkmpw entry: {target}")
        bkmpw_versions.add(tested["version"])
        releases.extend([item, tested])
    if len(updater_versions) != 1 or len(bkmpw_versions) != 1:
        raise ValueError("all platforms must use the same tested version")
    args.output.write_text(json.dumps(dict(schemaVersion=1, releases=releases), indent=2) + "\n")

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    release = commands.add_parser("validate-release")
    release.add_argument("--tag", required=True)
    release.add_argument("--metadata", type=pathlib.Path, required=True)
    verify = commands.add_parser("verify-bkmpw")
    verify.add_argument("--binary", type=pathlib.Path, required=True)
    verify.add_argument("--sha256", required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--target", required=True)
    verify.add_argument("--output", type=pathlib.Path, required=True)
    generate = commands.add_parser("catalog")
    generate.add_argument("--assets", type=pathlib.Path, required=True)
    generate.add_argument("--version", required=True)
    generate.add_argument("--output", type=pathlib.Path, required=True)
    update = commands.add_parser("refresh")
    update.add_argument("--assets", type=pathlib.Path, required=True)
    update.add_argument("--existing", type=pathlib.Path, required=True)
    update.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    {"validate-release": validate_release, "verify-bkmpw": verify_bkmpw, "catalog": catalog, "refresh": refresh}[args.command](args)
