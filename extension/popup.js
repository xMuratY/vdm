// Popup: shows whether Vibe Download Manager answers, and edits the two capture settings.
const api = globalThis.browser ?? globalThis.chrome;
const NATIVE_HOST = "com.vdm.native";

// Translated text from _locales (English when a message is missing).
const msg = (name, subs) => api.i18n.getMessage(name, subs) || name;

// Fill in the static text: each data-i18n attribute names a message.
for (const el of document.querySelectorAll("[data-i18n]")) {
  el.textContent = msg(el.dataset.i18n);
}
document.title = msg("extName");

const statusEl = document.getElementById("status");
const captureEl = document.getElementById("capture");
const minSizeEl = document.getElementById("minSize");
const lastErrorEl = document.getElementById("lastError");

function showConnected(version) {
  statusEl.className = "status ok";
  statusEl.textContent = msg("popupConnected", [version ?? ""]).trim();
}

function showNotFound() {
  statusEl.className = "status bad";
  // Built with DOM nodes so no text is parsed as HTML.
  const code = document.createElement("code");
  code.textContent = "vdm-nmhost install";
  // The message has $1 where the command goes; split around it.
  const [before, after = ""] = msg("popupNotFound", ["\u0000"]).split("\u0000");
  statusEl.replaceChildren(before, code, after);
}

// Asks the host for its version. Any failure means it is missing or broken.
async function checkConnection() {
  try {
    const reply = await api.runtime.sendNativeMessage(NATIVE_HOST, { type: "ping" });
    if (reply?.type !== "pong") throw new Error("unexpected reply");
    showConnected(reply.version);
  } catch (e) {
    showNotFound();
  }
}

async function loadSettings() {
  const stored = await api.storage.local.get("settings");
  return { ...VDM.DEFAULT_SETTINGS, ...stored.settings };
}

// Saves one field, keeping the rest of the stored settings.
async function saveSetting(key, value) {
  const current = await loadSettings();
  await api.storage.local.set({ settings: { ...current, [key]: value } });
}

async function showLastError() {
  const { lastStatus } = await api.storage.local.get("lastStatus");
  if (lastStatus && !lastStatus.ok && lastStatus.error) {
    const when = new Date(lastStatus.at).toLocaleString();
    lastErrorEl.textContent = msg("popupLastError", [when, lastStatus.error]);
    lastErrorEl.hidden = false;
  }
}

captureEl.addEventListener("change", () => {
  saveSetting("capture", captureEl.checked);
});

minSizeEl.addEventListener("change", () => {
  // Negative, empty or non-numeric input falls back to 0.
  const kb = Math.max(0, Math.floor(Number(minSizeEl.value)) || 0);
  minSizeEl.value = String(kb);
  saveSetting("minSizeKB", kb);
});

// Videos on this page: the HLS/DASH streams the background saw in the active tab.
const streamsEl = document.getElementById("streams");
const noStreamsEl = document.getElementById("noStreams");

// Builds one stream row with DOM nodes, so page-supplied text is never parsed as HTML.
function streamRow(stream) {
  const item = document.createElement("li");
  item.className = "stream";

  const head = document.createElement("div");
  head.className = "stream-head";
  const kind = document.createElement("span");
  kind.className = "kind";
  kind.textContent = stream.kind === "dash" ? "DASH" : "HLS";
  const url = document.createElement("span");
  url.className = "stream-url";
  url.textContent = VDM.shortStreamLabel(stream.url);
  url.title = stream.url;
  head.append(kind, url);

  const page = document.createElement("div");
  page.className = "stream-page";
  page.textContent = stream.title || stream.pageUrl || "";
  page.title = stream.pageUrl || "";

  const actions = document.createElement("div");
  actions.className = "stream-actions";
  const button = document.createElement("button");
  button.type = "button";
  button.textContent = msg("popupDownloadStream");
  const result = document.createElement("span");
  result.className = "result";
  button.addEventListener("click", async () => {
    button.disabled = true;
    result.className = "result";
    result.textContent = msg("popupSending");
    const reply = await sendStream(stream);
    button.disabled = false;
    if (reply?.ok) {
      result.className = "result ok";
      result.textContent = msg("popupSent");
    } else {
      result.className = "result bad";
      result.textContent = reply?.error || msg("popupSendFailed");
    }
  });
  actions.append(button, result);

  item.append(head, page, actions);
  return item;
}

async function sendStream(stream) {
  try {
    return await api.runtime.sendMessage({
      type: "send-stream",
      url: stream.url,
      pageUrl: stream.pageUrl,
    });
  } catch (e) {
    return { ok: false, error: e?.message || String(e) };
  }
}

async function showStreams() {
  let streams = [];
  try {
    const [tab] = await api.tabs.query({ active: true, currentWindow: true });
    if (tab?.id !== undefined) {
      const reply = await api.runtime.sendMessage({ type: "list-streams", tabId: tab.id });
      if (Array.isArray(reply?.streams)) streams = reply.streams;
    }
  } catch (e) {
    streams = [];
  }

  // Primary entries (the HLS master, the DASH manifest) come first; the order is otherwise newest first.
  const sorted = [...streams].sort((a, b) => Number(!!b.primary) - Number(!!a.primary));
  streamsEl.replaceChildren(...sorted.map(streamRow));
  noStreamsEl.hidden = sorted.length > 0;
}

(async () => {
  const settings = await loadSettings();
  captureEl.checked = settings.capture;
  minSizeEl.value = String(settings.minSizeKB);
  await showLastError();
  await showStreams();
  await checkConnection();
})();
