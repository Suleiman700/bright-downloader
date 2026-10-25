// "Download this video" button shown over videos on any page, like IDM's.
// It appears while the pointer is over a video (and briefly when one starts
// playing) and lists what the tab loaded: qualities of HLS streams and
// plain video files.

(() => {
  if (window.__brightdlVideoButton) return;
  window.__brightdlVideoButton = true;

  const msg = (key, subs) => chrome.i18n.getMessage(key, subs) || key;
  let enabled = true;
  let mediaCount = 0;
  let current = null; // the video the button belongs to
  let hideTimer = 0;
  let menuOpen = false;

  chrome.storage.sync.get({ showVideoButton: true }, (s) => (enabled = s.showVideoButton));
  chrome.storage.onChanged.addListener((c) => {
    if (c.showVideoButton) {
      enabled = c.showVideoButton.newValue;
      if (!enabled) hide();
    }
  });
  chrome.runtime.onMessage.addListener((m) => {
    if (m?.type === "brightdl:media") mediaCount = m.count;
  });
  chrome.runtime.sendMessage({ type: "brightdl:count" }).then((r) => (mediaCount = r?.count ?? 0)).catch(() => {});

  const ICON =
    '<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"><path d="M12 4v11m0 0l-4.5-4.5M12 15l4.5-4.5M5 19h14" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';

  const host = document.createElement("brightdl-video-button");
  host.setAttribute("dir", chrome.i18n.getMessage("@@bidi_dir") || "ltr");
  host.style.cssText = "all:initial;position:fixed;z-index:2147483647;top:0;left:0;display:none;";
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `
    <style>
      :host { all: initial; }
      .wrap { position: relative; font: 12px/1.3 "Segoe UI", system-ui, sans-serif; }
      button.main {
        display: flex; align-items: center; gap: 6px; padding: 6px 11px 6px 9px;
        border: 1px solid rgba(255,255,255,.18); border-radius: 7px;
        background: rgba(18,22,30,.88); color: #fff; font: inherit; font-weight: 600;
        cursor: pointer; box-shadow: 0 4px 14px rgba(0,0,0,.35); white-space: nowrap;
      }
      button.main:hover { background: rgba(37,99,235,.95); }
      button.main svg { color: #7fb0ff; }
      button.main:hover svg { color: #fff; }
      .menu {
        position: absolute; right: 0; top: calc(100% + 4px); min-width: 230px; max-width: 360px;
        max-height: 300px; overflow-y: auto; padding: 4px; border-radius: 8px;
        background: rgba(24,28,36,.97); color: #e8ebf1; box-shadow: 0 8px 24px rgba(0,0,0,.45);
      }
      .menu[hidden] { display: none; }
      .menu button {
        display: block; width: 100%; padding: 7px 10px; border: 0; border-radius: 5px;
        background: none; color: inherit; font: inherit; text-align: left; cursor: pointer;
        overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
      }
      .menu button:hover { background: rgba(91,140,255,.3); }
      .menu .head { padding: 6px 10px 3px; color: #9aa3b2; font-size: 11px; }
      .menu .empty { padding: 8px 10px; color: #c3c9d4; white-space: normal; }
      .live { color: #ff6b6b; font-weight: 700; }
      .page { border-top: 1px solid rgba(255,255,255,.12); border-radius: 0 0 5px 5px; color: #b9c3d6; }
      :host([dir="rtl"]) .menu { right: auto; left: 0; }
      :host([dir="rtl"]) .menu button { text-align: right; }
    </style>
    <div class="wrap">
      <button class="main">${ICON}<span class="text"></span></button>
      <div class="menu" hidden></div>
    </div>`;
  const mainBtn = root.querySelector("button.main");
  const label = root.querySelector(".text");
  const menu = root.querySelector(".menu");
  mainBtn.title = msg("buttonTitle");
  label.textContent = msg("buttonLabel");

  function attach() {
    const fs = document.fullscreenElement;
    // A fullscreen <video> can't hold children; a fullscreen player container can.
    const parent = fs && !(fs instanceof HTMLVideoElement) ? fs : document.documentElement;
    if (host.parentNode !== parent) parent.appendChild(host);
  }

  function place() {
    if (!current || host.style.display === "none") return;
    if (!current.isConnected) return hide();
    const r = current.getBoundingClientRect();
    const w = host.offsetWidth || 170;
    host.style.top = `${Math.max(4, r.top + 10)}px`;
    host.style.left = `${Math.max(4, Math.min(r.right - w - 10, innerWidth - w - 4))}px`;
  }

  // Every sizeable video gets the button: even when no file or stream was
  // seen (players like YouTube's), the page itself can go to yt-dlp.
  function hasSomething(video) {
    return mediaCount > 0 || /^https?:|^blob:/i.test(video.currentSrc || "") || video.readyState > 0;
  }

  function show(video) {
    if (!enabled || !hasSomething(video)) return;
    clearTimeout(hideTimer);
    if (current !== video) closeMenu();
    current = video;
    attach();
    host.style.display = "block";
    place();
  }

  function hide() {
    clearTimeout(hideTimer);
    closeMenu();
    host.style.display = "none";
    current = null;
  }

  function hideSoon(ms = 1200) {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => !menuOpen && hide(), ms);
  }

  function videoAt(x, y) {
    for (const v of document.querySelectorAll("video")) {
      const r = v.getBoundingClientRect();
      if (r.width < 160 || r.height < 90) continue;
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return v;
    }
    return null;
  }

  // Players cover the <video> with their own controls, so hit-test by position.
  let pending = false;
  document.addEventListener(
    "mousemove",
    (e) => {
      if (!enabled || pending) return;
      pending = true;
      requestAnimationFrame(() => {
        pending = false;
        if (e.composedPath().includes(host)) return clearTimeout(hideTimer);
        const v = videoAt(e.clientX, e.clientY);
        if (v) show(v);
        else if (current && !menuOpen) hideSoon();
      });
    },
    { capture: true, passive: true },
  );
  document.addEventListener(
    "play",
    (e) => {
      if (e.target instanceof HTMLVideoElement) {
        // The playlist request may still be in flight; give it a moment.
        setTimeout(() => {
          show(e.target);
          hideSoon(4000);
        }, 600);
      }
    },
    true,
  );
  addEventListener("scroll", () => requestAnimationFrame(place), { capture: true, passive: true });
  addEventListener("resize", () => requestAnimationFrame(place), { passive: true });
  document.addEventListener("fullscreenchange", () => {
    attach();
    requestAnimationFrame(place);
  });
  document.addEventListener("mousedown", (e) => {
    if (menuOpen && !e.composedPath().includes(host)) closeMenu();
  });

  function closeMenu() {
    menuOpen = false;
    menu.hidden = true;
  }

  function fmtSize(n) {
    if (!n) return "";
    const units = ["KB", "MB", "GB"];
    let v = n / 1024;
    let u = 0;
    while (v >= 1024 && u < units.length - 1) {
      v /= 1024;
      u++;
    }
    return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[u]}`;
  }

  function fmtTime(s) {
    const m = Math.floor(s / 60);
    return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m}:${String(Math.round(s % 60)).padStart(2, "0")}`;
  }

  function fileName(url) {
    try {
      return decodeURIComponent(new URL(url).pathname.split("/").filter(Boolean).pop() || url);
    } catch {
      return url;
    }
  }

  /** Download choices for the current video: its own file first, then
   * streams, then other files, then the page itself (video sites). */
  function choices(items) {
    const out = [];
    const src = current?.currentSrc || "";
    if (/^https?:/i.test(src)) {
      const known = items.find((e) => e.url === src);
      out.push({ url: src, kind: "file", label: `${msg("thisVideo")} · ${fileName(src)}${known?.size ? ` · ${fmtSize(known.size)}` : ""}` });
    }
    const streams = items.filter((e) => e.kind === "hls" || e.kind === "dash");
    streams.forEach((e, k) => {
      const name = streams.length > 1 ? msg("streamN", [String(k + 1)]) : msg("stream");
      if (e.variants.length) {
        for (const v of e.variants) out.push({ url: e.url, variant: v.url, kind: e.kind, label: `${name} · ${v.label}` });
      } else if (e.live) {
        out.push({ url: e.url, kind: e.kind, live: true, label: `${name} · ${msg("liveRecord")}` });
      } else {
        out.push({ url: e.url, kind: e.kind, label: `${name}${e.duration ? ` · ${fmtTime(e.duration)}` : ""}` });
      }
    });
    for (const e of items.filter((x) => x.kind === "file" && x.url !== src).sort((a, b) => (b.size ?? 0) - (a.size ?? 0))) {
      out.push({ url: e.url, kind: "file", label: `${fileName(e.url)}${e.size ? ` · ${fmtSize(e.size)}` : ""}` });
    }
    if (/^https?:/i.test(location.href)) out.push({ url: location.href, kind: "page", label: msg("fromPage"), page: true });
    return out;
  }

  function flash(text) {
    label.textContent = text;
    setTimeout(() => (label.textContent = msg("buttonLabel")), 2500);
  }

  async function send(choice) {
    closeMenu();
    label.textContent = msg("sending");
    const res = await chrome.runtime
      .sendMessage({ type: "brightdl:download", ...choice, title: document.title, pageUrl: location.href })
      .catch(() => null);
    flash(res?.ok ? msg(res.fallback ? "inBrowser" : "sent") : res?.error || msg("unreachable"));
  }

  mainBtn.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (menuOpen) return closeMenu();
    const items = (await chrome.runtime.sendMessage({ type: "brightdl:list" }).catch(() => null)) ?? [];
    const list = choices(items);
    menu.replaceChildren();
    if (!list.length) {
      const p = document.createElement("div");
      p.className = "empty";
      p.textContent = msg("nothingYet");
      menu.append(p);
    } else {
      const head = document.createElement("div");
      head.className = "head";
      head.textContent = msg("menuDownload");
      menu.append(head);
      for (const c of list) {
        const b = document.createElement("button");
        b.textContent = c.label;
        b.title = c.variant || c.url;
        if (c.live) b.classList.add("live");
        if (c.page) b.classList.add("page");
        b.addEventListener("click", (ev) => {
          ev.stopPropagation();
          send(c);
        });
        menu.append(b);
      }
    }
    menu.hidden = false;
    menuOpen = true;
    clearTimeout(hideTimer);
  });
  // Clicks must not reach the player underneath (it would pause the video).
  for (const type of ["mousedown", "mouseup", "pointerdown", "pointerup", "dblclick"]) {
    host.addEventListener(type, (e) => e.stopPropagation());
  }
})();
