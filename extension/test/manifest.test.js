const { test } = require("node:test");
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const EXT_DIR = path.join(__dirname, "..");
const manifestPath = path.join(EXT_DIR, "manifest.json");
const EXPECTED_CHROME_ID = "gepekebdmobfjhpghgflnhgbbmjcmhon";

function readManifest() {
  return JSON.parse(fs.readFileSync(manifestPath, "utf8"));
}

// Chrome's extension id: sha256 of the DER public key, first 32 hex chars, 0-f mapped to a-p.
function chromeIdFromKey(keyBase64) {
  const digest = crypto.createHash("sha256").update(Buffer.from(keyBase64, "base64")).digest("hex");
  return [...digest.slice(0, 32)]
    .map((c) => String.fromCharCode("a".charCodeAt(0) + parseInt(c, 16)))
    .join("");
}

function assertFileExists(relPath) {
  const full = path.join(EXT_DIR, relPath);
  assert.ok(fs.existsSync(full), `missing file: ${relPath}`);
}

test("manifest.json parses as JSON", () => {
  assert.doesNotThrow(readManifest);
});

test("manifest is MV3 with the expected name and version", () => {
  const m = readManifest();
  assert.equal(m.manifest_version, 3);
  assert.equal(m.name, "__MSG_extName__");
  assert.equal(m.default_locale, "en");
  assert.equal(m.version, "0.1.0");
  assert.ok(m.description && m.description.length > 0);
});

test("manifest has a public key", () => {
  const m = readManifest();
  assert.equal(typeof m.key, "string");
  assert.match(m.key, /^MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA/);
  // Must be a single base64 line.
  assert.match(m.key, /^[A-Za-z0-9+/=]+$/);
});

test("key derives the fixed Chrome extension id", () => {
  const { key } = readManifest();
  assert.equal(chromeIdFromKey(key), EXPECTED_CHROME_ID);
});

test("manifest lists the required permissions", () => {
  const { permissions } = readManifest();
  for (const p of ["downloads", "nativeMessaging", "contextMenus", "cookies", "storage", "webRequest"]) {
    assert.ok(permissions.includes(p), `missing permission ${p}`);
  }
  assert.deepEqual([...permissions].sort(), [
    "contextMenus",
    "cookies",
    "downloads",
    "nativeMessaging",
    "storage",
    "webRequest",
  ]);
});

test("manifest has <all_urls> host permission", () => {
  assert.deepEqual(readManifest().host_permissions, ["<all_urls>"]);
});

test("background has both a service_worker and scripts (Chrome and Firefox)", () => {
  const { background } = readManifest();
  assert.equal(background.service_worker, "background.js");
  assert.deepEqual(background.scripts, ["decide.js", "background.js"]);
});

test("every file referenced by the background exists", () => {
  const { background } = readManifest();
  assertFileExists(background.service_worker);
  for (const script of background.scripts) assertFileExists(script);
});

test("action popup and its title are set, and the popup file exists", () => {
  const { action } = readManifest();
  assert.equal(action.default_popup, "popup.html");
  assert.equal(action.default_title, "__MSG_extName__");
  assertFileExists(action.default_popup);
});

test("popup page exists and loads only local scripts and styles", () => {
  const html = fs.readFileSync(path.join(EXT_DIR, "popup.html"), "utf8");
  // MV3 CSP forbids inline scripts, so every script tag must have a src.
  for (const tag of html.match(/<script\b[^>]*>/gi) ?? []) {
    assert.match(tag, /\bsrc="/, `inline script tag: ${tag}`);
  }
  for (const [, src] of html.matchAll(/<(?:script|link|img)\b[^>]*?\b(?:src|href)="([^"]+)"/gi)) {
    if (!/^https?:/.test(src)) assertFileExists(src);
  }
});

test("all declared icons exist", () => {
  const { icons } = readManifest();
  assert.deepEqual(Object.keys(icons).sort(), ["128", "16", "48"]);
  for (const file of Object.values(icons)) assertFileExists(file);
});

test("icons are PNG files", () => {
  const { icons } = readManifest();
  for (const file of Object.values(icons)) {
    const head = fs.readFileSync(path.join(EXT_DIR, file)).subarray(0, 8);
    assert.deepEqual([...head], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], file);
  }
});

test("gecko id and minimum version are set", () => {
  const { browser_specific_settings: bss } = readManifest();
  assert.equal(bss.gecko.id, "vdm@vdm.local");
  assert.equal(bss.gecko.strict_min_version, "115.0");
});
