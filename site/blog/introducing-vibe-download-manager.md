+++
title = "Introducing Vibe Download Manager"
description = "Vibe is a new free, open-source download manager for Windows, written in Rust: multi-connection downloads that resume, a video grabber and browser integration."
date = "2026-10-24"
tags = ["announcement", "open source", "download manager"]
+++

Big downloads still fail in boring ways. The Wi-Fi drops at 87%, the laptop goes to sleep, the server caps every connection at a fraction of your bandwidth. Download managers solved this years ago, but the good ones are either paid, closed, or built on a bundled web browser that eats half a gigabyte of memory.

Vibe Download Manager is our answer: a small, native, open-source download manager that does the job well and stays out of the way.

## What it does

- **Splits each file across up to 32 connections.** When one connection finishes its piece, it takes over half of the largest piece still left, so the download never waits on one slow connection. [How that works](/blog/how-multi-connection-downloads-work/).
- **Resumes after anything.** A pause, a crash, a reboot, a dead network. Before it continues, Vibe checks that the file on the server hasn't changed, so you never end up with a file stitched from two versions.
- **Saves video streams.** HLS (`.m3u8`) and DASH (`.mpd`) streams, with quality selection, parallel segment downloads and AES-128 decryption.
- **Works with your browser.** The extension for Chrome, Edge, Brave and Firefox hands downloads to Vibe with the same cookies and referrer the browser would use, so downloads behind a login work too.
- **Keeps things tidy.** Files are sorted into folders by type; the list can be filtered, searched, sorted into categories and managed in bulk.

Queues can run on a schedule, say overnight, and put the computer to sleep or shut it down when they're done. It also checks checksums, can run a virus scanner after each download, supports proxies and per-site logins, and updates itself silently.

## Why build another one?

Three reasons.

**It should be free and open.** A download manager sees every file you fetch and often the cookies that come with them. That's a lot of trust to give software you can't inspect. Vibe's code is public under the MIT or Apache 2.0 license.

**It should be light.** Vibe is a single native program written in Rust. It starts instantly and uses little memory, because it doesn't ship a copy of a web browser to draw its window.

**It should be pleasant.** Light and dark themes that follow your system, twelve languages, resizable columns, keyboard shortcuts, and no nag screens.

## What's next

The [roadmap](@github#roadmap) starts with code signing, the browser extension in the official stores, and packages for macOS and Linux. After that: refreshing expired links, and a details view that shows every connection at work.

[Download Vibe for Windows](/download/) and tell us what you think. Bug reports and ideas are welcome on GitHub.
