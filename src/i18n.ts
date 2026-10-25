// Translation of the window. English is written in the code as usual; the
// translator replaces any text that matches a dictionary entry as it
// appears, including templates such as "{0} downloading · {1} queued"
// (whose parts are translated too). File names, addresses and other user
// content are marked translate="no" and left alone.

import { ROWS } from "./translations";

export type Lang = "en" | "ar" | "fr" | "es";
const COLUMNS: Record<Exclude<Lang, "en">, number> = { ar: 1, fr: 2, es: 3 };

let exact = new Map<string, string>();
let templates: { re: RegExp; to: string }[] = [];
const memo = new Map<string, string>();

/** "auto" → the system language if supported, else English. */
export function resolveLang(setting: string): Lang {
  const wanted = (!setting || setting === "auto" ? navigator.language : setting).toLowerCase().split(/[-_]/)[0];
  return (["en", "ar", "fr", "es"] as Lang[]).find((l) => l === wanted) ?? "en";
}

function escapeRe(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function load(lang: Lang) {
  exact = new Map();
  templates = [];
  memo.clear();
  if (lang === "en") return;
  const col = COLUMNS[lang];
  for (const row of ROWS) {
    const [en, to] = [row[0], row[col]];
    if (!to) continue;
    if (/\{\d\}/.test(en)) {
      const pattern = escapeRe(en).replace(/\\\{(\d)\\\}/g, "(?<p$1>.+?)");
      templates.push({ re: new RegExp(`^${pattern}$`, "s"), to });
    } else {
      exact.set(en, to);
    }
  }
  // Specific templates first; the generic "{0} · {1}" splitter last.
  templates.sort((a, b) => b.re.source.replace(/\(\?<p\d>\.\+\?\)/g, "").length - a.re.source.replace(/\(\?<p\d>\.\+\?\)/g, "").length);
}

/** Translate one complete English text (unknown text comes back unchanged). */
export function t(text: string, depth = 0): string {
  if (!text || depth > 12 || (exact.size === 0 && templates.length === 0)) return text;
  const hit = exact.get(text) ?? memo.get(text);
  if (hit !== undefined) return hit;
  let out = text;
  for (const { re, to } of templates) {
    const m = re.exec(text);
    if (!m?.groups) continue;
    const groups = m.groups;
    out = to.replace(/\{(\d)\}/g, (_, n) => t(groups[`p${n}`] ?? "", depth + 1));
    break;
  }
  if (memo.size > 5000) memo.clear();
  memo.set(text, out);
  return out;
}

const SKIP = '[translate="no"], .fname, .h-url, .g-url, .g-name, .batch-list span, input, textarea, script, style';
const ATTRS = ["title", "placeholder", "aria-label"];
/** What we last wrote into a node, so our own change is not translated again. */
const written = new WeakMap<Node, string>();

function translateText(node: Text) {
  if (written.get(node) === node.data) return;
  const parent = node.parentElement;
  if (!parent || parent.closest(SKIP)) return;
  const raw = node.data;
  const trimmed = raw.trim();
  if (!trimmed || !/[A-Za-z]/.test(trimmed)) return;
  const done = t(trimmed);
  if (done !== trimmed) {
    node.data = raw.replace(trimmed, done);
    written.set(node, node.data);
  }
}

function translateAttrs(el: Element) {
  if (el.closest('[translate="no"]')) return;
  for (const name of ATTRS) {
    const v = el.getAttribute(name);
    if (!v || written.get(el) === `${name}=${v}`) continue;
    const done = t(v.trim());
    if (done !== v.trim()) {
      el.setAttribute(name, done);
      written.set(el, `${name}=${done}`);
    }
  }
}

function translateTree(root: Node) {
  if (root.nodeType === Node.TEXT_NODE) return translateText(root as Text);
  if (root.nodeType !== Node.ELEMENT_NODE) return;
  translateAttrs(root as Element);
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if (n.nodeType === Node.TEXT_NODE) translateText(n as Text);
    else translateAttrs(n as Element);
  }
}

/** Pick the language, set the text direction and keep the page translated. */
export function setupI18n(setting: string): Lang {
  const lang = resolveLang(setting);
  load(lang);
  document.documentElement.lang = lang;
  document.documentElement.dir = lang === "ar" ? "rtl" : "ltr";
  if (lang === "en") return lang;
  translateTree(document.body);
  new MutationObserver((records) => {
    for (const r of records) {
      if (r.type === "characterData") translateText(r.target as Text);
      else if (r.type === "attributes") translateAttrs(r.target as Element);
      else r.addedNodes.forEach(translateTree);
    }
  }).observe(document.body, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ATTRS });
  return lang;
}
