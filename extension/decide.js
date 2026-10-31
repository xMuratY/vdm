// Pure download-capture and stream-detection logic. No browser APIs here, so this file runs in the
// Chrome service worker (importScripts), in Firefox background scripts and
// under Node's test runner.

const DEFAULT_SETTINGS = Object.freeze({
  capture: true,
  minSizeKB: 0,
  skipHosts: [],
});

// Pages are left to the browser: we only take over file downloads.
const SKIPPED_MIME_TYPES = new Set(["text/html", "application/xhtml+xml"]);
const FINISHED_STATES = new Set(["complete", "interrupted"]);

// True for http: and https: URLs only.
function isHttpUrl(url) {
  if (typeof url !== "string" || url === "") return false;
  try {
    const { protocol } = new URL(url);
    return protocol === "http:" || protocol === "https:";
  } catch {
    return false;
  }
}

// True for BitTorrent magnet links (magnet:?xt=urn:btih:...).
function isMagnetUrl(url) {
  return typeof url === "string" && /^magnet:\?(.*&)?xt=urn:btih:/i.test(url.trim());
}

// Lowercased hostname of a URL, or "" if it cannot be parsed.
function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

// Normalizes a user-entered skip entry: ".example.com" and "example.com" match alike.
function normalizeHost(entry) {
  return String(entry).trim().toLowerCase().replace(/^\.+/, "");
}

// Exact match or subdomain match.
function hostMatches(host, pattern) {
  return pattern !== "" && (host === pattern || host.endsWith("." + pattern));
}

// Decides whether VDM should take over a download.
// item: a downloads.DownloadItem-like object; settings: stored settings;
// ownExtensionId: this extension's id, so our own fallback downloads are skipped.
function shouldCapture(item, settings, ownExtensionId) {
  const s = { ...DEFAULT_SETTINGS, ...settings };
  if (!s.capture || !item) return false;
  if (FINISHED_STATES.has(item.state)) return false;

  const url = item.finalUrl || item.url;
  if (!isHttpUrl(url)) return false;

  // Downloads we restarted ourselves must go to the browser, not back to VDM.
  if (item.byExtensionId && item.byExtensionId === ownExtensionId) return false;

  // A size of -1, 0 or undefined means "unknown", so it is never filtered out.
  const size = item.fileSize;
  if (size > 0 && size < s.minSizeKB * 1024) return false;

  const mime = String(item.mime || "").split(";")[0].trim().toLowerCase();
  if (SKIPPED_MIME_TYPES.has(mime)) return false;

  const host = hostOf(url);
  const skipHosts = Array.isArray(s.skipHosts) ? s.skipHosts : [];
  if (skipHosts.some((entry) => hostMatches(host, normalizeHost(entry)))) return false;

  return true;
}

// [{name, value}] -> "a=1; b=2", or null when there are no cookies.
function cookieHeader(cookies) {
  if (!Array.isArray(cookies)) return null;
  const pairs = cookies
    .filter((c) => c && c.name)
    .map((c) => `${c.name}=${c.value ?? ""}`);
  return pairs.length > 0 ? pairs.join("; ") : null;
}

// Last path component, accepting both "/" and "\" separators.
function baseName(path) {
  if (typeof path !== "string") return null;
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : null;
}

// Builds the native-messaging "download" request for the host.
// cookies may be the [{name, value}] array from cookies.getAll or a ready header string.
function buildDownloadMessage({
  url,
  referrer = null,
  fileName = null,
  cookies = null,
  userAgent = null,
} = {}) {
  const cookieValue =
    typeof cookies === "string" ? cookies || null : cookieHeader(cookies);
  return {
    type: "download",
    url,
    referrer: referrer || null,
    cookies: cookieValue,
    user_agent: userAgent || null,
    file_name: baseName(fileName),
  };
}

// ---- Streams: HLS and DASH playlists that a page plays ----

const MAX_STREAMS_PER_TAB = 20;
const HLS_CONTENT_TYPES = new Set([
  "application/vnd.apple.mpegurl",
  "application/x-mpegurl",
  "audio/mpegurl",
  "audio/x-mpegurl",
]);
const DASH_CONTENT_TYPES = new Set(["application/dash+xml"]);

// "hls" or "dash" when a response is a playlist VDM can download, otherwise null.
// The path is matched without the query string, case-insensitively. A URL
// without a playlist extension still counts when its Content-Type says so.
// HLS is checked first, so a URL matching both rules is reported as "hls".
function streamKind(url, contentType) {
  if (!isHttpUrl(url)) return null;
  const path = new URL(url).pathname.toLowerCase();
  // Content-Type parameters such as "; charset=utf-8" are ignored.
  const mime = String(contentType || "").split(";")[0].trim().toLowerCase();
  if (path.endsWith(".m3u8") || HLS_CONTENT_TYPES.has(mime)) return "hls";
  if (path.endsWith(".mpd") || DASH_CONTENT_TYPES.has(mime)) return "dash";
  return null;
}

// Returns a NEW list with entry at the front (newest first). The input is never changed.
//  - De-dupe is by URL only. A URL already in the list is ignored: the copy
//    keeps the original entry in its original position. Moving it to the front
//    instead would reshuffle the list on every refetch of a live playlist.
//  - primary is true for the first entry of each kind in the list. HLS masters
//    are requested before their variant playlists, so the master is primary.
//    A primary value on the incoming entry is overwritten.
//  - When the list grows past max, the oldest non-primary entry is dropped, so
//    primary entries outlive variants. Only when every entry is primary is the
//    oldest one dropped.
function addStream(list, entry, max = MAX_STREAMS_PER_TAB) {
  const current = Array.isArray(list) ? list : [];
  if (current.some((item) => item.url === entry.url)) return current.slice();

  const primary = !current.some((item) => item.kind === entry.kind);
  const next = [{ ...entry, primary }, ...current];
  while (next.length > max) {
    let i = next.length - 1;
    while (i >= 0 && next[i].primary) i -= 1;
    next.splice(i >= 0 ? i : next.length - 1, 1);
  }
  return next;
}

// Short label for a stream URL: "cdn.example.com/…/master.m3u8".
// Directories are replaced by "…"; the query string is dropped.
// Non-http(s) or unparseable input is returned unchanged (as a string).
function shortStreamLabel(url) {
  if (!isHttpUrl(url)) return String(url ?? "");
  const parsed = new URL(url);
  const segments = parsed.pathname.split("/").filter(Boolean);
  if (segments.length === 0) return `${parsed.host}/`;
  const last = segments[segments.length - 1];
  return segments.length === 1 ? `${parsed.host}/${last}` : `${parsed.host}/…/${last}`;
}

const VDM = {
  DEFAULT_SETTINGS,
  MAX_STREAMS_PER_TAB,
  isHttpUrl,
  isMagnetUrl,
  shouldCapture,
  cookieHeader,
  baseName,
  buildDownloadMessage,
  streamKind,
  addStream,
  shortStreamLabel,
};

if (typeof module !== "undefined") module.exports = VDM;
else globalThis.VDM = VDM;
