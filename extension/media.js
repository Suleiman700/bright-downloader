// Finds videos and audio a tab loads (plain files and HLS playlists) by
// watching its network responses. The list per tab survives the service
// worker being stopped (kept in storage.session).

const PLAYLIST_TYPE = /mpegurl/i;
const MANIFEST_TYPE = /dash\+xml/i;
const MEDIA_EXT = /\.(mp4|m4v|webm|mkv|mov|flv|mp3|m4a|ogg|oga|opus|flac|wav)$/i;
/** Pieces of a stream, not something to download on their own. */
const SEGMENT_EXT = /\.(ts|m4s|m4f|cmfv|cmfa|aac|vtt|webvtt)$/i;
/** Smaller plain files are previews, ads or UI sounds. */
const MIN_FILE_BYTES = 512 * 1024;
/** YouTube's player fetches scrambled pieces that can't be joined into a
 * file; its pages are offered through yt-dlp instead (see content.js). */
const IGNORED_HOSTS = /(^|\.)googlevideo\.com$/i;
const MAX_PER_TAB = 50;

const tabs = new Map(); // tabId -> { items: entry[], covered: string[] }
let nextId = 1;

async function state(tabId) {
  if (!tabs.has(tabId)) {
    const key = `media:${tabId}`;
    const saved = (await chrome.storage.session.get(key))[key];
    if (!tabs.has(tabId)) tabs.set(tabId, saved ?? { items: [], covered: [] });
    for (const e of tabs.get(tabId).items) nextId = Math.max(nextId, e.id + 1);
  }
  return tabs.get(tabId);
}

async function changed(tabId) {
  const s = tabs.get(tabId);
  const count = s ? s.items.length : 0;
  chrome.storage.session.set({ [`media:${tabId}`]: s }).catch(() => {});
  chrome.action.setBadgeBackgroundColor({ tabId, color: "#2563eb" }).catch(() => {});
  chrome.action.setBadgeText({ tabId, text: count ? String(count) : "" }).catch(() => {});
  // Tell the page's button (all frames) that something can be downloaded.
  chrome.tabs.sendMessage(tabId, { type: "brightdl:media", count }).catch(() => {});
}

export async function resetTab(tabId) {
  tabs.set(tabId, { items: [], covered: [] });
  await changed(tabId);
}

export function forgetTab(tabId) {
  tabs.delete(tabId);
  chrome.storage.session.remove(`media:${tabId}`).catch(() => {});
}

export async function listMedia(tabId) {
  return tabId == null ? [] : (await state(tabId)).items;
}

function header(details, name) {
  const h = details.responseHeaders?.find((x) => x.name.toLowerCase() === name);
  return h?.value ?? "";
}

function totalSize(details) {
  const range = header(details, "content-range").match(/\/(\d+)\s*$/);
  if (range) return Number(range[1]);
  const len = Number(header(details, "content-length"));
  return len > 0 ? len : null;
}

function attributes(s) {
  const out = {};
  for (const m of s.matchAll(/([A-Z0-9-]+)=("[^"]*"|[^,]*)/g)) out[m[1]] = m[2].replace(/^"|"$/g, "");
  return out;
}

function variantLabel(v) {
  const rate = v.bandwidth >= 1e6 ? `${(v.bandwidth / 1e6).toFixed(1)} Mbps` : `${Math.round(v.bandwidth / 1000)} kbps`;
  const height = v.resolution.split("x")[1];
  return height ? `${height}p · ${rate}` : rate;
}

/** Enough of an m3u8 to list qualities and tell live from finished. */
export function parsePlaylist(text, base) {
  if (!text.trimStart().startsWith("#EXTM3U")) return null;
  const out = { variants: [], renditions: [], duration: 0, ended: false, drm: false };
  let pending = null;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    if (line.startsWith("#EXT-X-STREAM-INF:")) pending = attributes(line.slice(18));
    else if (line.startsWith("#EXT-X-MEDIA:")) {
      const a = attributes(line.slice(13));
      if (a.URI) out.renditions.push(new URL(a.URI, base).href);
    } else if (line.startsWith("#EXTINF:")) out.duration += parseFloat(line.slice(8)) || 0;
    else if (line === "#EXT-X-ENDLIST") out.ended = true;
    else if (line.startsWith("#EXT-X-KEY:") && !/METHOD=(NONE|AES-128)\b/.test(line)) out.drm = true;
    else if (!line.startsWith("#") && pending) {
      const v = { url: new URL(line, base).href, bandwidth: Number(pending.BANDWIDTH) || 0, resolution: pending.RESOLUTION || "" };
      out.variants.push({ ...v, label: variantLabel(v) });
      pending = null;
    }
  }
  out.variants.sort((a, b) => b.bandwidth - a.bandwidth);
  return out;
}

/** ISO 8601 duration (PT1H2M3S) → seconds. */
function isoSeconds(s) {
  const m = /^P(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:([\d.]+)S)?)?$/.exec(s || "");
  return m ? (+m[1] || 0) * 86400 + (+m[2] || 0) * 3600 + (+m[3] || 0) * 60 + (+m[4] || 0) : 0;
}

/** Enough of an MPEG-DASH manifest to list its video qualities. */
export function parseMpd(text) {
  if (!/<MPD[\s>]/.test(text)) return null;
  const tag = /<MPD\b([^>]*)>/.exec(text);
  const root = tag ? attributes(tag[1].replace(/(\w+)=/g, (_, k) => `${k.toUpperCase()}=`)) : {};
  const seen = new Map();
  for (const m of text.matchAll(/<Representation\b([^>]*)>/g)) {
    const a = attributes(m[1].replace(/(\w+)=/g, (_, k) => `${k.toUpperCase()}=`));
    if (!a.HEIGHT || !a.ID || seen.has(a.ID)) continue;
    const v = { url: a.ID, bandwidth: Number(a.BANDWIDTH) || 0, resolution: `${a.WIDTH || ""}x${a.HEIGHT}` };
    seen.set(a.ID, { ...v, label: variantLabel(v) });
  }
  return {
    variants: [...seen.values()].sort((a, b) => b.bandwidth - a.bandwidth),
    live: root.TYPE === "dynamic",
    duration: isoSeconds(root.MEDIAPRESENTATIONDURATION),
    drm: /<ContentProtection\b/.test(text),
  };
}

async function onManifest(tabId, url, pageUrl) {
  const s = await state(tabId);
  if (s.items.some((e) => e.url === url)) return;
  let text;
  try {
    const res = await fetch(url, { credentials: "include" });
    if (!res.ok) return;
    text = await res.text();
  } catch {
    return;
  }
  const p = parseMpd(text);
  if (!p || p.drm) return;
  const variants = p.variants.length > 1 ? p.variants.map(({ url, label }) => ({ url, label })) : [];
  add(tabId, s, { kind: "dash", url, pageUrl, variants, duration: p.live ? null : p.duration, live: p.live });
  await changed(tabId);
}

function add(tabId, s, entry) {
  if (s.items.some((e) => e.url === entry.url)) return false;
  s.items.unshift({ id: nextId++, time: Date.now(), ...entry });
  s.items.length = Math.min(s.items.length, MAX_PER_TAB);
  return true;
}

async function onPlaylist(tabId, url, pageUrl) {
  const s = await state(tabId);
  if (s.covered.includes(url) || s.items.some((e) => e.url === url)) return;
  let text;
  try {
    const res = await fetch(url, { credentials: "include" });
    if (!res.ok) return;
    text = await res.text();
  } catch {
    return;
  }
  const p = parsePlaylist(text, url);
  if (!p || p.drm) return; // DRM-protected streams can't be saved
  if (p.variants.length) {
    // A master playlist: its qualities and audio tracks are not listed separately.
    const children = [...p.variants.map((v) => v.url), ...p.renditions];
    s.covered.push(...children);
    s.items = s.items.filter((e) => !children.includes(e.url));
    add(tabId, s, { kind: "hls", url, pageUrl, variants: p.variants.map(({ url, label }) => ({ url, label })) });
  } else {
    add(tabId, s, { kind: "hls", url, pageUrl, variants: [], duration: p.ended ? p.duration : null, live: !p.ended });
  }
  await changed(tabId);
}

/** webRequest.onHeadersReceived handler. */
export async function onResponse(details) {
  const { tabId } = details;
  if (tabId < 0) return;
  if (details.type === "main_frame") return resetTab(tabId);
  if (details.statusCode >= 400 || !/^https?:/i.test(details.url)) return;
  const url = new URL(details.url);
  if (IGNORED_HOSTS.test(url.hostname)) return;
  const type = header(details, "content-type").toLowerCase().split(";")[0].trim();
  const path = url.pathname.toLowerCase();
  const pageUrl = details.documentUrl || details.initiator || "";
  if (PLAYLIST_TYPE.test(type) || path.endsWith(".m3u8")) return onPlaylist(tabId, details.url, pageUrl);
  if (MANIFEST_TYPE.test(type) || path.endsWith(".mpd")) return onManifest(tabId, details.url, pageUrl);
  if (type === "video/mp2t" || SEGMENT_EXT.test(path)) return;
  if (!(type.startsWith("video/") || type.startsWith("audio/") || MEDIA_EXT.test(path))) return;
  const size = totalSize(details);
  if (size != null && size < MIN_FILE_BYTES) return;
  const s = await state(tabId);
  if (add(tabId, s, { kind: "file", url: details.url, pageUrl, size, contentType: type })) await changed(tabId);
}
