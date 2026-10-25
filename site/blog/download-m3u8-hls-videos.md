+++
title = "How to download HLS (.m3u8) and DASH videos"
description = "What .m3u8 and .mpd streams are, why right-click Save doesn't work on them, and how to save them as a single MP4 with a free download manager."
date = "2026-10-22"
tags = ["video", "hls", "m3u8", "dash", "guide"]
+++

Right-click a video on many sites and there's no "Save video as". Look at the network traffic and you'll see dozens of small files and one odd one ending in `.m3u8` or `.mpd`. That's adaptive streaming, and it's how most video on the web is delivered today. Here's what's going on and how to save such a stream as a normal MP4 file.

> Only download videos you have the right to keep: your own uploads, content with a download-friendly license, or material the site allows you to save. Vibe doesn't break DRM, and streams protected by it can't be downloaded.

## What HLS and DASH are

Instead of one big video file, the server cuts the video into short segments, usually 2 to 10 seconds each, often in several qualities. A small text file lists them:

- **HLS** (HTTP Live Streaming) uses a playlist ending in `.m3u8`. A *master* playlist lists the available qualities; each quality has its own playlist of segments.
- **DASH** (Dynamic Adaptive Streaming over HTTP) uses an XML manifest ending in `.mpd`, which describes the same idea.

The player picks a quality, fetches segments one after another and switches quality as your bandwidth changes. That's great for watching and inconvenient for saving, because there's no single file to save.

## Saving a stream, the manual way

You can open the browser's developer tools, find the master playlist in the network tab, copy its address and give it to a tool like ffmpeg:

```
ffmpeg -i "https://example.com/video/master.m3u8" -c copy video.mp4
```

It works, but it downloads one segment at a time, doesn't resume, and you need the right cookies and headers for sites that require a login.

## Saving a stream with Vibe

1. **Install Vibe and its browser extension.** See the [download page](/download/).
2. **Start playing the video.** The player has to load its playlist before anything can see it.
3. **Click the Vibe button in the browser toolbar.** It lists the streams the page has loaded, with the main playlist first.
4. **Choose "Download with Vibe".** The stream goes to Vibe with the page's cookies and referrer.

Vibe then reads the playlist, picks the best quality (or the one you chose with the command line's `--quality` option), downloads the segments in parallel, decrypts AES-128 encrypted segments, and joins everything into one `.mp4` with ffmpeg. If ffmpeg isn't installed, you get the original pieces instead: a `.ts` file for HLS, or separate video and audio files for DASH.

From the command line it's one step:

```
vdm get -q 720 "https://example.com/video/master.m3u8"
```

## Troubleshooting

- **No streams are listed.** Start the video first, then open the popup. Some players load segments directly without a playlist; those can't be detected.
- **Error 403 or 401.** The stream needs your login. Use the extension (it sends cookies) rather than pasting the address into Vibe.
- **Separate audio and video files.** Install [ffmpeg](https://ffmpeg.org/download.html) and make sure it's on your PATH.
- **It's a live stream.** Live streams aren't supported yet; they're on the roadmap.

[Get Vibe for Windows](/download/) and save the next stream with one click.
