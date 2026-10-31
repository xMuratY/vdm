const { test } = require("node:test");
const assert = require("node:assert/strict");
const VDM = require("../decide.js");

const OWN_ID = "vdm-extension-own-id";

// A download in progress from a normal https host, used as the base for each case.
function download(overrides = {}) {
  return {
    id: 1,
    url: "https://files.example.com/setup.exe",
    finalUrl: "https://files.example.com/setup.exe",
    mime: "application/octet-stream",
    fileSize: 5 * 1024 * 1024,
    byExtensionId: undefined,
    state: "in_progress",
    ...overrides,
  };
}

test("shouldCapture: a normal file download is captured", () => {
  assert.equal(VDM.shouldCapture(download(), VDM.DEFAULT_SETTINGS, OWN_ID), true);
});

test("shouldCapture: capture switched off is never captured", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, capture: false };
  assert.equal(VDM.shouldCapture(download(), settings, OWN_ID), false);
});

test("shouldCapture: non-http URLs are left to the browser", () => {
  for (const url of [
    "blob:https://example.com/1234",
    "data:application/zip;base64,AAAA",
    "file:///C:/Users/me/a.zip",
    "ftp://example.com/a.zip",
  ]) {
    assert.equal(
      VDM.shouldCapture(download({ url, finalUrl: url }), VDM.DEFAULT_SETTINGS, OWN_ID),
      false,
      url,
    );
  }
});

test("shouldCapture: finalUrl is checked, so a redirect to a non-http URL is rejected", () => {
  const item = download({ url: "https://a.example.com/x", finalUrl: "file:///etc/passwd" });
  assert.equal(VDM.shouldCapture(item, VDM.DEFAULT_SETTINGS, OWN_ID), false);
});

test("shouldCapture: http URL is accepted", () => {
  const item = download({ url: "http://files.example.com/a.zip", finalUrl: "" });
  assert.equal(VDM.shouldCapture(item, VDM.DEFAULT_SETTINGS, OWN_ID), true);
});

test("shouldCapture: downloads started by our own extension are not captured", () => {
  const item = download({ byExtensionId: OWN_ID });
  assert.equal(VDM.shouldCapture(item, VDM.DEFAULT_SETTINGS, OWN_ID), false);
});

test("shouldCapture: downloads started by another extension are captured", () => {
  const item = download({ byExtensionId: "someone-else" });
  assert.equal(VDM.shouldCapture(item, VDM.DEFAULT_SETTINGS, OWN_ID), true);
});

test("shouldCapture: a missing byExtensionId never matches our own id", () => {
  const item = download({ byExtensionId: undefined });
  assert.equal(VDM.shouldCapture(item, VDM.DEFAULT_SETTINGS, undefined), true);
});

test("shouldCapture: known size below minSizeKB is ignored", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, minSizeKB: 100 };
  const item = download({ fileSize: 99 * 1024 });
  assert.equal(VDM.shouldCapture(item, settings, OWN_ID), false);
});

test("shouldCapture: size exactly at minSizeKB is captured", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, minSizeKB: 1 };
  assert.equal(VDM.shouldCapture(download({ fileSize: 1024 }), settings, OWN_ID), true);
});

test("shouldCapture: unknown sizes (-1, 0, undefined) pass the size filter", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, minSizeKB: 500 };
  for (const fileSize of [-1, 0, undefined]) {
    assert.equal(VDM.shouldCapture(download({ fileSize }), settings, OWN_ID), true, String(fileSize));
  }
});

test("shouldCapture: text/html and application/xhtml+xml pages are not captured", () => {
  for (const mime of ["text/html", "application/xhtml+xml", "TEXT/HTML; charset=utf-8"]) {
    assert.equal(VDM.shouldCapture(download({ mime }), VDM.DEFAULT_SETTINGS, OWN_ID), false, mime);
  }
});

test("shouldCapture: other mime types are captured", () => {
  for (const mime of ["application/zip", "video/mp4", "text/plain", "", undefined]) {
    assert.equal(VDM.shouldCapture(download({ mime }), VDM.DEFAULT_SETTINGS, OWN_ID), true, String(mime));
  }
});

test("shouldCapture: skipHosts matches the exact host and its subdomains", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, skipHosts: ["Intranet.Example.com"] };
  const at = (url) => VDM.shouldCapture(download({ url, finalUrl: url }), settings, OWN_ID);
  assert.equal(at("https://intranet.example.com/a.zip"), false);
  assert.equal(at("https://a.intranet.example.com/a.zip"), false);
  assert.equal(at("https://example.com/a.zip"), true);
});

test("shouldCapture: skipHosts does not match a different host that merely ends with the name", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, skipHosts: ["example.com"] };
  const url = "https://notexample.com/a.zip";
  assert.equal(VDM.shouldCapture(download({ url, finalUrl: url }), settings, OWN_ID), true);
});

test("shouldCapture: a leading dot in skipHosts is accepted", () => {
  const settings = { ...VDM.DEFAULT_SETTINGS, skipHosts: [".example.com"] };
  const url = "https://dl.example.com/a.zip";
  assert.equal(VDM.shouldCapture(download({ url, finalUrl: url }), settings, OWN_ID), false);
});

test("shouldCapture: complete and interrupted downloads are not captured", () => {
  for (const state of ["complete", "interrupted"]) {
    assert.equal(VDM.shouldCapture(download({ state }), VDM.DEFAULT_SETTINGS, OWN_ID), false, state);
  }
});

test("shouldCapture: an in_progress download is captured", () => {
  assert.equal(VDM.shouldCapture(download({ state: "in_progress" }), VDM.DEFAULT_SETTINGS, OWN_ID), true);
});

test("shouldCapture: partial settings fall back to defaults", () => {
  assert.equal(VDM.shouldCapture(download(), {}, OWN_ID), true);
  assert.equal(VDM.shouldCapture(download(), { capture: false }, OWN_ID), false);
});

test("shouldCapture: a missing item is not captured", () => {
  assert.equal(VDM.shouldCapture(undefined, VDM.DEFAULT_SETTINGS, OWN_ID), false);
});

test("cookieHeader: joins name=value pairs with '; '", () => {
  assert.equal(
    VDM.cookieHeader([
      { name: "a", value: "1" },
      { name: "b", value: "2" },
    ]),
    "a=1; b=2",
  );
});

test("cookieHeader: empty or missing input gives null", () => {
  assert.equal(VDM.cookieHeader([]), null);
  assert.equal(VDM.cookieHeader(undefined), null);
  assert.equal(VDM.cookieHeader(null), null);
});

test("cookieHeader: skips entries without a name and keeps empty values", () => {
  assert.equal(
    VDM.cookieHeader([{ name: "", value: "x" }, { name: "sid", value: "" }, null]),
    "sid=",
  );
  assert.equal(VDM.cookieHeader([{ value: "orphan" }]), null);
});

test("baseName: splits on forward slashes", () => {
  assert.equal(VDM.baseName("/downloads/files/a.zip"), "a.zip");
});

test("baseName: splits on backslashes (Windows paths)", () => {
  assert.equal(VDM.baseName("C:\\Users\\me\\Downloads\\a.zip"), "a.zip");
});

test("baseName: mixed separators", () => {
  assert.equal(VDM.baseName("C:\\dir/sub\\file.iso"), "file.iso");
});

test("baseName: a bare name is returned unchanged", () => {
  assert.equal(VDM.baseName("plain.txt"), "plain.txt");
});

test("baseName: empty or missing input gives null", () => {
  assert.equal(VDM.baseName(""), null);
  assert.equal(VDM.baseName(undefined), null);
  assert.equal(VDM.baseName(null), null);
  assert.equal(VDM.baseName("/"), null);
});

test("buildDownloadMessage: full message with cookie array", () => {
  const msg = VDM.buildDownloadMessage({
    url: "https://files.example.com/a.zip",
    referrer: "https://example.com/page",
    fileName: "C:\\Users\\me\\Downloads\\a.zip",
    cookies: [
      { name: "a", value: "1" },
      { name: "b", value: "2" },
    ],
    userAgent: "Mozilla/5.0 test",
  });
  assert.deepEqual(msg, {
    type: "download",
    url: "https://files.example.com/a.zip",
    referrer: "https://example.com/page",
    cookies: "a=1; b=2",
    user_agent: "Mozilla/5.0 test",
    file_name: "a.zip",
  });
});

test("buildDownloadMessage: missing optional values become null", () => {
  const msg = VDM.buildDownloadMessage({ url: "https://x.example/a" });
  assert.deepEqual(msg, {
    type: "download",
    url: "https://x.example/a",
    referrer: null,
    cookies: null,
    user_agent: null,
    file_name: null,
  });
});

test("buildDownloadMessage: empty referrer, empty cookies and empty name become null", () => {
  const msg = VDM.buildDownloadMessage({
    url: "https://x.example/a",
    referrer: "",
    fileName: "",
    cookies: [],
    userAgent: "",
  });
  assert.equal(msg.referrer, null);
  assert.equal(msg.cookies, null);
  assert.equal(msg.user_agent, null);
  assert.equal(msg.file_name, null);
});

test("buildDownloadMessage: a ready cookie header string is passed through", () => {
  const msg = VDM.buildDownloadMessage({ url: "https://x.example/a", cookies: "k=v; z=9" });
  assert.equal(msg.cookies, "k=v; z=9");
});

test("buildDownloadMessage: only the protocol keys are present", () => {
  const msg = VDM.buildDownloadMessage({ url: "https://x.example/a", fileName: "a" });
  assert.deepEqual(Object.keys(msg).sort(), [
    "cookies",
    "file_name",
    "referrer",
    "type",
    "url",
    "user_agent",
  ]);
});

test("isHttpUrl: accepts http and https", () => {
  assert.equal(VDM.isHttpUrl("http://example.com/a"), true);
  assert.equal(VDM.isHttpUrl("https://example.com/a?b=c"), true);
  assert.equal(VDM.isHttpUrl("HTTPS://EXAMPLE.COM/"), true);
});

test("isHttpUrl: rejects other schemes", () => {
  for (const url of ["blob:https://x/1", "data:text/plain,hi", "file:///a", "ftp://x/a", "javascript:alert(1)"]) {
    assert.equal(VDM.isHttpUrl(url), false, url);
  }
});

test("isHttpUrl: rejects empty, non-string and unparseable input", () => {
  assert.equal(VDM.isHttpUrl(""), false);
  assert.equal(VDM.isHttpUrl(undefined), false);
  assert.equal(VDM.isHttpUrl(null), false);
  assert.equal(VDM.isHttpUrl(42), false);
  assert.equal(VDM.isHttpUrl("not a url"), false);
});

test("DEFAULT_SETTINGS has the documented defaults", () => {
  assert.deepEqual(VDM.DEFAULT_SETTINGS, { capture: true, minSizeKB: 0, skipHosts: [] });
});

// ---- Streams ----

const PAGE = "https://example.com/watch";

function stream(url, kind = "hls", overrides = {}) {
  return { url, kind, pageUrl: PAGE, title: "Title", at: "2024-01-01T00:00:00Z", ...overrides };
}

const urlsOf = (list) => list.map((entry) => entry.url);

test("streamKind: .m3u8 and .mpd paths, case-insensitive", () => {
  assert.equal(VDM.streamKind("https://cdn.example.com/live/master.m3u8", null), "hls");
  assert.equal(VDM.streamKind("https://cdn.example.com/a/INDEX.M3U8", null), "hls");
  assert.equal(VDM.streamKind("https://cdn.example.com/v/manifest.mpd", null), "dash");
  assert.equal(VDM.streamKind("https://cdn.example.com/v/MANIFEST.MPD", null), "dash");
});

test("streamKind: the query string is ignored when matching the extension", () => {
  assert.equal(VDM.streamKind("https://x.example/a/index.m3u8?token=abc.mpd", null), "hls");
  assert.equal(VDM.streamKind("https://x.example/stream.mpd?x=.m3u8", null), "dash");
  assert.equal(VDM.streamKind("https://x.example/file?name=master.m3u8", null), null);
});

test("streamKind: HLS content types, with or without parameters", () => {
  for (const ct of [
    "application/vnd.apple.mpegurl",
    "application/x-mpegurl",
    "audio/mpegurl",
    "audio/x-mpegurl",
    "application/vnd.apple.mpegurl; charset=utf-8",
    "APPLICATION/X-MPEGURL",
  ]) {
    assert.equal(VDM.streamKind("https://x.example/api/playlist", ct), "hls", ct);
  }
});

test("streamKind: DASH content types, with or without parameters", () => {
  for (const ct of ["application/dash+xml", "application/dash+xml; charset=utf-8", "Application/DASH+XML"]) {
    assert.equal(VDM.streamKind("https://x.example/api/playlist", ct), "dash", ct);
  }
});

test("streamKind: a content type can mark a stream even without an extension", () => {
  assert.equal(VDM.streamKind("https://x.example/no-extension", "application/vnd.apple.mpegurl"), "hls");
  assert.equal(VDM.streamKind("https://x.example/no-extension", "application/dash+xml"), "dash");
});

test("streamKind: ordinary files and pages are not streams", () => {
  assert.equal(VDM.streamKind("https://x.example/a.mp4", "video/mp4"), null);
  assert.equal(VDM.streamKind("https://x.example/a.zip", "application/zip"), null);
  assert.equal(VDM.streamKind("https://x.example/page", "text/html; charset=utf-8"), null);
  assert.equal(VDM.streamKind("https://x.example/a.m3u8.txt", "text/plain"), null);
  assert.equal(VDM.streamKind("https://x.example/a.mpd.json", "application/json"), null);
});

test("streamKind: a missing content type only leaves the URL rule", () => {
  assert.equal(VDM.streamKind("https://x.example/a.m3u8", undefined), "hls");
  assert.equal(VDM.streamKind("https://x.example/a.bin", undefined), null);
});

test("streamKind: invalid and non-http URLs are never streams", () => {
  for (const url of ["not a url", "", undefined, null, "file:///C:/a.m3u8", "blob:https://x/1.m3u8"]) {
    assert.equal(VDM.streamKind(url, "application/vnd.apple.mpegurl"), null, String(url));
  }
});

test("addStream: the new entry goes to the front (newest first)", () => {
  const a = stream("https://x.example/a.m3u8");
  const b = stream("https://x.example/b.m3u8");
  const list = VDM.addStream(VDM.addStream([], a), b);
  assert.deepEqual(urlsOf(list), [b.url, a.url]);
});

test("addStream: does not mutate the input list or the incoming entry", () => {
  const list = VDM.addStream([], stream("https://x.example/1.m3u8"));
  const before = JSON.parse(JSON.stringify(list));
  const entry = stream("https://x.example/2.m3u8");
  const entryBefore = { ...entry };

  const next = VDM.addStream(list, entry);

  assert.deepEqual(list, before);
  assert.deepEqual(entry, entryBefore);
  assert.notEqual(next, list);
  assert.equal(next.length, 2);
});

test("addStream: a duplicate URL keeps the original position and entry (no move to front)", () => {
  const a = stream("https://x.example/a.m3u8", "hls", { title: "first" });
  const b = stream("https://x.example/b.m3u8");
  const list = VDM.addStream(VDM.addStream([], a), b); // [b, a]

  const again = VDM.addStream(list, stream(a.url, "hls", { title: "second" }));

  assert.notEqual(again, list);
  assert.deepEqual(urlsOf(again), [b.url, a.url]);
  assert.equal(again[1].title, "first");
});

test("addStream: a URL is only de-duped by exact match", () => {
  const list = VDM.addStream([], stream("https://x.example/a.m3u8"));
  assert.equal(VDM.addStream(list, stream("https://x.example/a.m3u8?v=2")).length, 2);
});

test("addStream: the first HLS entry is primary, later HLS variants are not", () => {
  const master = stream("https://x.example/master.m3u8");
  const variant = stream("https://x.example/720p/index.m3u8");
  const list = VDM.addStream(VDM.addStream([], master), variant);
  assert.deepEqual(list.map((e) => [e.url, e.primary]), [
    [variant.url, false],
    [master.url, true],
  ]);
});

test("addStream: primary is per kind, so the first DASH entry is primary too", () => {
  const hls = stream("https://x.example/master.m3u8", "hls");
  const dash = stream("https://x.example/manifest.mpd", "dash");
  const list = VDM.addStream(VDM.addStream([], hls), dash);
  assert.equal(list.find((e) => e.kind === "dash").primary, true);
  assert.equal(list.find((e) => e.kind === "hls").primary, true);
});

test("addStream: a primary value on the incoming entry is overwritten", () => {
  const list = VDM.addStream([], stream("https://x.example/master.m3u8"));
  const next = VDM.addStream(list, stream("https://x.example/v.m3u8", "hls", { primary: true }));
  assert.equal(next[0].primary, false);
});

test("addStream: accepts a missing list", () => {
  const list = VDM.addStream(undefined, stream("https://x.example/a.m3u8"));
  assert.equal(list.length, 1);
  assert.equal(list[0].primary, true);
});

test("addStream: when full, the oldest non-primary entry is dropped and primary entries stay", () => {
  const u = (n) => `https://x.example/${n}.m3u8`;
  let list = [];
  for (const n of [1, 2, 3, 4]) list = VDM.addStream(list, stream(u(n)), 3);
  assert.deepEqual(urlsOf(list), [u(4), u(3), u(1)]);
  assert.equal(list[2].primary, true);
});

test("addStream: the default limit is MAX_STREAMS_PER_TAB (20)", () => {
  assert.equal(VDM.MAX_STREAMS_PER_TAB, 20);
  let list = [];
  for (let n = 1; n <= 25; n += 1) list = VDM.addStream(list, stream(`https://x.example/${n}.m3u8`));
  assert.equal(list.length, 20);
  assert.equal(list[0].url, "https://x.example/25.m3u8");
  assert.equal(list[19].url, "https://x.example/1.m3u8");
  assert.equal(list[19].primary, true);
});

test("addStream: when every entry is primary, the oldest one is dropped", () => {
  const master = stream("https://x.example/master.m3u8", "hls");
  const dash = stream("https://x.example/manifest.mpd", "dash");
  const list = VDM.addStream(VDM.addStream([], master), dash, 1);
  assert.deepEqual(urlsOf(list), [dash.url]);
});

test("shortStreamLabel: host plus the last path segment, with the directories elided", () => {
  assert.equal(
    VDM.shortStreamLabel("https://cdn.example.com/a/b/c/master.m3u8?token=1"),
    "cdn.example.com/\u2026/master.m3u8",
  );
});

test("shortStreamLabel: a single path segment is shown without an ellipsis", () => {
  assert.equal(VDM.shortStreamLabel("https://cdn.example.com/master.m3u8"), "cdn.example.com/master.m3u8");
});

test("shortStreamLabel: a bare host keeps a trailing slash and a port is kept", () => {
  assert.equal(VDM.shortStreamLabel("https://cdn.example.com/"), "cdn.example.com/");
  assert.equal(VDM.shortStreamLabel("https://host:8443/x/y.mpd"), "host:8443/\u2026/y.mpd");
});

test("shortStreamLabel: unparseable or non-http input is returned unchanged", () => {
  assert.equal(VDM.shortStreamLabel("not a url"), "not a url");
  assert.equal(VDM.shortStreamLabel("blob:https://x/1"), "blob:https://x/1");
  assert.equal(VDM.shortStreamLabel(undefined), "");
});

test("isMagnetUrl: BitTorrent magnet links only", () => {
  assert.equal(VDM.isMagnetUrl("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=x"), true);
  assert.equal(VDM.isMagnetUrl("MAGNET:?dn=x&xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK"), true);
  assert.equal(VDM.isMagnetUrl("magnet:?xt=urn:sha1:abc"), false);
  assert.equal(VDM.isMagnetUrl("https://example.com/magnet:?xt=urn:btih:1"), false);
  assert.equal(VDM.isMagnetUrl(undefined), false);
});
