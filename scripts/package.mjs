#!/usr/bin/env node
// Packages the plugin into dist/<name>-<version>-darwin.zip, the asset name
// Lithe Store's release tracker recognizes (docs/publishing.md there).
//
// The package carries one backend, bin/mise-provider, built from backend/ for
// Apple Silicon and Intel and joined with lipo into a universal binary, so one
// archive serves both Macs and the manifest path never varies by ${arch}.
// Building uses only crates already in Cargo.lock; nothing is downloaded at
// install or run time.
//
// Usage: node scripts/package.mjs [--expect-tag <tag>] [--skip-build]
//   --skip-build packages backend/target/universal/mise-provider as it is.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const PACKAGE_DIR = "io.github.fxrayhughes.mise";
const ASSET_PREFIX = "mise";
const PLATFORM = "darwin";
const BACKEND_PATH = "bin/mise-provider";
const TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
const EXPECTED_REQUIRES = { backend: 2, serverProcess: 1, settings: 2, toolchainProviders: 1 };

const manifest = JSON.parse(readFileSync(`${PACKAGE_DIR}/plugin.json`, "utf8"));
const cargo = readFileSync("backend/Cargo.toml", "utf8").match(/^version = "([^"]+)"/m)?.[1];
const problems = [];
const sorted = (value) => JSON.stringify(Object.fromEntries(Object.entries(value ?? {}).sort()));
if (manifest.id !== PACKAGE_DIR) problems.push(`plugin.json id ${manifest.id} must equal the directory ${PACKAGE_DIR}`);
if (!/^[0-9]+\.[0-9]+\.[0-9]+$/.test(manifest.version)) problems.push(`version ${manifest.version} is not MAJOR.MINOR.PATCH`);
if (cargo !== manifest.version) problems.push(`backend/Cargo.toml version ${cargo} must equal plugin.json version ${manifest.version}`);
if (manifest.vendor?.signatureRequirement !== "none") problems.push('vendor.signatureRequirement must be "none"');
if (manifest.id === "dev.lithe" || manifest.id.startsWith("dev.lithe.")) problems.push("dev.lithe.* is reserved for official plugins");
if (sorted(manifest.requires) !== sorted(EXPECTED_REQUIRES)) problems.push(`requires must be exactly ${sorted(EXPECTED_REQUIRES)}`);
if (manifest.backend?.launch?.kind !== "packaged" || manifest.backend.launch.path !== BACKEND_PATH) {
  problems.push(`backend.launch must be the packaged ${BACKEND_PATH}`);
}

const tagFlag = process.argv.indexOf("--expect-tag");
if (tagFlag >= 0) {
  const tag = process.argv[tagFlag + 1];
  if (tag !== `v${manifest.version}`) problems.push(`tag ${tag} does not match plugin.json version ${manifest.version} (expected v${manifest.version})`);
}
if (problems.length > 0) {
  for (const problem of problems) console.error(`✗ ${problem}`);
  process.exit(1);
}

const universal = resolve("backend/target/universal/mise-provider");
if (!process.argv.includes("--skip-build")) {
  const built = TARGETS.map((target) => {
    execFileSync("cargo", ["build", "--release", "--locked", "--target", target], { cwd: "backend", stdio: "inherit" });
    return resolve(`backend/target/${target}/release/mise-provider`);
  });
  mkdirSync(resolve("backend/target/universal"), { recursive: true });
  execFileSync("lipo", ["-create", "-output", universal, ...built]);
}
if (!existsSync(universal)) {
  console.error(`✗ ${universal} is missing; build without --skip-build first`);
  process.exit(1);
}
const architectures = execFileSync("lipo", ["-archs", universal], { encoding: "utf8" }).trim().split(/\s+/).sort();
if (JSON.stringify(architectures) !== JSON.stringify(["arm64", "x86_64"])) {
  console.error(`✗ ${universal} must contain arm64 and x86_64, found ${architectures.join(" ")}`);
  process.exit(1);
}

// Stage a copy so the source tree never holds the binary.
const staging = mkdtempSync(join(tmpdir(), "mise-package-"));
try {
  const root = join(staging, PACKAGE_DIR);
  cpSync(PACKAGE_DIR, root, { recursive: true, filter: (path) => !path.endsWith(".DS_Store") });
  mkdirSync(join(root, "bin"), { recursive: true });
  copyFileSync(universal, join(root, BACKEND_PATH));
  chmodSync(join(root, BACKEND_PATH), 0o755);
  // An unsigned universal binary gets an ad-hoc signature, which arm64 macOS needs to run it.
  execFileSync("codesign", ["--force", "--sign", "-", join(root, BACKEND_PATH)]);

  rmSync("dist", { recursive: true, force: true });
  mkdirSync("dist", { recursive: true });
  const asset = resolve("dist", `${ASSET_PREFIX}-${manifest.version}-${PLATFORM}.zip`);
  // -X drops extra file attributes; -y stores a symbolic link as a link so
  // Lithe refuses it instead of the archive silently carrying its target.
  execFileSync("zip", ["-qrXy", asset, PACKAGE_DIR], { cwd: staging });
  const sha256 = createHash("sha256").update(readFileSync(asset)).digest("hex");
  console.log(`dist/${ASSET_PREFIX}-${manifest.version}-${PLATFORM}.zip ${sha256}`);
} finally {
  rmSync(staging, { recursive: true, force: true });
}
