"""Resume drafts; verify public assets without replacing immutable releases."""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import tempfile

def gh(*args, capture=False):
    return subprocess.run(["gh", *map(str, args)], check=True, text=True,
                          capture_output=capture).stdout

def publish(args):
    if not re.fullmatch(r"v\d+\.\d+\.\d+", args.tag):
        raise ValueError("stable updater tag required")
    files = sorted(p for p in args.assets.iterdir() if p.is_file())
    if not files:
        raise ValueError("no release artifacts")
    try:
        release = json.loads(gh("api", f"repos/{args.repo}/releases/tags/{args.tag}", capture=True))
    except subprocess.CalledProcessError as error:
        if "HTTP 404" not in (error.stderr or ""):
            raise
        gh("release", "create", args.tag, "--repo", args.repo, "--draft", "--verify-tag", "--generate-notes")
        release = {"draft": True}
    if release["draft"]:
        gh("release", "upload", args.tag, *files, "--repo", args.repo, "--clobber")
        gh("release", "edit", args.tag, "--repo", args.repo, "--draft=false")
        return
    names = {asset["name"] for asset in release["assets"]}
    if any(file.name not in names for file in files):
        raise ValueError("public release is incomplete; refusing to modify it")
    with tempfile.TemporaryDirectory(prefix="mmpu-public-release-") as directory:
        for file in files:
            gh("release", "download", args.tag, "--repo", args.repo, "--pattern", file.name, "--dir", directory)
            with file.open("rb") as intended, (pathlib.Path(directory) / file.name).open("rb") as existing:
                if hashlib.file_digest(intended, "sha256").digest() != hashlib.file_digest(existing, "sha256").digest():
                    raise ValueError(f"public artifact differs: {file.name}; use a new version")
    print("Public release verified; all immutable artifacts match")

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--assets", type=pathlib.Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repo", required=True)
    publish(parser.parse_args())
