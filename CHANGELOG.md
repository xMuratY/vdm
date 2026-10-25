# Changelog

All notable changes to Vibe Download Manager are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## 0.1.0 - 2026-10-25

The first release.

### Downloads
- Up to 32 connections per file. Pieces split again as connections finish, so no
  connection sits idle until the file is done.
- Resume after a pause, a crash, a reboot or a lost connection. The file is checked
  against the server (ETag and Last-Modified) before resuming.
- A single stream for servers that can't resume.
- HTTP, HTTPS and FTP (with resume), through the system proxy, an HTTP proxy or SOCKS5.
- Saved site logins, kept in the Windows Credential Manager.
- MD5, SHA-1, SHA-256 and SHA-512 checksum checks, and a command to run after each download
  (for example a virus scan).

### Video streams
- Saves HLS (`.m3u8`) and DASH (`.mpd`) streams: picks the quality, downloads the
  segments in parallel, decrypts AES-128 and joins audio and video into one `.mp4`
  when `ffmpeg` is installed.

### Queues and schedules
- Named queues, each with its own number of downloads at once, speed limit and daily
  schedule, plus a global speed limit.
- When a queue finishes, Vibe can close, sleep, hibernate or shut down the computer
  after a 60-second countdown you can cancel.

### Desktop app
- Sidebar filters by state, queue and file type, and a search box.
- Select many downloads (Ctrl+click, Shift+click, Ctrl+A) and pause, resume, move,
  remove or delete them together.
- Columns you can resize, reorder, hide, fit to the window or size to their content.
- A details panel for each download: a live map of its pieces, a speed graph, what
  the server reported, the request it sent and a log of what happened.
- Total time left across all running downloads.
- Files sorted into folders by type, clipboard link detection, tray icon,
  notifications and a single running instance.
- Light and dark themes, and 12 languages: English, Español, Français, Deutsch,
  Português (Brasil), Italiano, Русский, Türkçe, Polski, 简体中文, 日本語 and 한국어.
- Checks for new versions and updates itself.

### Browser extension
- For Chrome, Edge, Brave and Firefox: takes over browser downloads with their
  cookies, referrer and user agent, adds "Download with Vibe" to the context menu and
  lists the video streams a page plays. It is attached to the release as a zip to
  load in the browser's developer mode (see the README).

### Command line
- `vdm get` downloads (and resumes) a file or video stream with the same engine;
  `vdm probe` shows what a server reports before downloading.

### Installer
- A per-user Windows installer (no administrator rights) that asks about the desktop
  shortcut, the Start menu entry and starting at sign-in, and offers to launch Vibe.
  It also adds `vdm` to your PATH and connects the browser extension.

