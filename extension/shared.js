// Settings shared by the background worker and the popup.

export const APP_URL = "http://127.0.0.1:47809";

export const DEFAULTS = {
  enabled: true,
  captureAll: false,
  extensions:
    "zip rar 7z tar gz bz2 xz iso exe msi apk dmg deb rpm mp4 mkv avi mov wmv flv webm m4v mpg mpeg " +
    "mp3 wav flac aac ogg m4a pdf doc docx xls xlsx ppt pptx epub torrent bin img",
  minSizeKb: 0,
  showVideoButton: true,
};

export async function loadSettings() {
  return { ...DEFAULTS, ...(await chrome.storage.sync.get(DEFAULTS)) };
}

export function extensionOf(name) {
  const base = (name || "").split(/[\\/]/).pop().split("?")[0];
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(dot + 1).toLowerCase() : "";
}

/** POST with a short timeout; resolves to the HTTP status or 0 if the app is unreachable. */
export async function postToApp(path, body, timeoutMs = 2000) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const res = await fetch(APP_URL + path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: ctrl.signal,
    });
    return res.status;
  } catch {
    return 0;
  } finally {
    clearTimeout(timer);
  }
}

export async function pingApp() {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 1500);
  try {
    const res = await fetch(APP_URL + "/ping", { signal: ctrl.signal });
    return res.ok ? await res.json() : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}
