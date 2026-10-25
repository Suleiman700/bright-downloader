<p align="center">
  <img src="site/images/hero.png" alt="Bright Downloader: downloads that finish faster" width="100%" />
</p>

<p align="center">
  <a href="https://github.com/Suleiman700/bright-downloader/releases/latest"><b>Download</b></a> ·
  <a href="https://suleiman700.github.io/bright-downloader/"><b>Website</b></a> ·
  <a href="https://github.com/Suleiman700/bright-downloader/issues">Report a problem</a> ·
  by BrightPixel
</p>

# Bright Downloader

**Bright Downloader** is a fast, free and open-source download manager for
Windows. It splits each file across up to 32 connections and keeps every one of
them busy until the last byte, so big downloads finish sooner. Pause any time
and pick up where you left off, even after a crash or a reboot.

It also catches downloads and videos straight from your browser, saves streams
and videos from 1,000+ sites in the quality you choose, and handles the
routine work for you: queues, a scheduler, speed limits, checksums and
automatic retries. No ads, no account, no tracking.

## Screenshots

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="site/images/main-dark.png" />
  <img src="site/images/main-light.png" alt="Main window with three downloads running, per-connection progress and a speed graph" />
</picture>

<table>
  <tr>
    <td width="62%"><img src="site/images/add-video.png" alt="Add download dialog with a quality choice for a video page" /></td>
    <td><img src="site/images/progress.png" alt="Progress window showing 16 connections downloading parts of one file" /></td>
  </tr>
  <tr>
    <td>Video pages and HLS / DASH streams: pick the quality before downloading.</td>
    <td>Progress window: each block is one connection's share of the file.</td>
  </tr>
</table>

## Download

Get the latest version from the **[Releases page](https://github.com/Suleiman700/bright-downloader/releases/latest)**:

| File | What it is |
|---|---|
| `Bright-Downloader_x.y.z_x64-setup.exe` | Installer. No administrator rights needed, updates itself. |
| `Bright-Downloader_x.y.z_x64-portable.zip` | Portable version. Unzip anywhere and run `brightdl.exe`; settings stay in a `data` folder next to it. |

Works on Windows 10 and 11 (64-bit).

## Features

### ⚡ Fast downloads
- Up to **32 connections per file** over HTTP(S) and FTP
- **Dynamic segmentation**: connections that finish early take over work from slower ones
- **Pause and resume** at any time, even after a crash or reboot
- **Automatic retries** when the connection drops
- Per-server connection limits, proxy support (HTTP, HTTPS, SOCKS5)

### 🎬 Video
- **HLS and MPEG-DASH streams**, including AES-128 encrypted HLS and live recordings
- **1,000+ video sites** (YouTube and more) through [yt-dlp](https://github.com/yt-dlp/yt-dlp)
- Choose the quality before downloading
- Saves as MP4 with audio merged when [ffmpeg](https://ffmpeg.org) is available; both helpers install in one click

### 🌐 Browser integration
- Extension for **Chrome, Edge and Firefox**
- Catches downloads automatically, including ones behind a login
- **Download this video** button on videos a page plays
- Right-click any link → *Download with Bright Downloader*
- Expired link? Refresh it from the browser and the download continues

### 🗂️ Organize and automate
- **Queues** with priorities and per-queue limits
- **Scheduler** to start and stop downloads at set times
- **Speed limits**, overall or per download
- **Data cap**: stop after a set amount per period
- Automatic **categories** with their own folders
- **Batch downloads** (`file[001-100].jpg`), link lists, drag and drop, paste
- **Site grabber** to pick files linked from a web page
- **Keep in sync**: re-download a file when it changes on the server
- When everything finishes: shut down, sleep, hibernate or run a program

### 🛡️ Safe and reliable
- **Checksum** verification (MD5, SHA-256)
- Optional **virus scan** after download
- Changed files on the server are detected on resume
- Free-space check before a download starts
- Saved passwords are encrypted for your Windows account
- Updates are verified against published SHA-256 sums before they install

### 🧩 Comfort
- Runs in the **system tray** with notifications when downloads finish
- **Clipboard monitor** offers to download copied links
- Speed graph and per-connection progress
- Archives extracted automatically after download (zip, 7z, rar, tar)
- Light, dark and system themes
- **English, Arabic, French and Spanish**

> DRM-protected video (Widevine, PlayReady, FairPlay) is not supported.

## Browser extension

**Chrome / Edge**

1. Open `chrome://extensions` (or `edge://extensions`).
2. Turn on **Developer mode**.
3. Click **Load unpacked** and choose the `extension` folder.

**Firefox** (128 or newer)

1. Open `about:debugging#/runtime/this-firefox`.
2. Click **Load Temporary Add-on…** and choose `extension/manifest.json`.
3. In `about:addons` → Bright Downloader Integration → Permissions, allow access to all websites.

While Bright Downloader is running, downloads are sent to it automatically; when it is closed, the browser downloads as usual. The app only accepts requests from the extension on `127.0.0.1:47809`, never from web pages.

## Command line

```text
brightdl.exe <url> [--dir <folder>] [--name <file>] [--queue <name>] [--later] [--silent]
```

| Option | Meaning |
|---|---|
| `--dir`, `--name` | Save folder and file name |
| `--queue` | Add to a specific queue |
| `--later` | Add without starting |
| `--silent` | Add directly, without opening the Add dialog |

Short switches `/d <url> /p <folder> /f <file> /a /n` are also accepted. Links sent while the app is running go to the open window.

## Updates

Bright Downloader checks GitHub for new versions at startup and twice a day (this can be turned off in Options). Updates are downloaded, verified against their SHA-256 checksum and installed with one click; running downloads continue afterwards. The portable version links to the new release instead.

## Building from source

Requirements: [Rust](https://rustup.rs), Node.js 20+, pnpm and the Microsoft C++ Build Tools.

```sh
pnpm install
pnpm tauri dev      # run in development mode
pnpm tauri build    # build the installer
cargo test --workspace
```

| Path | Contents |
|---|---|
| `crates/engine` | Download engine (Rust) |
| `src-tauri` | Desktop app backend |
| `src`, `index.html` | App interface (TypeScript) |
| `extension` | Browser extension |
| `site` | Website |

## License

[MIT](LICENSE)
