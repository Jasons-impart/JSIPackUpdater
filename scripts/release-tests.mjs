import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import vm from "node:vm";
import { spawn, spawnSync } from "node:child_process";

const version = fs.readFileSync("Cargo.toml", "utf8").match(/^version\s*=\s*"([^"]+)"/m)[1];
const targets = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc", "x86_64-apple-darwin", "aarch64-apple-darwin"];
async function fixture(run) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "mmpu-npm-test-"));
  try {
    const assets = path.join(directory, "assets");
    fs.mkdirSync(assets);
    for (const target of targets) {
      const name = `minecraft-modpack-updater-v${version}-${target}${target.includes("windows") ? ".exe" : ""}`;
      const bytes = Buffer.from("#!/bin/sh\nif [ \"$1\" = --terminate ]; then kill -TERM $$; fi\nif [ \"$1\" = --wait ]; then echo $$ > \"$2\"; exec sleep 30; fi\nprintf '%s\\n' \"$MMPU_MANAGED_BY_NPM\" \"$@\"\nexit 7\n");
      fs.writeFileSync(path.join(assets, name), bytes);
      fs.writeFileSync(path.join(assets, `${name}.sha256`), crypto.createHash("sha256").update(bytes).digest("hex") + "  " + name + "\n");
    }
    await run(directory, assets);
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
}
function prepare(directory, assets, requestedVersion = version) {
  return spawnSync(process.execPath, ["scripts/prepare-npm.mjs", assets, path.join(directory, "packages"), requestedVersion], { encoding: "utf8" });
}
test("pack seven packages and execute the platform wrapper with args and npm provenance", () => fixture((directory, assets) => {
  const result = prepare(directory, assets);
  assert.equal(result.status, 0, result.stderr);
  const output = path.join(directory, "packages");
  assert.equal(fs.readdirSync(output).filter(name => name.endsWith(".tgz")).length, 7);
  const metadata = JSON.parse(fs.readFileSync(path.join(output, "main/package.json")));
  assert.equal(metadata.name, "@jsi-team/minecraft-modpack-updater");
  assert.equal(Object.keys(metadata.optionalDependencies).length, 6);
  for (const [suffix, platform, arch] of [["linux-arm64", "linux", "arm64"], ["win32-arm64", "win32", "arm64"]]) {
    const native = JSON.parse(fs.readFileSync(path.join(output, suffix, "package.json")));
    assert.deepEqual(native.os, [platform]);
    assert.deepEqual(native.cpu, [arch]);
    assert.equal(metadata.optionalDependencies[native.name], version);
  }
  if (process.platform !== "linux" || !["x64", "arm64"].includes(process.arch)) return;
  const hostSuffix = `linux-${process.arch}`;
  const consumer = path.join(directory, "consumer");
  fs.mkdirSync(consumer);
  fs.writeFileSync(path.join(consumer, "package.json"), JSON.stringify({ private: true }));
  const install = spawnSync("npm", ["install", "--offline", "--omit=optional", "--ignore-scripts", "--no-audit", "--no-fund",
    path.join(output, `jsi-team-minecraft-modpack-updater-${version}.tgz`),
    path.join(output, `jsi-team-minecraft-modpack-updater-${hostSuffix}-${version}.tgz`)], { cwd: consumer, encoding: "utf8" });
  assert.equal(install.status, 0, install.stderr);
  const installedBin = path.join(consumer, "node_modules/.bin/minecraft-modpack-updater");
  const consumed = spawnSync(installedBin, ["--version", "a path with spaces"], { encoding: "utf8" });
  assert.equal(consumed.status, 7, consumed.stderr);
  assert.equal(consumed.stdout, "1\n--version\na path with spaces\n");
  const installed = path.join(output, `main/node_modules/@jsi-team/minecraft-modpack-updater-${hostSuffix}`);
  fs.mkdirSync(path.dirname(installed), { recursive: true });
  fs.cpSync(path.join(output, hostSuffix), installed, { recursive: true });
  const launch = spawnSync(process.execPath, [path.join(output, "main/bin/minecraft-modpack-updater.js"), "--version", "a path with spaces"], { encoding: "utf8" });
  assert.equal(launch.status, 7);
  assert.equal(launch.stdout, "1\n--version\na path with spaces\n");
  const terminated = spawnSync(process.execPath, [path.join(output, "main/bin/minecraft-modpack-updater.js"), "--terminate"], { encoding: "utf8" });
  assert.equal(terminated.signal, "SIGTERM");
  fs.rmSync(installed, { recursive: true });
  const missing = spawnSync(process.execPath, [path.join(output, "main/bin/minecraft-modpack-updater.js")], { encoding: "utf8" });
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /optional dependencies/);
}));
test("reject changed binaries and a mismatched release version", () => fixture((directory, assets) => {
  const name = fs.readdirSync(assets).find(name => !name.endsWith(".sha256"));
  fs.appendFileSync(path.join(assets, name), "tampered");
  const changed = prepare(directory, assets);
  assert.notEqual(changed.status, 0);
  assert.match(changed.stderr, /SHA-256 mismatch/);
  const mismatch = prepare(directory, assets, "999.0.0");
  assert.notEqual(mismatch.status, 0);
  assert.match(mismatch.stderr, /must match Cargo/);
}));
test("resume partial publication, reject mismatched existing content and registry authentication errors", { skip: process.platform === "win32" }, () => fixture((directory, assets) => {
  const prepared = prepare(directory, assets);
  assert.equal(prepared.status, 0, prepared.stderr);
  const state = path.join(directory, "registry.json");
  fs.writeFileSync(state, JSON.stringify({ entries: {}, published: [], failedOnce: false }));
  const fakeBin = path.join(directory, "fake-bin");
  fs.mkdirSync(fakeBin);
  fs.writeFileSync(path.join(fakeBin, "npm"), `#!/usr/bin/env node
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
const statePath = process.env.MMPU_TEST_REGISTRY;
const state = JSON.parse(fs.readFileSync(statePath));
const [command, arg] = process.argv.slice(2);
if (command === 'view') {
  if (state.authError) { console.log(JSON.stringify({error:{code:'E401'}})); process.exit(1); }
  if (state.entries[arg]) { console.log(JSON.stringify(state.entries[arg])); process.exit(0); }
  console.log(JSON.stringify({error:{code:'E404'}})); process.exit(1);
}
if (!state.failedOnce && arg.includes('win32-x64')) {
  state.failedOnce = true; fs.writeFileSync(statePath, JSON.stringify(state)); process.exit(1);
}
const match = path.basename(arg).match(/^jsi-team-(.+)-(\\d+\\.\\d+\\.\\d+)\\.tgz$/);
const spec = '@jsi-team/' + match[1] + '@' + match[2];
state.entries[spec] = 'sha512-' + crypto.createHash('sha512').update(fs.readFileSync(arg)).digest('base64');
state.published.push(spec); fs.writeFileSync(statePath, JSON.stringify(state));
`, { mode: 0o755 });
  const output = path.join(directory, "packages");
  const publish = () => spawnSync(process.execPath, ["scripts/publish-npm.mjs", output, version], {
    encoding: "utf8", env: { ...process.env, PATH: fakeBin + path.delimiter + process.env.PATH, MMPU_TEST_REGISTRY: state }
  });
  assert.notEqual(publish().status, 0);
  const retry = publish();
  assert.equal(retry.status, 0, retry.stderr);
  const completed = JSON.parse(fs.readFileSync(state));
  assert.equal(completed.published.length, 7);
  assert.equal(new Set(completed.published).size, 7);
  assert.equal(completed.published.at(-1), `@jsi-team/minecraft-modpack-updater@${version}`);
  completed.authError = true;
  fs.writeFileSync(state, JSON.stringify(completed));
  const auth = publish();
  assert.notEqual(auth.status, 0);
  assert.match(auth.stderr, /Registry query failed/);
  completed.authError = false;
  fs.writeFileSync(state, JSON.stringify(completed));
  fs.appendFileSync(path.join(output, `jsi-team-minecraft-modpack-updater-linux-x64-${version}.tgz`), "changed");
  const changed = publish();
  assert.notEqual(changed.status, 0);
  assert.match(changed.stderr, /differs from this artifact/);
}));
test("a signal addressed to the launcher also stops its native child", { skip: process.platform !== "linux" || !["x64", "arm64"].includes(process.arch) }, () => fixture(async (directory, assets) => {
  const prepared = prepare(directory, assets);
  assert.equal(prepared.status, 0, prepared.stderr);
  const output = path.join(directory, "packages");
  const hostSuffix = `linux-${process.arch}`;
  const installed = path.join(output, `main/node_modules/@jsi-team/minecraft-modpack-updater-${hostSuffix}`);
  fs.mkdirSync(path.dirname(installed), { recursive: true });
  fs.cpSync(path.join(output, hostSuffix), installed, { recursive: true });
  const pidFile = path.join(directory, "native.pid");
  const launcher = spawn(process.execPath, [path.join(output, "main/bin/minecraft-modpack-updater.js"), "--wait", pidFile], { stdio: "ignore" });
  let nativePid, timer;
  const closed = new Promise((resolve, reject) => {
    launcher.once("error", reject);
    launcher.once("close", (code, signal) => resolve({ code, signal }));
    timer = setTimeout(() => reject(new Error("launcher did not stop")), 5000);
  });
  try {
    const deadline = Date.now() + 3000;
    while (!fs.existsSync(pidFile) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
    assert.ok(fs.existsSync(pidFile), "native child must start");
    nativePid = Number(fs.readFileSync(pidFile, "utf8").trim());
    launcher.kill("SIGTERM");
    assert.equal((await closed).signal, "SIGTERM");
    assert.throws(() => process.kill(nativePid, 0), { code: "ESRCH" });
  } finally {
    clearTimeout(timer);
    launcher.kill("SIGKILL");
    if (nativePid) { try { process.kill(nativePid, "SIGKILL"); } catch { /* already reaped */ } }
  }
}));

test("ARM64 launchers resolve the native package and preserve executable suffix and args", () => {
  const source = fs.readFileSync("packages/npm/bin/minecraft-modpack-updater.js", "utf8");
  for (const platform of ["linux", "win32"]) {
    const packageName = `@jsi-team/minecraft-modpack-updater-${platform}-arm64`;
    let spawned = false;
    const mockRequire = name => {
      if (name === "node:path") return path;
      assert.equal(name, "node:child_process");
      return { spawn(binary, args, options) {
        spawned = true;
        assert.equal(binary, path.join("native", `minecraft-modpack-updater${platform === "win32" ? ".exe" : ""}`));
        assert.deepEqual(Array.from(args), ["gui", "a path with spaces"]);
        assert.equal(options.env.MMPU_MANAGED_BY_NPM, "1");
        return { once() {}, kill() {} };
      } };
    };
    mockRequire.resolve = name => {
      assert.equal(name, `${packageName}/package.json`);
      return path.join("native", "package.json");
    };
    vm.runInNewContext(source, { require: mockRequire, console, process: {
      platform, arch: "arm64", argv: ["node", "launcher", "gui", "a path with spaces"], env: {},
      on() {}, exit() { assert.fail("ARM64 launcher must resolve successfully"); }
    } });
    assert.ok(spawned);
  }
});
