#!/usr/bin/env node
"use strict";
const { spawn } = require("node:child_process");
const path = require("node:path");
const platforms = {
  "linux x64": "linux-x64",
  "win32 x64": "win32-x64",
  "darwin x64": "darwin-x64",
  "darwin arm64": "darwin-arm64"
};
try {
  const platform = platforms[`${process.platform} ${process.arch}`];
  if (!platform) throw new Error(`Unsupported platform: ${process.platform} ${process.arch}`);
  const packageName = `@jsi-team/minecraft-modpack-updater-${platform}`;
  let metadata;
  try { metadata = require.resolve(`${packageName}/package.json`); }
  catch { throw new Error(`Missing ${packageName}; reinstall with optional dependencies enabled.`); }
  const binary = path.join(path.dirname(metadata), `minecraft-modpack-updater${process.platform === "win32" ? ".exe" : ""}`);
  const child = spawn(binary, process.argv.slice(2), {
    stdio: "inherit", env: { ...process.env, MMPU_MANAGED_BY_NPM: "1" }
  });
  const handlers = new Map(["SIGINT", "SIGTERM"].map(signal => [signal, () => child.kill(signal)]));
  for (const [signal, handler] of handlers) process.on(signal, handler);
  let failed = false;
  child.once("error", error => {
    failed = true;
    console.error(`minecraft-modpack-updater: ${error.message}`);
    process.exitCode = 1;
  });
  child.once("close", (status, signal) => {
    for (const [name, handler] of handlers) process.removeListener(name, handler);
    if (failed) return;
    if (signal) {
      const code = 128 + (require("node:os").constants.signals[signal] ?? 1);
      process.exitCode = code;
      if (process.platform !== "win32") process.kill(process.pid, signal);
    } else { process.exitCode = status ?? 1; }
  });
} catch (error) {
  console.error(`minecraft-modpack-updater: ${error.message}`);
  process.exit(1);
}
