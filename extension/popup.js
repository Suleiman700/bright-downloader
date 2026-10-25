import { loadSettings, pingApp } from "./shared.js";

const $ = (id) => document.getElementById(id);
const msg = (key, subs) => chrome.i18n.getMessage(key, subs) || key;

for (const el of document.querySelectorAll("[data-i18n]")) el.textContent = msg(el.dataset.i18n);
document.documentElement.dir = msg("@@bidi_dir");
document.documentElement.lang = chrome.i18n.getUILanguage();

async function init() {
  const s = await loadSettings();
  $("enabled").checked = s.enabled;
  $("showVideoButton").checked = s.showVideoButton;
  $("captureAll").checked = s.captureAll;
  $("extensions").value = s.extensions;
  $("minSizeKb").value = s.minSizeKb;
  $("extensions").disabled = s.captureAll;

  const save = () =>
    chrome.storage.sync.set({
      enabled: $("enabled").checked,
      captureAll: $("captureAll").checked,
      extensions: $("extensions").value.trim(),
      minSizeKb: Math.max(0, Number($("minSizeKb").value) || 0),
      showVideoButton: $("showVideoButton").checked,
    });
  for (const id of ["enabled", "captureAll", "extensions", "minSizeKb", "showVideoButton"]) {
    $(id).addEventListener("change", () => {
      $("extensions").disabled = $("captureAll").checked;
      save();
    });
  }

  const app = await pingApp();
  const status = $("status");
  status.textContent = app ? msg("connected", [app.version]) : msg("appNotRunning");
  status.className = `status ${app ? "ok" : "off"}`;
  await showMedia();
}

function fmtSize(n) {
  if (!n) return "";
  return n >= 1 << 30 ? `${(n / (1 << 30)).toFixed(1)} GB` : `${Math.max(1, Math.round(n / (1 << 20)))} MB`;
}

function nameOf(url) {
  try {
    return decodeURIComponent(new URL(url).pathname.split("/").filter(Boolean).pop() || url);
  } catch {
    return url;
  }
}

/** Media the current tab loaded, each with a Download button. */
async function showMedia() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab?.id) return;
  const items = (await chrome.runtime.sendMessage({ type: "brightdl:list", tabId: tab.id })) ?? [];
  $("media").hidden = items.length === 0;
  $("media-list").replaceChildren(
    ...items.map((e) => {
      const li = document.createElement("li");
      const text = document.createElement("span");
      const kind =
        e.kind === "file"
          ? nameOf(e.url)
          : e.live
            ? msg("liveStream")
            : e.variants.length
              ? msg("streamQualities", [String(e.variants.length)])
              : msg("stream");
      text.textContent = e.size ? `${kind} · ${fmtSize(e.size)}` : kind;
      text.title = e.url;
      const btn = document.createElement("button");
      btn.textContent = msg("download");
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        const res = await chrome.runtime.sendMessage({
          type: "brightdl:download",
          tabId: tab.id,
          url: e.url,
          kind: e.kind,
          title: tab.title,
          pageUrl: e.pageUrl,
        });
        btn.textContent = res?.ok ? msg("sentShort") : msg("failed");
        if (!res?.ok) btn.title = res?.error ?? "";
      });
      li.append(text, btn);
      return li;
    }),
  );
}

init();
