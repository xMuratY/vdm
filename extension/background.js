// Service worker (Chrome/Edge/Brave) or event page (Firefox).
// Listeners are registered synchronously at the top level, as MV3 requires.

// Chrome loads decide.js through importScripts. Firefox already loaded it
// from the manifest's background.scripts, so this throws there and is ignored.
try {
  importScripts("decide.js");
} catch (e) {
  /* already loaded */
}

const api = globalThis.browser ?? globalThis.chrome;
// Translated text from _locales (English when a message is missing).
const msg = (name, subs) => api.i18n.getMessage(name, subs) || name;

const NATIVE_HOST = "com.vdm.native";
const MENU_ID = "vdm-download";

let settings = { ...VDM.DEFAULT_SETTINGS };
const settingsReady = loadSettings().catch((err) => console.error("VDM settings load failed", err));

async function loadSettings() {
  const stored = await api.storage.local.get("settings");
  settings = { ...VDM.DEFAULT_SETTINGS, ...stored.settings };
}

api.storage.onChanged.addListener((changes, area) => {
  if (area === "local" && changes.settings) {
    settings = { ...VDM.DEFAULT_SETTINGS, ...changes.settings.newValue };
  }
});

// Runs a browser API call and swallows any error, returning undefined instead.
async function quiet(fn) {
  try {
    return await fn();
  } catch (e) {
    return undefined;
  }
}

async function recordStatus(ok, error) {
  await api.storage.local.set({
    lastStatus: { ok, error: error ?? null, at: new Date().toISOString() },
  });
}

// Sends one message to VDM. Always resolves to {ok, error}, never rejects.
async function sendToVdm(message) {
  try {
    const reply = await api.runtime.sendNativeMessage(NATIVE_HOST, message);
    if (reply?.type === "result" && reply.ok === true) return { ok: true, error: null };
    return { ok: false, error: reply?.error || msg("errUnexpectedReply") };
  } catch (err) {
    return { ok: false, error: err?.message || String(err) };
  }
}

async function cookiesFor(url) {
  return (await quiet(() => api.cookies.getAll({ url }))) ?? [];
}

// Takes over a browser download and hands it to VDM.
async function handleCreated(item) {
  await settingsReady;
  if (!VDM.shouldCapture(item, settings, api.runtime.id)) return;

  const url = item.finalUrl || item.url;

  // Stop the browser's own copy and drop it from the download list.
  await quiet(() => api.downloads.cancel(item.id));
  await quiet(() => api.downloads.erase({ id: item.id }));

  const message = VDM.buildDownloadMessage({
    url,
    referrer: item.referrer,
    fileName: item.filename,
    cookies: await cookiesFor(url),
    userAgent: navigator.userAgent,
  });
  const result = await sendToVdm(message);
  await recordStatus(result.ok, result.error);

  if (!result.ok) {
    // VDM is missing or refused: let the browser download the file itself.
    // Our own downloads are not captured again (byExtensionId check).
    console.warn("VDM handoff failed, falling back to browser download:", result.error);
    await quiet(() => api.downloads.download({ url }));
  }
}

// Sends a link or media URL chosen from the context menu.
async function handleContextMenu(info, tab) {
  const url = info.linkUrl || info.srcUrl;
  if (!VDM.isHttpUrl(url)) {
    await recordStatus(false, msg("errHttpLinksOnly"));
    return;
  }
  const message = VDM.buildDownloadMessage({
    url,
    referrer: info.pageUrl || tab?.url || null,
    cookies: await cookiesFor(url),
    userAgent: navigator.userAgent,
  });
  const result = await sendToVdm(message);
  await recordStatus(result.ok, result.error);
}

api.downloads.onCreated.addListener((item) => {
  handleCreated(item).catch((err) => console.error("VDM capture failed", err));
});

api.runtime.onInstalled.addListener(async () => {
  // Remove first so an update does not fail on a duplicate menu id.
  await quiet(() => api.contextMenus.removeAll());
  api.contextMenus.create({
    id: MENU_ID,
    title: msg("menuDownload"),
    contexts: ["link", "image", "video", "audio"],
  });
});

api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId !== MENU_ID) return;
  handleContextMenu(info, tab).catch((err) => console.error("VDM context download failed", err));
});

// ---- Streams: HLS and DASH playlists that a page plays ----

const BADGE_COLOR = [64, 110, 230, 255]; // the popup accent blue

// Per-tab stream lists live in storage.session, which survives service worker
// restarts. Without it, a Map is used and the lists reset when the browser restarts it.
const memoryStreams = new Map();
const sessionArea = api.storage.session ?? {
  async get(key) {
    return memoryStreams.has(key) ? { [key]: memoryStreams.get(key) } : {};
  },
  async set(items) {
    for (const [key, value] of Object.entries(items)) memoryStreams.set(key, value);
  },
  async remove(key) {
    memoryStreams.delete(key);
  },
};

// Stream reads and writes run one at a time, so two responses that arrive
// together cannot overwrite each other's update.
let streamChain = Promise.resolve();
function serialized(task) {
  const run = streamChain.then(task);
  streamChain = run.catch(() => {});
  return run;
}

const OWN_ORIGIN = new URL(api.runtime.getURL("/")).origin;

// True for requests this extension made itself (its popup or background page).
function isOwnRequest(details) {
  const source = details.initiator || details.originUrl;
  if (!source) return false;
  try {
    return new URL(source).origin === OWN_ORIGIN;
  } catch {
    return false;
  }
}

// Value of a response header, matched by name case-insensitively, or null.
function headerValue(headers, name) {
  const wanted = name.toLowerCase();
  const header = (headers ?? []).find((h) => String(h.name ?? "").toLowerCase() === wanted);
  return header?.value ?? null;
}

function streamKey(tabId) {
  return `streams:${tabId}`;
}

async function readStreams(tabId) {
  const key = streamKey(tabId);
  const stored = await sessionArea.get(key);
  return Array.isArray(stored?.[key]) ? stored[key] : [];
}

// Adds one detected stream to its tab's list and updates the badge.
function recordStream(details, kind) {
  return serialized(async () => {
    const tab = await quiet(() => api.tabs.get(details.tabId));
    if (!tab) return; // the tab closed before the stream was recorded

    const entry = {
      url: details.url,
      kind,
      pageUrl: details.documentUrl || details.originUrl || tab.url || "",
      title: tab.title || "",
      at: new Date().toISOString(),
    };
    const next = VDM.addStream(await readStreams(details.tabId), entry);
    await sessionArea.set({ [streamKey(details.tabId)]: next });
    await quiet(() => api.action.setBadgeBackgroundColor({ tabId: details.tabId, color: BADGE_COLOR }));
    await quiet(() => api.action.setBadgeText({ tabId: details.tabId, text: String(next.length) }));
  });
}

function clearStreams(tabId) {
  return serialized(async () => {
    await sessionArea.remove(streamKey(tabId));
    await quiet(() => api.action.setBadgeText({ tabId, text: "" }));
  });
}

function onStreamResponse(details) {
  if (details.tabId < 0 || !VDM.isHttpUrl(details.url)) return;
  if (details.statusCode >= 400) return; // an error page is not a playable stream
  if (isOwnRequest(details)) return;

  const kind = VDM.streamKind(details.url, headerValue(details.responseHeaders, "content-type"));
  if (!kind) return;
  recordStream(details, kind).catch((err) => console.error("VDM stream detection failed", err));
}

// Sends a stream chosen in the popup. Always resolves to {ok, error}.
async function sendStream({ url, pageUrl }) {
  if (!VDM.isHttpUrl(url)) return { ok: false, error: msg("errHttpStreamsOnly") };

  const result = await sendToVdm(
    VDM.buildDownloadMessage({
      url,
      referrer: VDM.isHttpUrl(pageUrl) ? pageUrl : null,
      cookies: await cookiesFor(url),
      userAgent: navigator.userAgent,
    }),
  );
  await quiet(() => recordStatus(result.ok, result.error));
  return result;
}

// Requests from the popup. Messages from anywhere else are ignored.
api.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender.id !== api.runtime.id) return false;

  if (message?.type === "list-streams") {
    readStreams(message.tabId).then(
      (streams) => sendResponse({ streams }),
      () => sendResponse({ streams: [] }),
    );
    return true;
  }
  if (message?.type === "send-stream") {
    sendStream(message)
      .catch((err) => ({ ok: false, error: err?.message || String(err) }))
      .then(sendResponse);
    return true;
  }
  return false;
});

api.webRequest.onResponseStarted.addListener(
  onStreamResponse,
  { urls: ["<all_urls>"], types: ["xmlhttprequest", "media", "other"] },
  ["responseHeaders"],
);

// A new page load starts with an empty stream list.
api.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading" && changeInfo.url) {
    clearStreams(tabId).catch((err) => console.error("VDM stream clear failed", err));
  }
});

api.tabs.onRemoved.addListener((tabId) => {
  clearStreams(tabId).catch((err) => console.error("VDM stream clear failed", err));
});
