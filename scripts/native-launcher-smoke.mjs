import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";

// Run on every native CI host with its real release executable.
const binary = path.resolve(process.argv[2]);
const version = fs.readFileSync("Cargo.toml", "utf8").match(/^version\s*=\s*"([^"]+)"/m)[1];
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "mmpu-native-smoke-"));
try {
  const native = path.join(temporary, "node_modules", "@jsi-team", `minecraft-modpack-updater-${process.platform}-${process.arch}`);
  fs.mkdirSync(native, { recursive: true });
  fs.writeFileSync(path.join(native, "package.json"), JSON.stringify({ name: `@jsi-team/minecraft-modpack-updater-${process.platform}-${process.arch}` }));
  const executable = path.join(native, `minecraft-modpack-updater${process.platform === "win32" ? ".exe" : ""}`);
  fs.copyFileSync(binary, executable);
  fs.chmodSync(executable, 0o755);
  const launcher = path.join(temporary, "launcher.js");
  fs.copyFileSync("packages/npm/bin/minecraft-modpack-updater.js", launcher);
  const result = spawnSync(process.execPath, [launcher, "--version"], { encoding: "utf8", timeout: 15000 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), `minecraft-modpack-updater ${version}`);
  console.log(`Native npm launcher verified: ${process.platform} ${process.arch}`);
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}
