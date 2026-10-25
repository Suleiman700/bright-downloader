// Captures browser downloads and hands them to the Bright Downloader app.
// If the app is not running, the browser keeps the download as usual.

import { extensionOf, loadSettings, postToApp } from "./shared.js";
import { forgetTab, listMedia, onResponse } from "./media.js";

async function cookieHeader(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

async function sendToApp(url, { referrer, fileName, variant } = {}) {
  return postToApp("/add", {
    url,
    fileName: fileName || null,
    referrer: referrer || null,
    cookies: await cookieHeader(url),
    userAgent: navigator.userAgent,
    variant: variant || null,
  });
}

function shouldCapture(item, settings) {
  if (!settings.enabled) return false;
  if (item.byExtensionId === chrome.runtime.id) return false; // our own fallback
  const url = item.finalUrl || item.url;
  if (!/^https?:/i.test(url)) return false; // blob:, data:, file: stay in the browser
  if (settings.minSizeKb > 0 && item.totalBytes > 0 && item.totalBytes < settings.minSizeKb * 1024) return false;
  if (settings.captureAll) return true;
  const wanted = new Set(settings.extensions.toLowerCase().split(/[\s,;]+/).filter(Boolean));
  return wanted.has(extensionOf(item.filename)) || wanted.has(extensionOf(url));
}

async function takeOver(item) {
  const url = item.finalUrl || item.url;
  const name = (item.filename || "").split(/[\\/]/).pop();
  const status = await sendToApp(url, { referrer: item.referrer, fileName: name });
  if (status === 202) {
    try {
      await chrome.downloads.cancel(item.id);
      await chrome.downloads.erase({ id: item.id });
    } catch {
      /* already gone */
    }
  }
}

if (chrome.downloads.onDeterminingFilename) {
  // Chrome / Edge: decide once the browser knows the final file name
  // (including Content-Disposition).
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    (async () => {
      const settings = await loadSettings();
      if (shouldCapture(item, settings)) await takeOver(item);
      suggest();
    })();
    return true; // suggest() is called asynchronously
  });
} else {
  // Firefox has no onDeterminingFilename; take over as soon as it starts.
  chrome.downloads.onCreated.addListener(async (item) => {
    const settings = await loadSettings();
    if (shouldCapture(item, settings)) await takeOver(item);
  });
}

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "brightdl-download",
    title: chrome.i18n.getMessage("menuDownload"),
    contexts: ["link", "image", "video", "audio"],
  });
});

chrome.contextMenus.onClicked.addListener(async (info) => {
  if (info.menuItemId !== "brightdl-download") return;
  const url = info.linkUrl || info.srcUrl;
  if (!url || !/^https?:/i.test(url)) return;
  const status = await sendToApp(url, { referrer: info.pageUrl });
  if (status !== 202) {
    // App not running: fall back to a normal browser download.
    chrome.downloads.download({ url });
  }
});

// ---------- videos on pages ----------

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    onResponse(details);
  },
  { urls: ["<all_urls>"], types: ["main_frame", "media", "xmlhttprequest", "other"] },
  ["responseHeaders"],
);

chrome.tabs.onRemoved.addListener(forgetTab);

/** A file name from the page title, with the media's extension. */
function nameFromTitle(title, url, kind) {
  const clean = (title || "")
    .replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 120);
  if (!clean) return null;
  const ext = kind === "file" ? extensionOf(url) || "mp4" : "mp4";
  return `${clean}.${ext}`;
}

async function downloadMedia(msg, sender) {
  const title = msg.title || sender.tab?.title || "";
  const status = await sendToApp(msg.url, {
    referrer: msg.pageUrl || sender.tab?.url,
    fileName: nameFromTitle(title, msg.url, msg.kind),
    variant: msg.variant,
  });
  if (status === 202) return { ok: true };
  if (status === 0 && msg.kind === "file") {
    // App not running: a plain file can still be saved by the browser.
    chrome.downloads.download({ url: msg.url });
    return { ok: true, fallback: true };
  }
  return {
    ok: false,
    error: status === 0 ? chrome.i18n.getMessage("appNotRunning") : chrome.i18n.getMessage("appRefused", [String(status)]),
  };
}

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  const tabId = msg.tabId ?? sender.tab?.id;
  if (msg.type === "brightdl:list") {
    listMedia(tabId).then(sendResponse);
    return true;
  }
  if (msg.type === "brightdl:count") {
    listMedia(tabId).then((items) => sendResponse({ count: items.length }));
    return true;
  }
  if (msg.type === "brightdl:download") {
    downloadMedia(msg, sender).then(sendResponse);
    return true;
  }
  return false;
});
