#!/usr/bin/env node
/**
 * sign-manifest.mjs — Builds and Ed25519-signs `release-manifest-<ver>.json`
 * for the daemon auto-updater (see daemon-core/src/autoupdate.rs).
 *
 * Canonical form (MUST match the verifier byte-for-byte):
 *   - JSON object WITHOUT the `signature` field,
 *   - keys sorted recursively (serde_json::to_vec uses BTreeMap order),
 *   - minimal separators, no whitespace (JSON.stringify default),
 *   - ASCII-only values (URLs, hex digests, semver) so UTF-8 encoding is
 *     identical on both sides.
 * Top-level key order is alphabetical: artifacts, publishedAt, version.
 * Artifact entries: sha256, target, url. Artifact map: sorted by name.
 *
 * Only ONE entry per target triple is allowed: the daemon does
 * `artifacts.into_iter().find_map(...)` over a HashMap, so duplicate
 * triples would resolve non-deterministically. The NixOS asset is
 * deliberately excluded (NixOS updates via nix, not auto-update).
 *
 * Usage:
 *   UPDATE_SIGNING_KEY="$(...PEM...)" node scripts/sign-manifest.mjs \
 *     --version 0.1.22 --tag v0.1.22 --assets release_assets \
 *     --repo planobinario/synthhires-bridge \
 *     --out release-manifest-0.1.22.json [--check]
 */
import { createHash, sign, verify, createPrivateKey, createPublicKey } from "node:crypto";
import { readFileSync, writeFileSync, existsSync } from "node:fs";

const [major] = process.versions.node.split(".").map(Number);
if (major < 16) {
  console.error(`Node >=16 required for Ed25519 (have ${process.versions.node})`);
  process.exit(1);
}

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  if (i === -1 || i + 1 >= process.argv.length) {
    console.error(`Missing --${name}`);
    process.exit(1);
  }
  return process.argv[i + 1];
}
const has = (name) => process.argv.includes(`--${name}`);

const VERSION = arg("version");
const TAG = arg("tag");
const ASSETS = arg("assets");
const REPO = arg("repo");
const OUT = arg("out");
const CHECK = has("check");

// target triple -> release asset filename (exactly one row per triple).
// The universal macOS binary serves BOTH triples: it gets one manifest
// entry PER triple (distinct keys, same bytes/url) because the verifier
// matches on `entry.target`. Collapsing both triples into a single key
// silently dropped x86_64 macs from auto-update (second write wins).
const MANIFEST_ASSETS = [
  ["synthhires-bridge-linux-x64", "x86_64-unknown-linux-gnu"],
  ["synthhires-bridge-linux-arm64", "aarch64-unknown-linux-gnu"],
  ["synthhires-bridge-macos", "x86_64-apple-darwin", "synthhires-bridge-macos-x64"],
  ["synthhires-bridge-macos", "aarch64-apple-darwin", "synthhires-bridge-macos-arm64"],
  ["synthhires-bridge-windows-x64-user.exe", "x86_64-pc-windows-msvc"],
];

const artifacts = {};
for (const [asset, target, keyOverride] of MANIFEST_ASSETS) {
  const path = `${ASSETS}/${asset}`;
  if (!existsSync(path)) {
    console.warn(`[sign-manifest] skip ${target}: asset ${asset} not present`);
    continue;
  }
  const bytes = readFileSync(path);
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  const key = keyOverride ?? asset.replace(/\.[^.]+$/, "");
  artifacts[key] = {
    sha256,
    target,
    url: `https://github.com/${REPO}/releases/download/${TAG}/${asset}`,
  };
}

const names = Object.keys(artifacts).sort();
if (names.length === 0) {
  console.error("[sign-manifest] no assets found — refusing to sign an empty manifest");
  process.exit(1);
}
const sortedArtifacts = {};
for (const n of names) sortedArtifacts[n] = artifacts[n];

// Alphabetical top-level order: artifacts, publishedAt, version.
const canonical = {
  artifacts: sortedArtifacts,
  publishedAt: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
  version: VERSION,
};
const canonicalBytes = Buffer.from(JSON.stringify(canonical), "utf8");

const pem = process.env.UPDATE_SIGNING_KEY;
if (!pem) {
  console.error("[sign-manifest] UPDATE_SIGNING_KEY env (PEM) is required");
  process.exit(1);
}
const privateKey = createPrivateKey(pem.trim() + "\n");
const signature = sign(null, canonicalBytes, privateKey).toString("hex");

const signed = { ...canonical, signature };
writeFileSync(OUT, JSON.stringify(signed, null, 2) + "\n");
console.log(`[sign-manifest] wrote ${OUT} (${names.length} artifacts)`);

if (CHECK) {
  const pubHex = process.env.UPDATE_PUBKEY_HEX;
  let publicKey;
  if (pubHex) {
    // Raw 32-byte Ed25519 key -> DER SPKI wrapper for Node:
    // SEQUENCE { SEQUENCE { OID 1.3.101.112 } BIT STRING <key> }.
    const spki = Buffer.concat([
      Buffer.from("302a300506032b6570032100", "hex"),
      Buffer.from(pubHex.trim(), "hex"),
    ]);
    publicKey = createPublicKey({ key: spki, format: "der", type: "spki" });
  } else {
    publicKey = createPublicKey(privateKey);
  }
  const ok = verify(null, canonicalBytes, publicKey, Buffer.from(signature, "hex"));
  if (!ok) {
    console.error("[sign-manifest] self-check FAILED");
    process.exit(1);
  }
  console.log("[sign-manifest] self-check ok");
}
