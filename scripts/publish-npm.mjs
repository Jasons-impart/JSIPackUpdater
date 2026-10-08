import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { spawnSync } from "node:child_process";

const [directory, version] = process.argv.slice(2);
if (!directory || !/^\d+\.\d+\.\d+$/.test(version ?? "")) {
  throw new Error("usage: node scripts/publish-npm.mjs PACKAGE_DIR VERSION");
}
const suffixes = ["-linux-x64", "-linux-arm64", "-win32-x64", "-win32-arm64", "-darwin-x64", "-darwin-arm64", ""];
const packages = suffixes.map(suffix => {
  const name = `@jsi-team/minecraft-modpack-updater${suffix}`;
  const archive = path.join(directory, `jsi-team-minecraft-modpack-updater${suffix}-${version}.tgz`);
  const digest = crypto.createHash("sha512").update(fs.readFileSync(archive)).digest("base64");
  return { name, archive, integrity: `sha512-${digest}` };
});
for (const { name, archive, integrity } of packages) {
  const spec = `${name}@${version}`;
  const queried = spawnSync("npm", ["view", spec, "dist.integrity", "--json"], { encoding: "utf8" });
  if (queried.error) throw queried.error;
  let metadata;
  try { metadata = JSON.parse(queried.stdout); }
  catch { throw new Error(`Cannot query registry for ${spec}: ${queried.stderr}`); }
  if (queried.status === 0) {
    if (metadata !== integrity) throw new Error(`Published ${spec} differs from this artifact; refusing to skip it`);
    console.log(`Verified existing publication: ${spec}`);
    continue;
  }
  if (metadata?.error?.code !== "E404") {
    throw new Error(`Registry query failed for ${spec}: ${queried.stderr || queried.stdout}`);
  }
  const published = spawnSync("npm", ["publish", archive, "--access", "public"], { stdio: "inherit" });
  if (published.error) throw published.error;
  if (published.status !== 0) throw new Error(`Publishing ${spec} failed; the job can be retried`);
}
