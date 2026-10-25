// Every translation has the same messages as English, with the same $n substitutions.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const DIR = path.join(__dirname, "..", "_locales");
const read = (lang) => JSON.parse(fs.readFileSync(path.join(DIR, lang, "messages.json"), "utf8"));
const subs = (text) => (text.match(/\$\d/g) ?? []).sort().join(",");

const english = read("en");

test("there are translations besides English", () => {
  assert.ok(fs.readdirSync(DIR).length >= 12);
});

for (const lang of fs.readdirSync(DIR)) {
  test(`${lang} matches English`, () => {
    const messages = read(lang);
    assert.deepEqual(Object.keys(messages).sort(), Object.keys(english).sort());
    for (const [name, { message }] of Object.entries(messages)) {
      assert.ok(message.trim().length > 0, `${name} is empty`);
      assert.equal(subs(message), subs(english[name].message), `${name}: substitutions differ`);
    }
  });
}

test("every message used in the code exists", () => {
  const sources = ["background.js", "popup.js", "popup.html", "manifest.json"]
    .map((f) => fs.readFileSync(path.join(__dirname, "..", f), "utf8"))
    .join("\n");
  const used = new Set([
    ...[...sources.matchAll(/msg\("(\w+)"/g)].map((m) => m[1]),
    ...[...sources.matchAll(/data-i18n="(\w+)"/g)].map((m) => m[1]),
    ...[...sources.matchAll(/__MSG_(\w+)__/g)].map((m) => m[1]),
  ]);
  assert.ok(used.size >= 15, `only ${used.size} found`);
  for (const name of used) assert.ok(name in english, `missing message ${name}`);
});
