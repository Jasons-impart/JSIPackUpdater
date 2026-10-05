import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { execFileSync } from "node:child_process";

const [assets, destination, version] = process.argv.slice(2);
if (!assets || !destination || !/^\d+\.\d+\.\d+$/.test(version ?? "")) {
  throw new Error("usage: node scripts/prepare-npm.mjs ASSET_DIR OUTPUT_DIR VERSION");
}
const cargoVersion = fs.readFileSync("Cargo.toml", "utf8").match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (cargoVersion !== version) throw new Error("npm version must match Cargo.toml");
const output = path.resolve(destination);
fs.mkdirSync(output, { recursive: true });
const platforms = [
  ["linux-x64", "linux", "x64", "x86_64-unknown-linux-gnu", ""],
  ["win32-x64", "win32", "x64", "x86_64-pc-windows-msvc", ".exe"],
  ["darwin-x64", "darwin", "x64", "x86_64-apple-darwin", ""],
  ["darwin-arm64", "darwin", "arm64", "aarch64-apple-darwin", ""]
];
const wrapper = JSON.parse(fs.readFileSync("packages/npm/package.json", "utf8"));
wrapper.version = version;
wrapper.optionalDependencies = {};
for (const [suffix, os, cpu, target, extension] of platforms) {
  const name = `@jsi-team/minecraft-modpack-updater-${suffix}`;
  const directory = path.join(output, suffix);
  fs.mkdirSync(directory, { recursive: true });
  const asset = path.join(assets, `minecraft-modpack-updater-v${version}-${target}${extension}`);
  const bytes = fs.readFileSync(asset);
  const expected = fs.readFileSync(`${asset}.sha256`, "utf8").trim().split(/\s+/)[0].toLowerCase();
  if (!/^[a-f0-9]{64}$/.test(expected) || crypto.createHash("sha256").update(bytes).digest("hex") !== expected) {
    throw new Error(`SHA-256 mismatch: ${asset}`);
  }
  const executable = `minecraft-modpack-updater${extension}`;
  fs.writeFileSync(path.join(directory, executable), bytes, { mode: 0o755 });
  for (const license of ["DroidSansFallbackFull.COPYRIGHT", "Apache-2.0.txt"]) {
    fs.copyFileSync(path.join("assets", license), path.join(directory, license));
  }
  fs.writeFileSync(path.join(directory, "package.json"), JSON.stringify({ name, version, os: [os], cpu: [cpu], files: [executable, "DroidSansFallbackFull.COPYRIGHT", "Apache-2.0.txt"], repository: wrapper.repository }, null, 2) + "\n");
  wrapper.optionalDependencies[name] = version;
  execFileSync("npm", ["pack", directory, "--pack-destination", output], { stdio: "inherit" });
}
const main = path.join(output, "main");
fs.mkdirSync(path.join(main, "bin"), { recursive: true });
fs.copyFileSync("packages/npm/bin/minecraft-modpack-updater.js", path.join(main, "bin/minecraft-modpack-updater.js"));
fs.copyFileSync("packages/npm/README.md", path.join(main, "README.md"));
fs.writeFileSync(path.join(main, "package.json"), JSON.stringify(wrapper, null, 2) + "\n");
execFileSync("npm", ["pack", main, "--pack-destination", output], { stdio: "inherit" });
