import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { setupI18n, t } from "./i18n";

type Status = "queued" | "downloading" | "paused" | "completed" | "error";
type Priority = "high" | "normal" | "low";
type OpenWhenDone = "nothing" | "file" | "folder" | "shutdown" | "sleep" | "command";
type DoneAction = "nothing" | "exit" | "sleep" | "hibernate" | "shutdown" | "run";

interface Item {
  id: number;
  url: string;
  fileName: string;
  savePath: string;
  category: string;
  size: number | null;
  downloaded: number;
  status: Status;
  error: string | null;
  resumable: boolean;
  addedAt: number;
  finishedAt: number | null;
  scheduled: boolean;
  queue: string;
  priority: Priority;
  speedLimitKib: number;
  checksum: string | null;
  openWhenDone: OpenWhenDone;
  note: string | null;
  autoRetries: number;
  stream: boolean;
  live: boolean;
  variant: string | null;
  siteFormat: string | null;
  syncHours: number;
  awaitingRefresh: boolean;
}

interface SegmentView {
  start: number;
  end: number | null;
  written: number;
  active: boolean;
}

interface ProgressEvent {
  id: number;
  total: number | null;
  downloaded: number;
  speed: number;
  connections: number;
  acceptsRanges: boolean;
  segments: SegmentView[];
}

interface ProbeResult {
  finalUrl: string;
  fileName: string;
  size: number | null;
  resumable: boolean;
  contentType: string | null;
  category: string;
  saveDir: string;
  duplicate: { id: number; fileName: string; status: Status } | null;
  freeSpace: number | null;
  stream: StreamInfo | null;
  site: SiteInfo | null;
}

interface SiteInfo {
  title: string;
  site: string;
  duration: number | null;
  live: boolean;
  formats: { id: string; label: string; size: number | null }[];
  ffmpeg: boolean;
}

interface SiteLogin {
  host: string;
  username: string;
  /** Sealed by the app; sent back unchanged unless `newPassword` is set. */
  password: string;
  newPassword?: string | null;
}

interface ToolsStatus {
  ffmpeg: string | null;
  ytDlp: string | null;
  dir: string;
}

interface StreamInfo {
  format: "hls" | "dash";
  container: string;
  variants: { url: string; label: string; bandwidth: number }[];
  live: boolean;
  duration: number | null;
  separateAudio: boolean;
  ffmpeg: boolean;
  estimatedSize: number | null;
}

interface Schedule {
  enabled: boolean;
  start: string;
  stop: string;
  days: number[];
}

interface Queue {
  name: string;
  maxConcurrent: number;
}

interface Settings {
  downloadDir: string;
  connections: number;
  maxConcurrent: number;
  speedLimitKib: number;
  sortByCategory: boolean;
  schedule: Schedule;
  historyRetention: "off" | "onExit" | "day" | "week" | "month" | "forever";
  queues: Queue[];
  hostConnections: { host: string; connections: number }[];
  autoRetry: number;
  scanCommand: string;
  extractArchives: boolean;
  whenDone: DoneAction;
  whenDoneProgram: string;
  clipboardMonitor: boolean;
  startWithWindows: boolean;
  proxyMode: "system" | "none" | "custom";
  proxyUrl: string;
  logins: SiteLogin[];
  quotaMb: number;
  quotaHours: number;
  progressWindows: boolean;
  language: string;
  startCommand: string;
  finishCommand: string;
  defaultWhenDone: OpenWhenDone;
  defaultDoneCommand: string;
  checkUpdates: boolean;
  skippedVersion: string;
}

interface UpdateInfo {
  version: string;
  current: string;
  notes: string;
  page: string;
  asset: string;
  url: string;
  size: number;
  sha256: string;
}

interface AfterUpdate {
  version: string;
  notes: string;
  page: string;
}

interface AppInfo {
  version: string;
  repo: string;
  site: string;
}

interface HistoryEntry {
  url: string;
  fileName: string | null;
  size: number | null;
  checkedAt: number;
}

interface Grabbed {
  files: { url: string; fileName: string }[];
  pages: number;
  truncated: boolean;
}

const DAY_NAMES = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MAIN_QUEUE = "Main";

const CATEGORIES = ["Compressed", "Documents", "Music", "Programs", "Video", "General"];
type View = { id: string; label: string; sub?: boolean; test: (i: Item) => boolean };
const BASE_VIEWS: View[] = [
  { id: "all", label: "All Downloads", test: () => true },
  ...CATEGORIES.map((c) => ({ id: `cat:${c}`, label: c, sub: true, test: (i: Item) => i.category === c })),
  { id: "unfinished", label: "Unfinished", test: (i) => i.status !== "completed" },
  { id: "finished", label: "Finished", test: (i) => i.status === "completed" },
  { id: "queued", label: "Queued", test: (i) => i.status === "queued" },
  { id: "scheduled", label: "Scheduled", test: (i) => i.scheduled && i.status !== "completed" },
];

const items = new Map<number, Item>();
const progress = new Map<number, ProgressEvent>();
/** Recent transfer rates per download, for the speed graph (one sample per progress event). */
const speedHist = new Map<number, number[]>();
const GRAPH_POINTS = 240;
const selected = new Set<number>();
let settings: Settings | null = null;
let view = "all";
let query = "";
let lastClicked: number | null = null;

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

function queues(): Queue[] {
  return settings?.queues ?? [{ name: MAIN_QUEUE, maxConcurrent: 0 }];
}

function views(): View[] {
  return [
    ...BASE_VIEWS,
    ...queues().map((q) => ({ id: `queue:${q.name}`, label: q.name, sub: true, test: (i: Item) => i.queue === q.name })),
  ];
}

// ---------- formatting ----------

function fmtBytes(n: number | null | undefined): string {
  if (n == null) return "Unknown";
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v < 10 ? v.toFixed(2) : v < 100 ? v.toFixed(1) : v.toFixed(0)} ${units[u]}`;
}

function fmtEta(remaining: number, speed: number): string {
  if (speed <= 0 || !isFinite(remaining)) return "";
  let s = Math.round(remaining / speed);
  if (s < 60) return `${s} sec`;
  const h = Math.floor(s / 3600);
  s %= 3600;
  const m = Math.floor(s / 60);
  s %= 60;
  return h ? `${h} h ${m} min` : `${m} min ${s} sec`;
}

function fmtDate(secs: number): string {
  const d = new Date(secs * 1000);
  return d.toLocaleDateString(document.documentElement.lang || undefined, { month: "short", day: "numeric" }) +
    " " + d.toLocaleTimeString(document.documentElement.lang || undefined, { hour: "2-digit", minute: "2-digit" });
}

function timeAgo(secs: number): string {
  const d = Math.max(0, Date.now() / 1000 - secs);
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  const days = Math.floor(d / 86400);
  return days === 1 ? "yesterday" : `${days} days ago`;
}

function extLabel(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1, dot + 4).toUpperCase() : "?";
}

const capitalize = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

/** Every distinct http(s) link in `text`; lines starting with # are comments. */
function linksIn(text: string): string[] {
  const found = new Set<string>();
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    for (const word of t.split(/\s+/)) {
      try {
        const u = new URL(word);
        if ((u.protocol === "http:" || u.protocol === "https:") && u.host) found.add(word);
      } catch {
        /* not a link */
      }
    }
  }
  return [...found];
}

function fileNameOf(url: string): string {
  try {
    const last = new URL(url).pathname.split("/").filter(Boolean).pop();
    return last ? decodeURIComponent(last) : url;
  } catch {
    return url;
  }
}

// ---------- rendering ----------

function visibleItems(): Item[] {
  const v = views().find((x) => x.id === view) ?? BASE_VIEWS[0];
  const q = query.toLowerCase();
  return [...items.values()]
    .filter((i) => v.test(i) && (!q || i.fileName.toLowerCase().includes(q) || i.url.toLowerCase().includes(q)))
    .sort((a, b) => b.id - a.id);
}

function statusInfo(i: Item) {
  const p = progress.get(i.id);
  const total = p?.total ?? i.size;
  const done = i.status === "completed" ? (i.size ?? i.downloaded) : (p?.downloaded ?? i.downloaded);
  const pct = total ? Math.min(100, (done / total) * 100) : i.status === "completed" ? 100 : 0;
  let label: string;
  switch (i.status) {
    case "completed":
      label = i.note ? `Complete · ${i.note}` : "Complete";
      break;
    case "downloading":
      if (i.live) label = i.note ?? (done === 0 ? "Connecting…" : `Recording · ${fmtBytes(done)}`);
      else label = i.note ?? (done === 0 ? "Connecting…" : total ? `${pct.toFixed(1)}%` : `${fmtBytes(done)}`);
      break;
    case "paused":
      if (i.awaitingRefresh) label = "Waiting for a new address";
      else if (i.note) label = i.note;
      else label = i.scheduled ? "Scheduled" : "Paused";
      if (total && done > 0 && !i.awaitingRefresh) label += ` · ${pct.toFixed(1)}%`;
      break;
    case "queued": label = "Queued"; break;
    case "error": label = i.awaitingRefresh ? "Waiting for a new address" : `Error: ${i.error ?? "failed"}`; break;
  }
  const speed = i.status === "downloading" ? (p?.speed ?? 0) : 0;
  return { total, done, pct, label, speed };
}

function renderSidebar() {
  const nav = $("sidebar");
  nav.replaceChildren();
  nav.append(el("div", "cat-group", "Categories"));
  const all = [...items.values()];
  for (const v of views()) {
    if (v.id === "unfinished") nav.append(el("div", "cat-group", "Status"));
    if (v.id === `queue:${queues()[0]?.name}`) nav.append(el("div", "cat-group", "Queues"));
    const b = el("button", `cat${v.sub ? " sub" : ""}${v.id === view ? " active" : ""}`);
    b.append(el("span", "", v.label));
    const queueName = v.id.startsWith("queue:") ? v.id.slice(6) : null;
    if (queueName && all.some((i) => i.queue === queueName && i.status === "downloading")) {
      const dot = el("span", "q-run");
      dot.title = "Downloading";
      b.append(dot);
    }
    b.append(el("span", "count", String(all.filter(v.test).length)));
    b.onclick = () => {
      view = v.id;
      selected.clear();
      scheduleRender();
    };
    if (queueName) {
      b.oncontextmenu = (e) => {
        e.preventDefault();
        queueMenu(e.clientX, e.clientY, queueName);
      };
    }
    nav.append(b);
  }
}

function renderRows() {
  const list = visibleItems();
  const tbody = $("rows");
  tbody.replaceChildren(
    ...list.map((i) => {
      const s = statusInfo(i);
      const tr = el("tr");
      tr.dataset.id = String(i.id);
      if (selected.has(i.id)) tr.classList.add("selected");

      const name = el("td");
      const nc = el("div", "name-cell");
      nc.append(el("span", `ficon ${i.category}`, extLabel(i.fileName)), el("span", "fname", i.fileName));
      name.append(nc);
      name.title = i.fileName;

      const status = el("td");
      const st = el("div", `status ${i.status}${i.status === "downloading" && s.done === 0 ? " connecting" : ""}`);
      st.append(el("span", "label", s.label));
      if (i.status !== "completed") {
        const bar = el("div", "bar");
        const fill = el("i");
        fill.style.width = `${s.pct}%`;
        bar.append(fill);
        st.append(bar);
      }
      status.append(st);
      status.title = i.error ?? i.note ?? "";

      tr.append(
        name,
        el("td", "num", fmtBytes(s.total)),
        status,
        el("td", "num", i.status === "downloading" && s.total ? fmtEta(s.total - s.done, s.speed) : ""),
        el("td", "num", i.status === "downloading" ? `${fmtBytes(s.speed)}/s` : ""),
        el("td", "num", fmtDate(i.addedAt)),
      );
      return tr;
    }),
  );
  $("empty").hidden = list.length > 0;
}

function renderDetails() {
  const panel = $("details");
  const id = selected.size === 1 ? [...selected][0] : null;
  const i = id != null ? items.get(id) : undefined;
  panel.hidden = !i;
  if (!i) return;
  const s = statusInfo(i);
  const p = progress.get(i.id);
  $("d-name").textContent = i.fileName;
  $("d-url").textContent = i.url;
  $("d-url").title = i.url;
  $("d-path").textContent = i.savePath;
  $("d-path").title = i.savePath;
  $("d-done").textContent = i.status === "downloading" && s.done === 0 ? "Connecting…" : s.total ? `${fmtBytes(s.done)} of ${fmtBytes(s.total)} (${s.pct.toFixed(1)}%)` : fmtBytes(s.done);
  $("d-conns").textContent = i.status === "downloading" && p ? String(p.connections) : "—";
  $("d-resume").textContent = i.live
    ? "Live stream: Stop ends the recording and saves it"
    : i.resumable
      ? "Supported"
      : "Not supported (download restarts if stopped)";
  $("d-queue").textContent = `${i.queue} · ${capitalize(i.priority)} priority`;
  $("d-limit").textContent = i.speedLimitKib ? `${fmtBytes(i.speedLimitKib * 1024)}/s` : "None";
  // The field is marked untranslatable for the checksum itself.
  $("d-sum").textContent = i.checksum ?? t("Not checked");
  $("d-sum").title = i.checksum ?? "";
  $("d-note").textContent = i.note ?? "—";
  $("d-note").title = i.note ?? "";
  $<HTMLButtonElement>("d-open").disabled = i.status !== "completed";

  const bar = $("d-segbar");
  bar.replaceChildren();
  const total = s.total;
  if (i.status === "completed") {
    const seg = el("div", "seg done");
    seg.style.left = "0";
    seg.style.width = "100%";
    bar.append(seg);
  } else if (p && total) {
    for (const g of p.segments) {
      const seg = el("div", `seg${g.active ? " active" : ""}`);
      seg.style.left = `${(g.start / total) * 100}%`;
      seg.style.width = `${(g.written / total) * 100}%`;
      bar.append(seg);
      if (g.start > 0) {
        const edge = el("div", "edge");
        edge.style.left = `${(g.start / total) * 100}%`;
        bar.append(edge);
      }
    }
  } else if (total) {
    const seg = el("div", "seg");
    seg.style.left = "0";
    seg.style.width = `${s.pct}%`;
    bar.append(seg);
  }
  drawGraph(i.id);
}

/** Line chart of the selected download's transfer rate; newest sample on the right. */
function drawGraph(id: number) {
  const canvas = $<HTMLCanvasElement>("d-graph");
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (!w || !h) return;
  const dpr = devicePixelRatio || 1;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  const ctx = canvas.getContext("2d")!;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const css = getComputedStyle(document.documentElement);
  const accent = css.getPropertyValue("--accent").trim();
  ctx.strokeStyle = css.getPropertyValue("--border").trim();
  ctx.lineWidth = 1;
  const top = 20;
  const yAt = (v: number, max: number) => h - 2 - (v / max) * (h - top - 2);
  ctx.beginPath();
  ctx.moveTo(0, Math.round(yAt(0.5, 1)) + 0.5);
  ctx.lineTo(w, Math.round(yAt(0.5, 1)) + 0.5);
  ctx.stroke();

  const data = speedHist.get(id) ?? [];
  const label = $("d-graph-label");
  if (data.length < 2) {
    label.textContent = data.length ? `${fmtBytes(data[0])}/s` : "No speed data yet";
    return;
  }
  const max = Math.max(1, ...data);
  const step = w / (GRAPH_POINTS - 1);
  const x0 = w - (data.length - 1) * step;
  ctx.beginPath();
  data.forEach((v, k) => (k ? ctx.lineTo(x0 + k * step, yAt(v, max)) : ctx.moveTo(x0, yAt(v, max))));
  ctx.strokeStyle = accent;
  ctx.lineWidth = 1.5;
  ctx.stroke();
  ctx.lineTo(w, h);
  ctx.lineTo(x0, h);
  ctx.closePath();
  ctx.globalAlpha = 0.16;
  ctx.fillStyle = accent;
  ctx.fill();
  ctx.globalAlpha = 1;
  label.textContent = `${fmtBytes(data[data.length - 1])}/s · peak ${fmtBytes(max)}/s`;
}

function renderToolbar() {
  const sel = [...selected].map((id) => items.get(id)).filter((i): i is Item => !!i);
  $<HTMLButtonElement>("btn-resume").disabled = !sel.some((i) => i.status === "paused" || i.status === "error");
  $<HTMLButtonElement>("btn-pause").disabled = !sel.some((i) => i.status === "downloading" || i.status === "queued");
  $<HTMLButtonElement>("btn-delete").disabled = sel.length === 0;
  $<HTMLButtonElement>("btn-folder").disabled = sel.length !== 1;
  $<HTMLButtonElement>("btn-pause-all").disabled = ![...items.values()].some((i) => i.status === "downloading" || i.status === "queued");
  $<HTMLButtonElement>("btn-clear").disabled = ![...items.values()].some((i) => i.status === "completed");
}

const DONE_LABELS: Record<DoneAction, string> = {
  nothing: "",
  exit: "Quit Bright Downloader",
  sleep: "Sleep",
  hibernate: "Hibernate",
  shutdown: "Shut down",
  run: "Run program",
};

function renderStatusbar() {
  const active = [...items.values()].filter((i) => i.status === "downloading");
  const speed = active.reduce((sum, i) => sum + (progress.get(i.id)?.speed ?? 0), 0);
  $("sb-active").textContent = active.length
    ? `${active.length} downloading · ${[...items.values()].filter((i) => i.status === "queued").length} queued`
    : "No active downloads";
  $("sb-speed").textContent = active.length ? `${fmtBytes(speed)}/s` : "";
  const done = settings?.whenDone ?? "nothing";
  $("sb-done").textContent = done === "nothing" ? "" : `When all finish: ${DONE_LABELS[done]}`;
}

let renderPending = false;
function scheduleRender() {
  if (renderPending) return;
  renderPending = true;
  requestAnimationFrame(() => {
    renderPending = false;
    renderSidebar();
    renderRows();
    renderDetails();
    renderToolbar();
    renderStatusbar();
  });
}

// ---------- toast ----------

let toastTimer = 0;

function showToast(text: string, action?: { label: string; run: () => unknown }) {
  const toast = $("toast");
  $("toast-text").textContent = text;
  $("toast-text").title = text;
  const go = $<HTMLButtonElement>("toast-go");
  go.hidden = !action;
  if (action) {
    go.textContent = action.label;
    go.onclick = () => {
      toast.hidden = true;
      action.run();
    };
  }
  $("toast-close").textContent = action ? "Dismiss" : "OK";
  toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (toast.hidden = true), action ? 20_000 : 5_000);
}

// ---------- actions ----------

async function act(cmd: string, filter: (i: Item) => boolean, ids: Iterable<number> = selected) {
  for (const id of [...ids]) {
    const i = items.get(id);
    if (i && filter(i)) await invoke(cmd, { id });
  }
}

const resumeSelected = () => act("resume_download", (i) => i.status === "paused" || i.status === "error");
const pauseSelected = () =>
  act("pause_download", (i) => i.status === "downloading" || i.status === "queued" || (i.status === "error" && i.autoRetries > 0));

function askDelete(ids: number[]) {
  if (!ids.length) return;
  const dlg = $<HTMLDialogElement>("dlg-delete");
  $("del-text").textContent =
    ids.length === 1 ? `Remove "${items.get(ids[0])?.fileName}" from the list?` : `Remove ${ids.length} downloads from the list?`;
  $<HTMLInputElement>("del-files").checked = false;
  dlg.returnValue = "";
  dlg.onclose = async () => {
    if (dlg.returnValue !== "ok") return;
    const deleteFile = $<HTMLInputElement>("del-files").checked;
    for (const id of ids) await invoke("remove_download", { id, deleteFile });
  };
  dlg.showModal();
}

// ---------- context menu ----------

type MenuItem = {
  label: string;
  run?: () => unknown;
  enabled?: boolean;
  danger?: boolean;
  /** Shows a check mark column; true = ticked. */
  checked?: boolean;
  sub?: MenuEntry[];
};
type MenuEntry = MenuItem | "sep";

function hideMenu() {
  $("ctx").hidden = true;
  $("ctx-sub").hidden = true;
}

function fillMenu(menu: HTMLElement, entries: MenuEntry[], top: boolean) {
  menu.replaceChildren(
    ...entries.map((e) => {
      if (e === "sep") return el("hr");
      const cls = [e.danger && "danger", e.sub && "has-sub", e.checked === true && "checked", e.checked === false && "unchecked"];
      const b = el("button", cls.filter(Boolean).join(" "), e.label);
      b.disabled = e.enabled === false || (!!e.sub && e.sub.length === 0);
      if (top) {
        b.onmouseenter = () => (e.sub && !b.disabled ? showSubmenu(b, e.sub) : ($("ctx-sub").hidden = true));
      }
      b.onclick = () => {
        if (e.sub) return showSubmenu(b, e.sub);
        hideMenu();
        e.run?.();
      };
      return b;
    }),
  );
}

function showMenu(x: number, y: number, entries: MenuEntry[]) {
  const menu = $("ctx");
  $("ctx-sub").hidden = true;
  fillMenu(menu, entries, true);
  menu.hidden = false;
  // Keep the menu inside the window.
  const { width, height } = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(4, Math.min(x, innerWidth - width - 4))}px`;
  menu.style.top = `${Math.max(4, Math.min(y, innerHeight - height - 4))}px`;
}

function showSubmenu(anchor: HTMLElement, entries: MenuEntry[]) {
  const sub = $("ctx-sub");
  fillMenu(sub, entries, false);
  sub.hidden = false;
  const r = anchor.getBoundingClientRect();
  const { width, height } = sub.getBoundingClientRect();
  const x = r.right + width + 6 < innerWidth ? r.right + 2 : r.left - width - 2;
  sub.style.left = `${Math.max(4, x)}px`;
  sub.style.top = `${Math.max(4, Math.min(r.top - 4, innerHeight - height - 4))}px`;
}

function rowMenu(x: number, y: number) {
  const sel = [...selected].map((id) => items.get(id)).filter((i): i is Item => !!i);
  if (!sel.length) return;
  const one = sel.length === 1 ? sel[0] : null;
  const ids = sel.map((i) => i.id);
  const running = (i: Item) => i.status === "downloading" || i.status === "queued";
  const priorities: Priority[] = ["high", "normal", "low"];
  showMenu(x, y, [
    { label: "Open", enabled: one?.status === "completed", run: () => invoke("open_file", { id: one!.id }) },
    { label: "Show in folder", enabled: !!one, run: () => invoke("show_in_folder", { id: one!.id }) },
    "sep",
    { label: "Resume", enabled: sel.some((i) => i.status === "paused" || i.status === "error"), run: resumeSelected },
    { label: "Stop", enabled: sel.some(running), run: pauseSelected },
    "sep",
    {
      label: "Priority",
      enabled: sel.some((i) => i.status !== "completed"),
      sub: priorities.map((p) => ({
        label: capitalize(p),
        checked: sel.every((i) => i.priority === p),
        run: () => invoke("set_priority", { ids, priority: p }),
      })),
    },
    {
      label: "Move to queue",
      enabled: queues().length > 1,
      sub: queues().map((q) => ({
        label: q.name,
        checked: sel.every((i) => i.queue === q.name),
        run: () => invoke("set_queue", { ids, queue: q.name }).catch((e) => showToast(String(e))),
      })),
    },
    { label: "Speed limit…", enabled: !!one && one.status !== "completed", run: () => openLimit(one!) },
    { label: "Verify checksum…", enabled: one?.status === "completed", run: () => openHash(one!) },
    {
      label: "Keep in sync…",
      enabled: sel.every((i) => i.status === "completed" && !i.stream && !i.siteFormat),
      run: () => openSync(sel),
    },
    { label: "Progress window", enabled: !!one && one.status !== "completed", run: () => invoke("open_progress_window", { id: one!.id }) },
    "sep",
    { label: "Copy address", enabled: !!one, run: () => navigator.clipboard.writeText(one!.url) },
    {
      label: "Change address…",
      enabled: !!one && one.status !== "completed" && !running(one),
      run: () => openChangeUrl(one!),
    },
    {
      label: "Get a new address from the browser…",
      enabled: !!one && one.status !== "completed" && !running(one) && !one.siteFormat,
      run: () =>
        invoke("refresh_address", { id: one!.id })
          .then(() => showToast("Start the download again in your browser; Bright Downloader will take the new address and continue."))
          .catch((e) => showToast(String(e))),
    },
    "sep",
    { label: "Delete…", danger: true, run: () => askDelete(ids) },
  ]);
}

function queueMenu(x: number, y: number, name: string) {
  const inQueue = [...items.values()].filter((i) => i.queue === name);
  showMenu(x, y, [
    {
      label: "Start queue",
      enabled: inQueue.some((i) => i.status === "paused" || i.status === "error"),
      run: () => invoke("start_queue", { queue: name }),
    },
    {
      label: "Stop queue",
      enabled: inQueue.some((i) => i.status === "downloading" || i.status === "queued"),
      run: () => invoke("stop_queue", { queue: name }),
    },
    "sep",
    { label: "Edit queues…", run: () => openSettings("queues") },
  ]);
}

function linksMenu() {
  const r = $("btn-links").getBoundingClientRect();
  const sel = [...selected];
  showMenu(r.left, r.bottom + 2, [
    { label: "Import links from a file…", run: importLinks },
    { label: "Paste a list of links…", run: () => openBatchList([], "Add links") },
    "sep",
    { label: `Export selected links (${sel.length})…`, enabled: sel.length > 0, run: () => exportLinks(sel) },
    { label: "Export all links…", enabled: items.size > 0, run: () => exportLinks([]) },
  ]);
}

// ---------- help & updates ----------

function helpMenu() {
  const r = $("btn-help").getBoundingClientRect();
  showMenu(r.left, r.bottom + 2, [
    { label: "Check for updates…", run: () => checkForUpdates() },
    { label: "What's new in each version", run: () => invoke("open_help", { page: "releases" }) },
    "sep",
    { label: "Bright Downloader website", run: () => invoke("open_help", { page: "site" }) },
    { label: "Source code on GitHub", run: () => invoke("open_help", { page: "repo" }) },
    { label: "Report a problem", run: () => invoke("open_help", { page: "issues" }) },
    "sep",
    { label: "About Bright Downloader", run: openAbout },
  ]);
}

async function openAbout() {
  const info = await invoke<AppInfo>("app_info");
  $("about-version").textContent = `Version ${info.version}`;
  $("about-repo").textContent = info.repo;
  $("about-repo-btn").onclick = () => invoke("open_help", { page: "repo" });
  $<HTMLDialogElement>("dlg-about").showModal();
}

/** Release notes are Markdown; show headings, lists and paragraphs as plain text. */
function renderNotes(box: HTMLElement, markdown: string) {
  box.replaceChildren();
  let list: HTMLUListElement | null = null;
  const clean = (line: string) => line.replace(/\*\*(.+?)\*\*|__(.+?)__/g, "$1$2").replace(/`([^`]+)`/g, "$1").replace(/\[([^\]]+)\]\([^)]+\)/g, "$1");
  for (const raw of markdown.replace(/<!--[\s\S]*?-->/g, "").split(/\r?\n/)) {
    const line = raw.trim();
    const bullet = /^[-*+]\s+(.*)$/.exec(line);
    if (bullet) {
      if (!list) box.append((list = document.createElement("ul")));
      const li = document.createElement("li");
      li.textContent = clean(bullet[1]);
      list.append(li);
      continue;
    }
    list = null;
    if (!line) continue;
    const heading = /^#{1,6}\s+(.*)$/.exec(line);
    // The dialog has its own "What's new" title.
    if (heading && !box.childElementCount && /^what['’]?s new$/i.test(heading[1].trim())) continue;
    const el = document.createElement(heading ? "h4" : "p");
    el.textContent = clean(heading ? heading[1] : line);
    box.append(el);
  }
  if (!box.childElementCount) {
    const p = document.createElement("p");
    p.textContent = "Bug fixes and improvements.";
    box.append(p);
  }
}

let updating = false;

function showUpdate(info: UpdateInfo) {
  const dlg = $<HTMLDialogElement>("dlg-update");
  if (dlg.open || updating) return;
  document.querySelectorAll<HTMLDialogElement>("dialog[open]").forEach((d) => d.close());
  $("upd-text").textContent = `Bright Downloader ${info.version} is ready to install. You have ${info.current}.`;
  renderNotes($("upd-notes"), info.notes);
  $("upd-progress").hidden = true;
  $("upd-error").textContent = "";
  $("upd-how").hidden = false;
  const buttons = ["upd-skip", "upd-later", "upd-go"].map((id) => $<HTMLButtonElement>(id));
  buttons.forEach((b) => (b.disabled = false));
  $("upd-go").textContent = "Update now";
  $("upd-later").onclick = () => dlg.close();
  $("upd-skip").onclick = () => {
    invoke("skip_update", { version: info.version });
    if (settings) settings.skippedVersion = info.version;
    dlg.close();
  };
  // Closing (Esc) is not possible while the update is being installed.
  dlg.oncancel = (e) => updating && e.preventDefault();
  $("form-update").onsubmit = async (e) => {
    e.preventDefault();
    updating = true;
    buttons.forEach((b) => (b.disabled = true));
    $("upd-how").hidden = true;
    $("upd-error").textContent = "";
    $("upd-progress").hidden = false;
    $("upd-bar").style.width = "0%";
    $("upd-status").textContent = "Downloading the update…";
    try {
      // On success the app closes and the installer takes over.
      await invoke("install_update", { version: info.version });
    } catch (err) {
      updating = false;
      buttons.forEach((b) => (b.disabled = false));
      $("upd-go").textContent = "Try again";
      $("upd-progress").hidden = true;
      $("upd-error").textContent = String(err);
    }
  };
  dlg.showModal();
  $("upd-go").focus();
}

async function checkForUpdates() {
  showToast("Checking for updates…");
  try {
    const info = await invoke<UpdateInfo | null>("check_update");
    if (info) {
      $("toast").hidden = true;
      showUpdate(info);
    } else {
      const { version } = await invoke<AppInfo>("app_info");
      showToast(`You have the latest version (${version}).`);
    }
  } catch (e) {
    showToast(`Could not check for updates: ${e}`);
  }
}

async function showWhatsNew() {
  const after = await invoke<AfterUpdate | null>("take_update_notes").catch(() => null);
  if (!after) return;
  $("new-title").textContent = `Bright Downloader was updated to ${after.version}`;
  renderNotes($("new-notes"), after.notes);
  $("new-page").onclick = () => invoke("open_help", { page: "releases" });
  $<HTMLDialogElement>("dlg-whatsnew").showModal();
}

function openChangeUrl(item: Item) {
  const dlg = $<HTMLDialogElement>("dlg-url");
  const input = $<HTMLInputElement>("url-new");
  input.value = item.url;
  $("url-error").textContent = "";
  $("form-url").onsubmit = async (e) => {
    e.preventDefault();
    try {
      const updated = await invoke<Item>("set_download_url", { id: item.id, url: input.value.trim() });
      items.set(updated.id, updated);
      dlg.close();
      scheduleRender();
    } catch (err) {
      $("url-error").textContent = String(err);
    }
  };
  dlg.showModal();
  input.select();
}

function openSync(sel: Item[]) {
  const dlg = $<HTMLDialogElement>("dlg-sync");
  const hours = $<HTMLSelectElement>("sync-hours");
  hours.value = String(sel[0]?.syncHours ?? 0);
  if (!hours.value) hours.value = "0";
  dlg.returnValue = "";
  dlg.onclose = () => {
    if (dlg.returnValue === "ok") invoke("set_sync", { ids: sel.map((i) => i.id), hours: Number(hours.value) });
  };
  dlg.showModal();
}

function openLimit(item: Item) {
  const dlg = $<HTMLDialogElement>("dlg-limit");
  const input = $<HTMLInputElement>("limit-kib");
  $("limit-name").textContent = item.fileName;
  input.value = String(item.speedLimitKib);
  dlg.returnValue = "";
  dlg.onclose = () => {
    if (dlg.returnValue !== "ok") return;
    const kib = Math.max(0, Math.round(Number(input.value)) || 0);
    invoke("set_speed_limit", { id: item.id, kib });
  };
  dlg.showModal();
  input.select();
}

function openHash(item: Item) {
  const dlg = $<HTMLDialogElement>("dlg-hash");
  const algo = $<HTMLSelectElement>("hash-algo");
  const out = $<HTMLInputElement>("hash-out");
  const expect = $<HTMLInputElement>("hash-expect");
  const result = $("hash-result");
  const go = $<HTMLButtonElement>("hash-go");
  $("hash-name").textContent = item.fileName;
  expect.value = item.checksum ?? "";
  algo.value = expect.value.length === 32 ? "md5" : "sha256";
  out.value = "";
  result.textContent = "";
  const compare = () => {
    const want = expect.value.trim().toLowerCase();
    if (!want || !out.value) {
      result.textContent = "";
      return;
    }
    const match = want === out.value.toLowerCase();
    result.textContent = match ? "✓ The checksums match. The file is intact." : "✗ The checksums do not match.";
    result.style.color = match ? "var(--ok)" : "var(--err)";
  };
  expect.oninput = () => {
    const len = expect.value.trim().length;
    if (len === 32 || len === 64) {
      const want = len === 32 ? "md5" : "sha256";
      if (algo.value !== want) {
        algo.value = want;
        out.value = "";
      }
    }
    compare();
  };
  algo.onchange = () => {
    out.value = "";
    compare();
  };
  $("form-hash").onsubmit = async (e) => {
    e.preventDefault();
    go.disabled = true;
    go.textContent = "Calculating…";
    result.textContent = "";
    try {
      out.value = await invoke<string>("compute_checksum", { id: item.id, algo: algo.value });
      compare();
    } catch (err) {
      result.textContent = String(err);
      result.style.color = "var(--err)";
    } finally {
      go.disabled = false;
      go.textContent = "Calculate";
    }
  };
  $("hash-close").onclick = () => dlg.close();
  dlg.showModal();
}

// ---------- link history ----------

async function loadHistory() {
  const entries = await invoke<HistoryEntry[]>("list_history").catch(() => [] as HistoryEntry[]);
  const datalist = $("add-url-list");
  datalist.replaceChildren(
    ...entries.map((e) => {
      const o = el("option");
      o.value = e.url;
      if (e.fileName) o.label = e.fileName;
      return o;
    }),
  );
  const list = $("hist-list");
  list.replaceChildren(
    ...entries.map((e) => {
      const li = el("li");
      li.title = e.url;
      const main = el("div", "h-main");
      let host = e.url;
      try {
        host = new URL(e.url).host;
      } catch {
        /* keep raw text */
      }
      const name = e.fileName ?? host;
      main.append(el("div", "h-name", e.size != null ? `${name} · ${fmtBytes(e.size)}` : name), el("div", "h-url", e.url));
      const del = el("button", "h-del", "✕");
      del.type = "button";
      del.title = "Remove from history";
      del.onclick = async (ev) => {
        ev.stopPropagation();
        await invoke("remove_history", { url: e.url });
        loadHistory();
      };
      li.append(main, el("span", "h-time", timeAgo(e.checkedAt)), del);
      li.onclick = () => {
        $<HTMLInputElement>("add-url").value = e.url;
        resetAddDialogKeepUrl();
        toggleHistory(false);
        probeLink();
      };
      return li;
    }),
  );
  $("hist-empty").hidden = entries.length > 0;
  $("hist-clear").hidden = entries.length === 0;
}

function toggleHistory(show?: boolean) {
  const panel = $("add-history");
  const open = show ?? panel.hidden;
  panel.hidden = !open;
  $("add-hist-toggle").setAttribute("aria-expanded", String(open));
  if (open) loadHistory();
}

// ---------- add dialog ----------

let probed: ProbeResult | null = null;
/** Cookies / referrer / user agent from the browser for a captured link. */
let pendingHeaders: [string, string][] = [];

interface Captured {
  url: string;
  fileName: string | null;
  headers: [string, string][];
  /** Quality picked on the page for an HLS stream. */
  variant: string | null;
}

function fillQueueSelect(select: HTMLSelectElement, current = MAIN_QUEUE) {
  select.replaceChildren(
    ...queues().map((q) => {
      const o = el("option", "", q.name);
      o.value = q.name;
      return o;
    }),
  );
  select.value = queues().some((q) => q.name === current) ? current : MAIN_QUEUE;
}

/** Links from the browser / command line not shown yet (one Add dialog at a time). */
const capturedQueue: Captured[] = [];

async function takeCaptured() {
  capturedQueue.push(...(await invoke<Captured[]>("take_pending").catch(() => [] as Captured[])));
  if (!$<HTMLDialogElement>("dlg-add").open) showNextCaptured();
}

function showNextCaptured() {
  const next = capturedQueue.shift();
  if (next) openCaptured(next);
}

async function openCaptured(c: Captured) {
  const dlg = $<HTMLDialogElement>("dlg-add");
  if (dlg.open) dlg.close();
  resetAddDialog();
  $<HTMLInputElement>("add-url").value = c.url;
  pendingHeaders = c.headers;
  dlg.showModal();
  await probeLink();
  if (probed && c.fileName) {
    // Streams keep the container the app can produce (.mp4 with ffmpeg, else .ts).
    const ext = probed.stream || probed.site ? probed.fileName.slice(probed.fileName.lastIndexOf(".")) : "";
    $<HTMLInputElement>("add-name").value = ext ? c.fileName.replace(/\.[^.\\/]*$/, "") + ext : c.fileName;
  }
  if ((probed?.stream || probed?.site) && c.variant) {
    const q = $<HTMLSelectElement>("add-quality");
    if ([...q.options].some((o) => o.value === c.variant)) q.value = c.variant;
  }
}

/** Open the Add dialog for `url` and check it right away (drop, paste, clipboard offer). */
async function openAddWith(url: string) {
  const dlg = $<HTMLDialogElement>("dlg-add");
  if (dlg.open) dlg.close();
  resetAddDialog();
  $<HTMLInputElement>("add-url").value = url;
  dlg.showModal();
  await probeLink();
}

function resetAddDialog() {
  probed = null;
  pendingHeaders = [];
  toggleHistory(false);
  loadHistory();
  $<HTMLInputElement>("add-url").value = "";
  $("add-info").hidden = true;
  $("add-later").hidden = true;
  $("add-error").textContent = "";
  $("add-go").textContent = "Check link";
  renderStreamInfo(null);
  fillQueueSelect($<HTMLSelectElement>("add-queue"));
  $<HTMLSelectElement>("add-open").value = settings?.defaultWhenDone ?? "nothing";
  $<HTMLInputElement>("add-cmd").value = settings?.defaultDoneCommand ?? "";
  $("add-cmd-field").hidden = $<HTMLSelectElement>("add-open").value !== "command";
  $<HTMLInputElement>("add-sum").value = "";
  $<HTMLInputElement>("add-limit").value = "0";
  $<HTMLSelectElement>("add-priority").value = "normal";
  $<HTMLDetailsElement>("add-info").querySelector("details")!.open = false;
}

async function openAddDialog() {
  resetAddDialog();
  $<HTMLDialogElement>("dlg-add").showModal();
  const input = $<HTMLInputElement>("add-url");
  input.focus();
  // Like IDM: pre-fill a link copied to the clipboard. Selected, so typing replaces it.
  const link = await invoke<string | null>("clipboard_url").catch(() => null);
  if (link && !input.value) {
    input.value = link;
    input.select();
  }
}

function fmtDuration(secs: number): string {
  const s = Math.round(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h ? `${h} h ${m} min` : m ? `${m} min ${s % 60} sec` : `${s} sec`;
}

/** Quality picker and notes for HLS streams. */
function renderStreamInfo(info: StreamInfo | null) {
  const field = $("add-quality-field");
  const note = $("add-stream-note");
  field.hidden = !info || info.variants.length < 2;
  note.hidden = !info;
  if (!info) return;
  $<HTMLSelectElement>("add-quality").replaceChildren(
    ...info.variants.map((v) => {
      const o = el("option", "", v.label);
      o.value = v.url;
      return o;
    }),
  );
  const parts = [info.live ? "Live stream: it is recorded until you press Stop, then saved." : `Video stream${info.duration ? ` · ${fmtDuration(info.duration)}` : ""}.`];
  if (!info.ffmpeg) {
    parts.push(
      info.separateAudio
        ? "Install ffmpeg to save it as MP4 with sound; without it the audio is saved as a separate file."
        : "Saved as .ts (install ffmpeg to get .mp4).",
    );
  }
  note.textContent = parts.map((p) => t(p)).join(" ");
}

/** Quality choices and notes for a video-site page (yt-dlp). */
function renderSiteInfo(info: SiteInfo | null) {
  if (!info) return;
  const field = $("add-quality-field");
  const note = $("add-stream-note");
  field.hidden = info.formats.length < 2;
  note.hidden = false;
  $<HTMLSelectElement>("add-quality").replaceChildren(
    ...info.formats.map((f) => {
      const o = el("option", "", f.size ? `${f.label} · ${fmtBytes(f.size)}` : f.label);
      o.value = f.id;
      return o;
    }),
  );
  const parts = [
    info.live
      ? "Live stream: it is recorded until you press Stop, then saved."
      : `Video page${info.site ? ` (${info.site})` : ""}${info.duration ? ` · ${fmtDuration(info.duration)}` : ""}.`,
  ];
  if (!info.ffmpeg) parts.push("Get ffmpeg in Options → Video for the best quality (it joins video and audio).");
  note.textContent = parts.map((p) => t(p)).join(" ");
}

/** Warn when the file will not fit in the chosen folder. */
function renderSpace(free: number | null) {
  const warn = $("add-space");
  const size = probed?.size;
  const short = size != null && free != null && size > free;
  warn.hidden = !short;
  if (short) warn.textContent = `Not enough free space: the file needs ${fmtBytes(size)}, only ${fmtBytes(free)} is free in this folder.`;
}

async function probeLink() {
  const url = $<HTMLInputElement>("add-url").value.trim();
  const go = $<HTMLButtonElement>("add-go");
  $("add-error").textContent = "";
  go.disabled = true;
  go.textContent = "Checking…";
  try {
    probed = await invoke<ProbeResult>("probe_url", { url, headers: pendingHeaders });
    $<HTMLInputElement>("add-name").value = probed.fileName;
    $<HTMLInputElement>("add-dir").value = probed.saveDir;
    $("add-meta").textContent = [
      probed.stream || probed.site ? (probed.size ? `Size: about ${fmtBytes(probed.size)}` : "") : `Size: ${fmtBytes(probed.size)}`,
      probed.site ? probed.site.site || "Video" : probed.stream ? probed.stream.format.toUpperCase() : probed.resumable ? "Resume supported" : "Resume not supported",
      probed.category,
      probed.freeSpace != null ? `${fmtBytes(probed.freeSpace)} free` : "",
    ].filter(Boolean).join(" · ");
    renderStreamInfo(probed.stream);
    renderSiteInfo(probed.site);
    const dup = $("add-dup");
    dup.hidden = !probed.duplicate;
    if (probed.duplicate) {
      const d = probed.duplicate;
      const state = d.status === "completed" ? "already downloaded" : `already in the list (${d.status})`;
      dup.textContent = `This link is ${state} as "${d.fileName}". Adding it again downloads a second copy.`;
    }
    renderSpace(probed.freeSpace);
    $("add-info").hidden = false;
    $("add-later").hidden = false;
    toggleHistory(false);
    go.textContent = "Start Download";
  } catch (e) {
    $("add-error").textContent = String(e);
    go.textContent = "Check link";
  } finally {
    go.disabled = false;
  }
}

async function submitAdd(start: boolean, scheduled = false) {
  if (!probed) return probeLink();
  if ($<HTMLSelectElement>("add-open").value === "command" && !$<HTMLInputElement>("add-cmd").value.trim()) {
    $("add-error").textContent = t("Enter the command to run when the download is done.");
    $<HTMLInputElement>("add-cmd").focus();
    return;
  }
  const request = {
    url: $<HTMLInputElement>("add-url").value.trim(),
    fileName: $<HTMLInputElement>("add-name").value.trim() || null,
    saveDir: $<HTMLInputElement>("add-dir").value.trim() || null,
    headers: pendingHeaders,
    start,
    scheduled,
    // A video site's size depends on the chosen quality.
    size: probed.site?.formats.find((f) => f.id === $<HTMLSelectElement>("add-quality").value)?.size ?? probed.size,
    resumable: probed.resumable,
    queue: $<HTMLSelectElement>("add-queue").value,
    openWhenDone: $<HTMLSelectElement>("add-open").value as OpenWhenDone,
    checksum: $<HTMLInputElement>("add-sum").value.trim() || null,
    doneCommand: $<HTMLInputElement>("add-cmd").value.trim() || null,
    speedLimitKib: Math.min(10_000_000, Math.max(0, Math.round(Number($<HTMLInputElement>("add-limit").value)) || 0)),
    priority: $<HTMLSelectElement>("add-priority").value as Priority,
    stream: !!probed.stream,
    live: !!(probed.stream?.live || probed.site?.live),
    variant: probed.stream && !$("add-quality-field").hidden ? $<HTMLSelectElement>("add-quality").value : null,
    siteFormat: probed.site ? ($("add-quality-field").hidden ? (probed.site.formats[0]?.id ?? "bv*+ba/b") : $<HTMLSelectElement>("add-quality").value) : null,
  };
  try {
    const item = await invoke<Item>("add_download", { request });
    // The "item" events (e.g. queued -> downloading) can arrive before this
    // reply, so don't overwrite them with the older snapshot.
    if (!items.has(item.id)) items.set(item.id, item);
    selected.clear();
    selected.add(item.id);
    $<HTMLDialogElement>("dlg-add").close();
    scheduleRender();
  } catch (e) {
    $("add-error").textContent = String(e);
  }
}

async function browseInto(inputId: string) {
  const input = $<HTMLInputElement>(inputId);
  const dir = await openDialog({ directory: true, defaultPath: input.value || undefined });
  if (typeof dir === "string") {
    input.value = dir;
    input.dispatchEvent(new Event("change"));
  }
}

// ---------- batch dialog ----------

const BATCH_MAX = 1000;
/** `[1-20]`, `[001-100]` or `[a-z]` inside a batch address. */
const RANGE = /\[(\d+)-(\d+)\]|\[([a-zA-Z])-([a-zA-Z])\]/;
let batchMode: "pattern" | "list" = "pattern";

function expandRange(pattern: string, m: RegExpMatchArray): { urls: string[]; error: string } {
  const values: string[] = [];
  if (m[1] != null) {
    const [a, b] = [parseInt(m[1], 10), parseInt(m[2], 10)];
    if (b < a) return { urls: [], error: "In [from-to] the first number must not be larger than the second." };
    if (b - a + 1 > BATCH_MAX) return { urls: [], error: `At most ${BATCH_MAX} links at once.` };
    // A leading zero ("001") asks for zero-padded numbers of that width.
    const pad = m[1].length > 1 && m[1].startsWith("0") ? m[1].length : 0;
    for (let n = a; n <= b; n++) values.push(String(n).padStart(pad, "0"));
  } else {
    const [a, b] = [m[3].charCodeAt(0), m[4].charCodeAt(0)];
    const sameCase = (m[3] === m[3].toLowerCase()) === (m[4] === m[4].toLowerCase());
    if (!sameCase || b < a) return { urls: [], error: "Use a letter range like [a-z] or [A-F]." };
    for (let c = a; c <= b; c++) values.push(String.fromCharCode(c));
  }
  return { urls: values.map((v) => pattern.replace(m[0], v)), error: "" };
}

/** The links the dialog currently describes. */
function expandBatch(): { urls: string[]; error: string } {
  const fail = (error: string) => ({ urls: [], error });
  if (batchMode === "list") {
    const urls = linksIn($<HTMLTextAreaElement>("batch-links").value);
    return urls.length > BATCH_MAX ? fail(`At most ${BATCH_MAX} links at once.`) : { urls, error: "" };
  }
  const pattern = $<HTMLInputElement>("batch-url").value.trim();
  const range = pattern.match(RANGE);
  $("batch-ph-fields").classList.toggle("dimmed", !!range);
  if (range) return expandRange(pattern, range);
  const ph = $<HTMLInputElement>("batch-ph").value;
  const from = Math.trunc(Number($<HTMLInputElement>("batch-from").value));
  const to = Math.trunc(Number($<HTMLInputElement>("batch-to").value));
  const pad = Math.min(12, Math.max(0, Math.trunc(Number($<HTMLInputElement>("batch-pad").value)) || 0));
  if (!pattern) return fail("");
  if (!ph) return fail("Enter a placeholder.");
  if (!pattern.includes(ph)) return fail(`Add a range like [1-10] to the address, or a "${ph}" placeholder.`);
  if (!Number.isFinite(from) || !Number.isFinite(to) || from < 0 || to < from) return fail("Enter a valid range (From ≤ To).");
  if (to - from + 1 > BATCH_MAX) return fail(`At most ${BATCH_MAX} links at once.`);
  const urls: string[] = [];
  for (let n = from; n <= to; n++) urls.push(pattern.split(ph).join(String(n).padStart(pad, "0")));
  return { urls, error: "" };
}

/** Check results for the current list: a probe, or the error text. Null until "Check all" ran. */
let batchChecked: Map<string, ProbeResult | string> | null = null;
let batchBusy = false;

function batchOkCount(): number {
  return batchChecked ? [...batchChecked.values()].filter((r) => typeof r !== "string").length : 0;
}

function renderBatch() {
  const { urls, error } = expandBatch();
  $("batch-error").textContent = error;
  const list = $("batch-preview");
  list.hidden = urls.length === 0;
  list.replaceChildren(
    ...urls.map((u) => {
      const r = batchChecked?.get(u);
      const li = el("li", r === undefined ? "" : typeof r === "string" ? "bad" : "ok");
      li.append(el("span", "", u));
      if (r !== undefined) {
        let note = typeof r === "string" ? r : `${r.fileName} · ${fmtBytes(r.size)}`;
        if (typeof r !== "string" && r.duplicate) note += " · already in your list";
        li.append(el("small", "", note));
      }
      return li;
    }),
  );
  const go = $<HTMLButtonElement>("batch-go");
  if (!batchChecked) {
    $("batch-count").textContent = urls.length ? `${urls.length} link${urls.length === 1 ? "" : "s"}:` : "";
    go.textContent = "Check all";
  } else {
    const ok = batchOkCount();
    const bad = urls.length - ok;
    const total = [...batchChecked.values()].reduce<number>((sum, r) => sum + (typeof r === "string" ? 0 : (r.size ?? 0)), 0);
    const unknown = [...batchChecked.values()].some((r) => typeof r !== "string" && r.size == null);
    $("batch-count").textContent =
      `${ok} OK · ${bad} failed · total ${fmtBytes(total)}${unknown ? "+" : ""}` + (bad ? " (failed links are skipped)" : "");
    go.textContent = ok ? `Download ${ok}` : "Check all";
  }
  go.disabled = urls.length === 0 || batchBusy;
}

/** Any edit invalidates earlier check results. */
function updateBatchPreview() {
  batchChecked = null;
  renderBatch();
}

function setBatchMode(mode: "pattern" | "list") {
  batchMode = mode;
  $("batch-tabs").querySelectorAll<HTMLElement>(".tab").forEach((t) => t.classList.toggle("active", t.dataset.mode === mode));
  $("batch-pattern").hidden = mode !== "pattern";
  $("batch-listmode").hidden = mode !== "list";
  updateBatchPreview();
}

function showBatchDialog(title: string) {
  $("batch-title").textContent = title;
  fillQueueSelect($<HTMLSelectElement>("batch-queue"));
  $<HTMLDialogElement>("dlg-batch").showModal();
}

function openBatchDialog() {
  $<HTMLInputElement>("batch-url").value = "";
  $<HTMLTextAreaElement>("batch-links").value = "";
  setBatchMode("pattern");
  showBatchDialog("Batch download");
  $<HTMLInputElement>("batch-url").focus();
}

/** The batch dialog on its list page, filled with `urls` (import, grabber, drop). */
function openBatchList(urls: string[], title: string) {
  const dlg = $<HTMLDialogElement>("dlg-batch");
  if (dlg.open) dlg.close();
  $<HTMLTextAreaElement>("batch-links").value = urls.join("\n");
  setBatchMode("list");
  showBatchDialog(title);
  $<HTMLTextAreaElement>("batch-links").focus();
}

/** Runs `fn` over `urls`, a few at a time. */
async function pool(urls: string[], fn: (url: string) => Promise<void>) {
  let next = 0;
  const worker = async () => {
    while (next < urls.length) await fn(urls[next++]);
  };
  await Promise.all(Array.from({ length: 4 }, worker));
}

async function checkBatch(urls: string[]) {
  const results = new Map<string, ProbeResult | string>();
  const go = $<HTMLButtonElement>("batch-go");
  batchBusy = true;
  go.disabled = true;
  await pool(urls, async (url) => {
    try {
      results.set(url, await invoke<ProbeResult>("probe_url", { url, headers: [] }));
    } catch (e) {
      results.set(url, String(e));
    }
    go.textContent = `Checking ${results.size}/${urls.length}…`;
  });
  batchBusy = false;
  batchChecked = results;
  renderBatch();
}

async function downloadBatch(urls: string[]) {
  const go = $<HTMLButtonElement>("batch-go");
  const start = $<HTMLInputElement>("batch-start").checked;
  const saveDir = $<HTMLInputElement>("batch-dir").value.trim() || null;
  const queue = $<HTMLSelectElement>("batch-queue").value;
  const good = urls.filter((u) => typeof batchChecked?.get(u) === "object");
  batchBusy = true;
  go.disabled = true;
  const failures: string[] = [];
  let done = 0;
  await pool(good, async (url) => {
    const p = batchChecked!.get(url) as ProbeResult;
    try {
      const item = await invoke<Item>("add_download", {
        request: {
          url,
          fileName: p.fileName,
          saveDir,
          headers: [],
          start,
          scheduled: false,
          size: p.size,
          resumable: p.resumable,
          queue,
          stream: !!p.stream,
          live: !!p.stream?.live,
        },
      });
      if (!items.has(item.id)) items.set(item.id, item);
    } catch (e) {
      failures.push(`${url}: ${e}`);
    }
    go.textContent = `Adding ${++done}/${good.length}…`;
  });
  batchBusy = false;
  scheduleRender();
  renderBatch();
  if (failures.length === 0) $<HTMLDialogElement>("dlg-batch").close();
  else $("batch-error").textContent = `${failures.length} of ${good.length} could not be added. First: ${failures[0]}`;
}

async function submitBatch() {
  const { urls } = expandBatch();
  if (urls.length === 0 || batchBusy) return;
  if (batchChecked && batchOkCount() > 0) await downloadBatch(urls);
  else await checkBatch(urls);
}

// ---------- import / export / dropped links ----------

const LINK_FILES = [{ name: "Link lists", extensions: ["txt", "lst", "csv"] }];

async function importLinks() {
  const path = await openDialog({ multiple: false, filters: LINK_FILES });
  if (typeof path !== "string") return;
  try {
    const links = await invoke<string[]>("read_link_file", { path });
    if (!links.length) return showToast("No http or https links found in that file.");
    openBatchList(links, `Import ${links.length} link${links.length === 1 ? "" : "s"}`);
  } catch (e) {
    showToast(`Could not read the file: ${e}`);
  }
}

async function exportLinks(ids: number[]) {
  const path = await saveDialog({ defaultPath: "bright-downloader-links.txt", filters: LINK_FILES });
  if (!path) return;
  try {
    const n = await invoke<number>("export_links", { path, ids });
    showToast(`Saved ${n} link${n === 1 ? "" : "s"}.`);
  } catch (e) {
    showToast(`Could not save the file: ${e}`);
  }
}

/** One link opens the Add dialog; several go to the batch list. */
function handleLinks(links: string[], title: string) {
  if (links.length === 0) showToast("No http or https links found.");
  else if (links.length === 1) openAddWith(links[0]);
  else openBatchList(links, title);
}

function dropAccepted(e: DragEvent): boolean {
  const types = e.dataTransfer?.types ?? [];
  return !document.querySelector("dialog[open]") && ["text/uri-list", "text/plain", "Files"].some((t) => types.includes(t));
}

async function handleDrop(data: DataTransfer) {
  let text = data.getData("text/uri-list") || data.getData("text/plain");
  for (const f of [...data.files]) {
    if (f.type.startsWith("text/") || /\.(txt|lst|csv|url)$/i.test(f.name)) text += "\n" + (await f.text());
  }
  // .url shortcuts store their target as "URL=https://…".
  text = text.replace(/^URL=/gim, "");
  handleLinks(linksIn(text), "Dropped links");
}

// ---------- grabber ----------

const GRAB_EXTS = "zip,rar,7z,iso,exe,msi,apk,pdf,epub,mp3,flac,m4a,mp4,mkv,avi,mov,webm";
let grabbed: Grabbed | null = null;

function resetGrab() {
  grabbed = null;
  $("grab-results").hidden = true;
  $("grab-head").hidden = true;
  $("grab-error").textContent = "";
  $("grab-go").textContent = "Find files";
}

function grabSelection(): string[] {
  return [...$("grab-results").querySelectorAll<HTMLInputElement>("input:checked")].map((b) => b.value);
}

function updateGrabButton() {
  const n = grabSelection().length;
  const go = $<HTMLButtonElement>("grab-go");
  go.textContent = n ? `Add ${n} file${n === 1 ? "" : "s"}…` : "Find files";
}

async function openGrabber() {
  resetGrab();
  const exts = $<HTMLInputElement>("grab-exts");
  if (!exts.value) exts.value = GRAB_EXTS;
  $<HTMLDialogElement>("dlg-grab").showModal();
  const url = $<HTMLInputElement>("grab-url");
  if (!url.value) {
    const link = await invoke<string | null>("clipboard_url").catch(() => null);
    if (link) url.value = link;
  }
  url.select();
}

async function submitGrab() {
  const go = $<HTMLButtonElement>("grab-go");
  const picked = grabSelection();
  if (grabbed && picked.length) {
    $<HTMLDialogElement>("dlg-grab").close();
    let host = "";
    try {
      host = ` from ${new URL($<HTMLInputElement>("grab-url").value).host}`;
    } catch {
      /* no host */
    }
    openBatchList(picked, `Grabbed ${picked.length} file${picked.length === 1 ? "" : "s"}${host}`);
    return;
  }
  resetGrab();
  go.disabled = true;
  go.textContent = "Searching…";
  try {
    grabbed = await invoke<Grabbed>("grab_links", {
      url: $<HTMLInputElement>("grab-url").value.trim(),
      depth: Number($<HTMLSelectElement>("grab-depth").value),
      exts: $<HTMLInputElement>("grab-exts").value.split(/[\s,;]+/).filter(Boolean),
    });
    const list = $("grab-results");
    list.replaceChildren(
      ...grabbed.files.map((f) => {
        const li = el("li");
        const label = el("label");
        const box = el("input");
        box.type = "checkbox";
        box.checked = true;
        box.value = f.url;
        box.onchange = updateGrabButton;
        label.title = f.url;
        label.append(box, el("span", "g-name", f.fileName || f.url), el("span", "g-url", f.url));
        li.append(label);
        return li;
      }),
    );
    const n = grabbed.files.length;
    $("grab-status").textContent =
      `Found ${n} file${n === 1 ? "" : "s"} on ${grabbed.pages} page${grabbed.pages === 1 ? "" : "s"}` +
      (grabbed.truncated ? " (stopped at the page or time limit)" : "");
    $("grab-head").hidden = false;
    list.hidden = n === 0;
  } catch (e) {
    $("grab-error").textContent = String(e);
  } finally {
    go.disabled = false;
    updateGrabButton();
  }
}

// ---------- when all downloads finish ----------

let doneTimer = 0;

function onAllDone(action: DoneAction) {
  const what: Record<DoneAction, string> = {
    nothing: "",
    run: "",
    exit: "Bright Downloader will quit",
    sleep: "The computer will go to sleep",
    hibernate: "The computer will hibernate",
    shutdown: "The computer will shut down",
  };
  if (!what[action]) return;
  const dlg = $<HTMLDialogElement>("dlg-done");
  let left = 30;
  const tick = () => ($("done-text").textContent = `${what[action]} in ${left} seconds.`);
  tick();
  clearInterval(doneTimer);
  doneTimer = window.setInterval(() => {
    left--;
    tick();
    if (left <= 0) {
      dlg.returnValue = "now";
      dlg.close();
    }
  }, 1000);
  dlg.returnValue = "";
  dlg.onclose = () => {
    clearInterval(doneTimer);
    if (dlg.returnValue === "now") invoke("run_done_action", { action }).catch((e) => showToast(`Could not do that: ${e}`));
  };
  if (!dlg.open) dlg.showModal();
}

// ---------- theme ----------

type Theme = "light" | "dark" | "system";
const THEME_KEY = "theme";

function savedTheme(): Theme {
  try {
    const t = localStorage.getItem(THEME_KEY);
    if (t === "light" || t === "dark") return t;
  } catch {
    /* storage unavailable */
  }
  return "system";
}

function applyTheme(t: Theme) {
  if (t === "system") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.dataset.theme = t;
}

function saveTheme(t: Theme) {
  try {
    localStorage.setItem(THEME_KEY, t);
  } catch {
    /* storage unavailable */
  }
  applyTheme(t);
}

// ---------- settings ----------

/** Editable rows for the Queues tab. Existing queues can't be renamed (their downloads would move to Main). */
function renderQueueRows(list: Queue[]) {
  const box = $("set-queues");
  const head = el("div", "queue-row queue-head");
  head.append(el("span", "", "Name"), el("span", "", "At once"), el("span"));
  box.replaceChildren(
    head,
    ...list.map((q) => {
      const row = el("div", "queue-row");
      const name = el("input");
      name.value = q.name;
      name.placeholder = "Queue name";
      name.readOnly = q.name !== "";
      name.className = "q-name";
      const max = el("input");
      max.type = "number";
      max.min = "0";
      max.max = "20";
      max.value = String(q.maxConcurrent);
      max.className = "q-max";
      const del = el("button", "q-del", "✕");
      del.type = "button";
      del.title = "Delete queue (its downloads move to Main)";
      del.disabled = q.name === MAIN_QUEUE;
      del.onclick = () => row.remove();
      row.append(name, max, del);
      return row;
    }),
  );
}

function readQueueRows(): Queue[] {
  return [...$("set-queues").querySelectorAll<HTMLElement>(".queue-row:not(.queue-head)")]
    .map((row) => ({
      name: row.querySelector<HTMLInputElement>(".q-name")!.value.trim(),
      maxConcurrent: Math.min(20, Math.max(0, Math.round(Number(row.querySelector<HTMLInputElement>(".q-max")!.value)) || 0)),
    }))
    .filter((q) => q.name);
}

function parseHosts(text: string): Settings["hostConnections"] {
  return text
    .split(/\r?\n/)
    .map((l) => l.trim().split(/\s+/))
    .filter((p) => p.length >= 2 && p[0] && Number.isFinite(Number(p[1])))
    .map(([host, n]) => ({ host: host.replace(/^https?:\/\//, "").replace(/\/.*$/, ""), connections: Math.min(32, Math.max(1, Math.round(Number(n)))) }));
}

function renderLoginRows(list: SiteLogin[]) {
  const box = $("set-logins");
  const head = el("div", "queue-row login-row queue-head");
  head.append(el("span", "", "Server"), el("span", "", "User name"), el("span", "", "Password"), el("span"));
  box.replaceChildren(
    head,
    ...list.map((l) => {
      const row = el("div", "queue-row login-row");
      row.dataset.sealed = l.password;
      const input = (cls: string, value: string, placeholder: string, type = "text") => {
        const i = el("input", cls);
        i.type = type;
        i.value = value;
        i.placeholder = placeholder;
        i.spellcheck = false;
        return i;
      };
      const pass = input("l-pass", "", l.password ? "Saved" : "Password", "password");
      const del = el("button", "q-del", "✕");
      del.type = "button";
      del.title = "Delete login";
      del.onclick = () => row.remove();
      row.append(input("l-host", l.host, "files.example.com"), input("l-user", l.username, "User name"), pass, del);
      return row;
    }),
  );
}

function readLoginRows(): SiteLogin[] {
  return [...$("set-logins").querySelectorAll<HTMLElement>(".login-row:not(.queue-head)")]
    .map((row) => {
      const typed = row.querySelector<HTMLInputElement>(".l-pass")!.value;
      return {
        host: row.querySelector<HTMLInputElement>(".l-host")!.value.trim(),
        username: row.querySelector<HTMLInputElement>(".l-user")!.value.trim(),
        password: row.dataset.sealed ?? "",
        newPassword: typed || null,
      };
    })
    .filter((l) => l.host);
}

const TOOL_IDS = { ffmpeg: "tool-ffmpeg", ytdlp: "tool-ytdlp" } as const;
let installing: keyof typeof TOOL_IDS | null = null;

async function refreshTools() {
  const st = await invoke<ToolsStatus>("tools_status").catch(() => null);
  if (!st) return;
  for (const [tool, path] of [["ffmpeg", st.ffmpeg], ["ytdlp", st.ytDlp]] as const) {
    const status = $(`${TOOL_IDS[tool]}-status`);
    status.textContent = path ?? "—";
    status.classList.toggle("ok", !!path);
    const btn = $<HTMLButtonElement>(TOOL_IDS[tool]);
    if (installing !== tool) {
      btn.textContent = path ? "Update" : tool === "ffmpeg" ? "Get ffmpeg" : "Get yt-dlp";
      btn.disabled = installing !== null;
    }
    btn.onclick = () => installTool(tool);
  }
}

async function installTool(tool: keyof typeof TOOL_IDS) {
  installing = tool;
  $("tool-error").textContent = "";
  const btn = $<HTMLButtonElement>(TOOL_IDS[tool]);
  btn.disabled = true;
  btn.textContent = "Downloading…";
  try {
    await invoke("install_tool", { tool: tool === "ffmpeg" ? "ffmpeg" : "ytdlp" });
  } catch (e) {
    $("tool-error").textContent = String(e);
  } finally {
    installing = null;
    refreshTools();
  }
}

async function openSettings(tab = "general") {
  const s = await invoke<Settings>("get_settings");
  settings = s;
  const themeBefore = savedTheme();
  const themeRadios = [...document.querySelectorAll<HTMLInputElement>("#set-theme input")];
  const pickedTheme = () => (themeRadios.find((r) => r.checked)?.value ?? "system") as Theme;
  themeRadios.forEach((r) => {
    r.checked = r.value === themeBefore;
    r.onchange = () => applyTheme(pickedTheme()); // live preview; reverted on Cancel
  });
  $<HTMLInputElement>("set-dir").value = s.downloadDir;
  $<HTMLInputElement>("set-sort").checked = s.sortByCategory;
  $<HTMLInputElement>("set-clip").checked = s.clipboardMonitor;
  $<HTMLInputElement>("set-autostart").checked = s.startWithWindows;
  $<HTMLInputElement>("set-updates").checked = s.checkUpdates;
  $<HTMLInputElement>("set-progress-win").checked = s.progressWindows;
  $<HTMLSelectElement>("set-lang").value = s.language || "auto";
  const proxyMode = $<HTMLSelectElement>("set-proxy-mode");
  proxyMode.value = s.proxyMode;
  const syncProxy = () => $("set-proxy-field").classList.toggle("dimmed", proxyMode.value !== "custom");
  proxyMode.onchange = syncProxy;
  syncProxy();
  $<HTMLInputElement>("set-proxy").value = s.proxyUrl;
  $<HTMLInputElement>("set-quota-mb").value = String(s.quotaMb);
  $<HTMLInputElement>("set-quota-hours").value = String(s.quotaHours);
  renderLoginRows(s.logins);
  $("set-login-add").onclick = () => {
    const rows = readLoginRows();
    renderLoginRows([...rows, { host: "", username: "", password: "" }]);
    const hosts = $("set-logins").querySelectorAll<HTMLInputElement>(".l-host");
    hosts[hosts.length - 1]?.focus();
  };
  refreshTools();
  $("set-error").textContent = "";
  $<HTMLInputElement>("set-conns").value = String(s.connections);
  $<HTMLInputElement>("set-max").value = String(s.maxConcurrent);
  $<HTMLInputElement>("set-limit").value = String(s.speedLimitKib);
  $<HTMLInputElement>("set-retry").value = String(s.autoRetry);
  $<HTMLTextAreaElement>("set-hosts").value = s.hostConnections.map((h) => `${h.host} ${h.connections}`).join("\n");
  renderQueueRows(s.queues);
  $("set-queue-add").onclick = () => {
    const rows = readQueueRows();
    renderQueueRows([...rows, { name: "", maxConcurrent: 1 }]);
    const inputs = $("set-queues").querySelectorAll<HTMLInputElement>(".q-name");
    // Rows rebuilt from the form keep their names editable only if new; re-lock the old ones.
    inputs.forEach((input, k) => (input.readOnly = k < rows.length && s.queues.some((q) => q.name === input.value)));
    inputs[inputs.length - 1]?.focus();
  };
  $<HTMLInputElement>("set-scan").value = s.scanCommand;
  $("set-scan-defender").onclick = async () => {
    $<HTMLInputElement>("set-scan").value = await invoke<string>("default_scan_command");
  };
  $<HTMLInputElement>("set-extract").checked = s.extractArchives;
  // Variable buttons insert into whichever command box was used last.
  const cmdInputs = ["set-scan", "set-default-cmd", "set-start-cmd", "set-finish-cmd"].map((id) => $<HTMLInputElement>(id));
  let lastCmd = cmdInputs[3];
  cmdInputs.forEach((input) => (input.onfocus = () => (lastCmd = input)));
  $("set-vars").querySelectorAll<HTMLButtonElement>(".var").forEach((btn) => {
    btn.onmousedown = (e) => e.preventDefault(); // keep the caret in the command box
    btn.onclick = () => {
      const start = lastCmd.selectionStart ?? lastCmd.value.length;
      const end = lastCmd.selectionEnd ?? start;
      lastCmd.setRangeText(btn.dataset.var!, start, end, "end");
      lastCmd.focus();
    };
  });
  $<HTMLInputElement>("set-start-cmd").value = s.startCommand;
  $<HTMLInputElement>("set-finish-cmd").value = s.finishCommand;
  const defaultDone = $<HTMLSelectElement>("set-default-done");
  defaultDone.value = s.defaultWhenDone;
  const syncDefaultCmd = () => $("set-default-cmd-field").classList.toggle("dimmed", defaultDone.value !== "command");
  defaultDone.onchange = syncDefaultCmd;
  syncDefaultCmd();
  $<HTMLInputElement>("set-default-cmd").value = s.defaultDoneCommand;
  $<HTMLSelectElement>("set-history").value = s.historyRetention;
  $("set-history-note").textContent = "";
  $("set-clear-history").onclick = async () => {
    await invoke("clear_history");
    $("set-history-note").textContent = "History cleared.";
  };
  const sched = $("sch-on").closest("fieldset")!;
  const schOn = $<HTMLInputElement>("sch-on");
  schOn.checked = s.schedule.enabled;
  sched.classList.toggle("off", !schOn.checked);
  schOn.onchange = () => sched.classList.toggle("off", !schOn.checked);
  $<HTMLInputElement>("sch-start").value = s.schedule.start;
  $<HTMLInputElement>("sch-stop").value = s.schedule.stop;
  const whenDone = $<HTMLSelectElement>("set-when-done");
  whenDone.value = s.whenDone;
  const syncProgram = () => $("set-program-field").classList.toggle("dimmed", whenDone.value !== "run");
  whenDone.onchange = syncProgram;
  syncProgram();
  $<HTMLInputElement>("set-program").value = s.whenDoneProgram;
  const days = $("sch-days");
  days.replaceChildren(
    ...DAY_NAMES.map((name, d) => {
      const label = el("label", "day");
      const box = el("input");
      box.type = "checkbox";
      box.value = String(d);
      box.checked = s.schedule.days.includes(d);
      label.append(box, document.createTextNode(name));
      return label;
    }),
  );
  const dlg = $<HTMLDialogElement>("dlg-settings");
  const showTab = (name: string) => {
    dlg.querySelectorAll<HTMLElement>(".tab").forEach((t) => t.classList.toggle("active", t.dataset.tab === name));
    dlg.querySelectorAll<HTMLElement>(".tab-panel").forEach((p) => (p.hidden = p.dataset.panel !== name));
  };
  dlg.querySelectorAll<HTMLElement>(".tab").forEach((t) => (t.onclick = () => showTab(t.dataset.tab!)));
  showTab(tab);
  dlg.returnValue = "";
  dlg.onclose = async () => {
    if (dlg.returnValue !== "ok") {
      applyTheme(themeBefore);
      return;
    }
    saveTheme(pickedTheme());
    const clamp = (v: string, lo: number, hi: number, d: number) => {
      const n = Math.round(Number(v));
      return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : d;
    };
    const next: Settings = {
      downloadDir: $<HTMLInputElement>("set-dir").value.trim() || s.downloadDir,
      sortByCategory: $<HTMLInputElement>("set-sort").checked,
      connections: clamp($<HTMLInputElement>("set-conns").value, 1, 32, s.connections),
      maxConcurrent: clamp($<HTMLInputElement>("set-max").value, 1, 20, s.maxConcurrent),
      speedLimitKib: clamp($<HTMLInputElement>("set-limit").value, 0, 10_000_000, s.speedLimitKib),
      historyRetention: $<HTMLSelectElement>("set-history").value as Settings["historyRetention"],
      schedule: {
        enabled: schOn.checked,
        start: $<HTMLInputElement>("sch-start").value || s.schedule.start,
        stop: $<HTMLInputElement>("sch-stop").value,
        days: [...days.querySelectorAll<HTMLInputElement>("input:checked")].map((b) => Number(b.value)),
      },
      queues: readQueueRows(),
      hostConnections: parseHosts($<HTMLTextAreaElement>("set-hosts").value),
      autoRetry: clamp($<HTMLInputElement>("set-retry").value, 0, 20, s.autoRetry),
      scanCommand: $<HTMLInputElement>("set-scan").value.trim(),
      extractArchives: $<HTMLInputElement>("set-extract").checked,
      whenDone: whenDone.value as DoneAction,
      whenDoneProgram: $<HTMLInputElement>("set-program").value.trim(),
      clipboardMonitor: $<HTMLInputElement>("set-clip").checked,
      startWithWindows: $<HTMLInputElement>("set-autostart").checked,
      proxyMode: proxyMode.value as Settings["proxyMode"],
      proxyUrl: $<HTMLInputElement>("set-proxy").value.trim(),
      logins: readLoginRows(),
      quotaMb: clamp($<HTMLInputElement>("set-quota-mb").value, 0, 10_000_000, s.quotaMb),
      quotaHours: clamp($<HTMLInputElement>("set-quota-hours").value, 1, 168, s.quotaHours),
      progressWindows: $<HTMLInputElement>("set-progress-win").checked,
      language: $<HTMLSelectElement>("set-lang").value,
      startCommand: $<HTMLInputElement>("set-start-cmd").value.trim(),
      finishCommand: $<HTMLInputElement>("set-finish-cmd").value.trim(),
      defaultWhenDone: defaultDone.value as OpenWhenDone,
      defaultDoneCommand: $<HTMLInputElement>("set-default-cmd").value.trim(),
      checkUpdates: $<HTMLInputElement>("set-updates").checked,
      skippedVersion: settings?.skippedVersion ?? s.skippedVersion,
    };
    try {
      await invoke("set_settings", { settings: next });
    } catch (e) {
      showToast(String(e));
    }
    const languageChanged = next.language !== s.language;
    settings = await invoke<Settings>("get_settings");
    // Texts were translated as they appeared; reload to show the new language.
    if (languageChanged) location.reload();
    if (view.startsWith("queue:") && !queues().some((q) => `queue:${q.name}` === view)) view = "all";
    scheduleRender();
  };
  dlg.showModal();
}

// ---------- wiring ----------

function wire() {
  $("btn-add").onclick = openAddDialog;
  $("btn-add-empty").onclick = openAddDialog;
  $("btn-batch").onclick = openBatchDialog;
  $("btn-grab").onclick = openGrabber;
  $("btn-links").onclick = (e) => {
    e.stopPropagation();
    linksMenu();
  };
  $("btn-help").onclick = (e) => {
    e.stopPropagation();
    helpMenu();
  };
  for (const id of ["batch-url", "batch-ph", "batch-from", "batch-to", "batch-pad", "batch-links"]) $(id).oninput = updateBatchPreview;
  $("batch-tabs").querySelectorAll<HTMLElement>(".tab").forEach((t) => (t.onclick = () => setBatchMode(t.dataset.mode as "pattern" | "list")));
  $("form-batch").onsubmit = (e) => {
    e.preventDefault();
    submitBatch();
  };
  $("batch-cancel").onclick = () => $<HTMLDialogElement>("dlg-batch").close();
  $("batch-browse").onclick = () => browseInto("batch-dir");
  $("form-grab").onsubmit = (e) => {
    e.preventDefault();
    submitGrab();
  };
  for (const id of ["grab-url", "grab-exts"]) $(id).oninput = resetGrab;
  $("grab-depth").onchange = resetGrab;
  $("grab-cancel").onclick = () => $<HTMLDialogElement>("dlg-grab").close();
  const tickAll = (on: boolean) => {
    $("grab-results").querySelectorAll<HTMLInputElement>("input").forEach((b) => (b.checked = on));
    updateGrabButton();
  };
  $("grab-all").onclick = () => tickAll(true);
  $("grab-none").onclick = () => tickAll(false);
  $("btn-resume").onclick = resumeSelected;
  $("btn-pause").onclick = pauseSelected;
  $("btn-pause-all").onclick = () =>
    act("pause_download", (i) => i.status === "downloading" || i.status === "queued", items.keys());
  $("btn-delete").onclick = () => askDelete([...selected]);
  $("btn-clear").onclick = () =>
    askDelete([...items.values()].filter((i) => i.status === "completed").map((i) => i.id));
  $("btn-folder").onclick = () => selected.size === 1 && invoke("show_in_folder", { id: [...selected][0] });
  $("btn-settings").onclick = () => openSettings();
  $("d-open").onclick = () => selected.size === 1 && invoke("open_file", { id: [...selected][0] });
  $("d-folder").onclick = () => selected.size === 1 && invoke("show_in_folder", { id: [...selected][0] });
  $("toast-close").onclick = () => ($("toast").hidden = true);

  $<HTMLInputElement>("search").oninput = (e) => {
    query = (e.target as HTMLInputElement).value;
    scheduleRender();
  };

  const rows = $("rows");
  rows.onclick = (e) => {
    const tr = (e.target as HTMLElement).closest("tr");
    if (!tr) return;
    const id = Number(tr.dataset.id);
    if (e.shiftKey && lastClicked != null) {
      const ids = visibleItems().map((i) => i.id);
      const [a, b] = [ids.indexOf(lastClicked), ids.indexOf(id)].sort((x, y) => x - y);
      if (!e.ctrlKey) selected.clear();
      ids.slice(a, b + 1).forEach((x) => selected.add(x));
    } else if (e.ctrlKey) {
      selected.has(id) ? selected.delete(id) : selected.add(id);
      lastClicked = id;
    } else {
      selected.clear();
      selected.add(id);
      lastClicked = id;
    }
    scheduleRender();
  };
  rows.oncontextmenu = (e) => {
    e.preventDefault();
    const tr = (e.target as HTMLElement).closest("tr");
    if (!tr) return;
    const id = Number(tr.dataset.id);
    if (!selected.has(id)) {
      selected.clear();
      selected.add(id);
      lastClicked = id;
      scheduleRender();
    }
    rowMenu(e.clientX, e.clientY);
  };
  // The webview's own menu (Reload, Inspect…) is not useful to users.
  document.addEventListener("contextmenu", (e) => {
    if (!(e.target as HTMLElement).closest("input, textarea")) e.preventDefault();
  });
  document.addEventListener("mousedown", (e) => {
    if (!(e.target as HTMLElement).closest("#ctx, #ctx-sub")) hideMenu();
  });
  window.addEventListener("blur", hideMenu);
  $("url-cancel").onclick = () => $<HTMLDialogElement>("dlg-url").close();
  rows.ondblclick = (e) => {
    const tr = (e.target as HTMLElement).closest("tr");
    const i = tr && items.get(Number(tr.dataset.id));
    if (!i) return;
    if (i.status === "completed") invoke("open_file", { id: i.id });
    else if (i.status === "downloading") invoke("open_progress_window", { id: i.id });
    else if (i.status === "queued") invoke("pause_download", { id: i.id });
    else invoke("resume_download", { id: i.id });
  };

  $("form-add").onsubmit = (e) => {
    e.preventDefault();
    submitAdd(true);
  };
  $<HTMLInputElement>("add-url").oninput = () => resetAddDialogKeepUrl();
  $("add-dir").onchange = async () => {
    const dir = $<HTMLInputElement>("add-dir").value.trim();
    renderSpace(dir ? await invoke<number | null>("free_space", { dir }).catch(() => null) : null);
  };
  $("add-later").onclick = () => submitAdd(false, true);
  $("add-open").onchange = () => {
    const command = $<HTMLSelectElement>("add-open").value === "command";
    $("add-cmd-field").hidden = !command;
    if (command) $<HTMLInputElement>("add-cmd").focus();
  };
  $("add-hist-toggle").onclick = () => toggleHistory();
  $("hist-clear").onclick = async () => {
    await invoke("clear_history");
    loadHistory();
  };
  $("add-cancel").onclick = () => $<HTMLDialogElement>("dlg-add").close();
  $("add-browse").onclick = () => browseInto("add-dir");
  $("set-browse").onclick = () => browseInto("set-dir");

  // Drag links, a selection of text, a .txt list or a .url shortcut onto the window.
  let dragDepth = 0;
  document.addEventListener("dragenter", (e) => {
    if (!dropAccepted(e)) return;
    e.preventDefault();
    dragDepth++;
    $("drop").hidden = false;
  });
  document.addEventListener("dragover", (e) => {
    if (!dropAccepted(e)) return;
    e.preventDefault();
    e.dataTransfer!.dropEffect = "copy";
  });
  document.addEventListener("dragleave", () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (!dragDepth) $("drop").hidden = true;
  });
  document.addEventListener("drop", (e) => {
    dragDepth = 0;
    $("drop").hidden = true;
    if (!dropAccepted(e)) return;
    e.preventDefault();
    handleDrop(e.dataTransfer!);
  });
  // Ctrl+V on the list adds the pasted links.
  document.addEventListener("paste", (e) => {
    if (document.querySelector("dialog[open]") || (e.target as HTMLElement).closest("input, textarea")) return;
    const text = e.clipboardData?.getData("text") ?? "";
    if (!text.trim()) return;
    e.preventDefault();
    handleLinks(linksIn(text), "Pasted links");
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideMenu();
    if (document.querySelector("dialog[open]")) return;
    if (e.key === "Delete") askDelete([...selected]);
    else if (e.ctrlKey && e.key.toLowerCase() === "n") {
      e.preventDefault();
      openAddDialog();
    } else if (e.ctrlKey && e.key.toLowerCase() === "a") {
      e.preventDefault();
      visibleItems().forEach((i) => selected.add(i.id));
      scheduleRender();
    }
  });
  window.addEventListener("resize", scheduleRender);
}

function resetAddDialogKeepUrl() {
  probed = null;
  // Captured cookies belong to the original link; never send them elsewhere.
  pendingHeaders = [];
  $("add-info").hidden = true;
  $("add-later").hidden = true;
  $("add-go").textContent = "Check link";
}

async function init() {
  applyTheme(savedTheme());
  settings = await invoke<Settings>("get_settings").catch(() => null);
  setupI18n(settings?.language ?? "auto");
  wire();
  for (const i of await invoke<Item[]>("list_downloads")) items.set(i.id, i);
  await listen<Item>("item", (e) => {
    items.set(e.payload.id, e.payload);
    if (e.payload.status !== "downloading") progress.delete(e.payload.id);
    scheduleRender();
  });
  await listen<ProgressEvent>("progress", (e) => {
    progress.set(e.payload.id, e.payload);
    const hist = speedHist.get(e.payload.id) ?? [];
    hist.push(e.payload.speed);
    if (hist.length > GRAPH_POINTS) hist.splice(0, hist.length - GRAPH_POINTS);
    speedHist.set(e.payload.id, hist);
    scheduleRender();
  });
  await listen("captured", () => takeCaptured());
  $<HTMLDialogElement>("dlg-add").addEventListener("close", () => setTimeout(showNextCaptured, 50));
  await listen<{ tool: string; total: number | null; downloaded: number }>("tool-progress", (e) => {
    const btn = $(TOOL_IDS[e.payload.tool === "ffmpeg" ? "ffmpeg" : "ytdlp"]);
    const { total, downloaded } = e.payload;
    btn.textContent = total ? `${Math.floor((downloaded / total) * 100)}%` : fmtBytes(downloaded);
  });
  await listen<number>("removed", (e) => {
    items.delete(e.payload);
    progress.delete(e.payload);
    speedHist.delete(e.payload);
    selected.delete(e.payload);
    scheduleRender();
  });
  await listen<string>("clipboard-link", (e) => {
    const url = e.payload;
    if ([...items.values()].some((i) => i.url === url) || document.querySelector("dialog[open]")) return;
    showToast(`Copied link: ${fileNameOf(url)}`, { label: "Download", run: () => openAddWith(url) });
  });
  await listen<DoneAction>("all-done", (e) => onAllDone(e.payload));
  await listen<UpdateInfo>("update-available", (e) => showUpdate(e.payload));
  await listen<{ total: number | null; downloaded: number }>("update-progress", (e) => {
    const { total, downloaded } = e.payload;
    const pct = total ? Math.floor((downloaded / total) * 100) : 0;
    $("upd-bar").style.width = `${pct}%`;
    $("upd-status").textContent =
      total && downloaded >= total ? "Checking the download and installing…" : `Downloading the update… ${total ? `${pct}%` : fmtBytes(downloaded)}`;
  });
  scheduleRender();
  takeCaptured();
  showWhatsNew();
}

// ---------- progress window (one download, like IDM's) ----------

async function initProgressWindow(id: number) {
  applyTheme(savedTheme());
  const s = await invoke<Settings>("get_settings").catch(() => null);
  setupI18n(s?.language ?? "auto");
  document.body.classList.add("progress-window");
  const root = el("div", "pw");
  const name = el("div", "pw-name");
  name.setAttribute("translate", "no");
  const status = el("div", "pw-status");
  const bar = el("div", "bar");
  const fill = el("i");
  bar.append(fill);
  const segbar = el("div", "segbar");
  const facts = el("dl", "facts");
  const fact = (label: string) => {
    const dd = el("dd");
    facts.append(el("dt", "", label), dd);
    return dd;
  };
  const fDone = fact("Downloaded");
  const fRate = fact("Transfer rate");
  const fEta = fact("Time left");
  const fConns = fact("Connections");
  const fUrl = fact("Address");
  fUrl.setAttribute("translate", "no");
  const actions = el("div", "actions");
  const btnToggle = el("button", "btn");
  const btnOpen = el("button", "btn", "Open");
  const btnFolder = el("button", "btn", "Show in folder");
  const btnClose = el("button", "btn", "Close");
  actions.append(btnOpen, btnFolder, btnToggle, btnClose);
  root.append(name, status, bar, segbar, facts, actions);
  document.body.append(root);

  let item = (await invoke<Item[]>("list_downloads")).find((i) => i.id === id);
  let prog: ProgressEvent | undefined;
  const render = () => {
    if (!item) {
      status.textContent = "This download was removed.";
      return;
    }
    items.set(item.id, item);
    if (prog) progress.set(item.id, prog);
    const s = statusInfo(item);
    name.textContent = item.fileName;
    status.textContent = s.label;
    fill.style.width = `${s.pct}%`;
    bar.hidden = item.status === "completed";
    fDone.textContent = s.total ? `${fmtBytes(s.done)} of ${fmtBytes(s.total)} (${s.pct.toFixed(1)}%)` : fmtBytes(s.done);
    fRate.textContent = item.status === "downloading" ? `${fmtBytes(s.speed)}/s` : "—";
    fEta.textContent = item.status === "downloading" && s.total ? fmtEta(s.total - s.done, s.speed) || "—" : "—";
    fConns.textContent = item.status === "downloading" && prog ? String(prog.connections) : "—";
    fUrl.textContent = item.url;
    segbar.replaceChildren();
    const total = s.total;
    if (prog && total && item.status === "downloading") {
      for (const g of prog.segments) {
        const seg = el("div", `seg${g.active ? " active" : ""}`);
        seg.style.left = `${(g.start / total) * 100}%`;
        seg.style.width = `${(g.written / total) * 100}%`;
        segbar.append(seg);
      }
    }
    segbar.hidden = !prog?.segments.length || item.status !== "downloading";
    const running = item.status === "downloading" || item.status === "queued";
    btnToggle.textContent = running ? (item.live ? "Stop recording" : "Pause") : "Resume";
    btnToggle.hidden = item.status === "completed";
    btnOpen.hidden = btnFolder.hidden = item.status !== "completed";
  };
  btnToggle.onclick = () => {
    if (!item) return;
    const running = item.status === "downloading" || item.status === "queued";
    invoke(running ? "pause_download" : "resume_download", { id });
  };
  btnOpen.onclick = () => invoke("open_file", { id });
  btnFolder.onclick = () => invoke("show_in_folder", { id });
  btnClose.onclick = () => getCurrentWindow().close();
  await listen<Item>("item", (e) => {
    if (e.payload.id !== id) return;
    item = e.payload;
    if (item.status !== "downloading") prog = undefined;
    render();
  });
  await listen<ProgressEvent>("progress", (e) => {
    if (e.payload.id !== id) return;
    prog = e.payload;
    if (item) item = { ...item, downloaded: prog.downloaded };
    render();
  });
  await listen<number>("removed", (e) => {
    if (e.payload === id) getCurrentWindow().close();
  });
  render();
}

const progressId = Number(new URLSearchParams(location.search).get("progress"));
if (progressId) initProgressWindow(progressId);
else init();
