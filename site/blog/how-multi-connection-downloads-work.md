+++
title = "Why multi-connection downloads are faster"
description = "Why a download accelerator can be several times faster than your browser: range requests, per-connection limits, and splitting the remaining work dynamically."
date = "2026-10-23"
tags = ["how it works", "download accelerator", "speed"]
+++

Your browser downloads a file over one connection, from the first byte to the last. A download accelerator like Vibe opens several connections to the same file at once, each fetching a different part. On many servers that's dramatically faster. Here's why, and how Vibe keeps every connection busy until the very end.

## Why one connection is often slow

A single connection rarely uses all of your bandwidth:

- **Servers and CDNs limit each connection.** Many cap the speed per connection to share capacity fairly between visitors.
- **Distance hurts one connection more than many.** TCP speeds up gradually and backs off when packets are lost. Over long distances, one connection spends much of its time recovering, while several connections fill the gaps for each other.
- **Busy servers answer in bursts.** Several parallel requests smooth that out.

If the server limits each connection to 5 MB/s and your line can do 50 MB/s, ten connections can get you close to the full 50.

## Range requests: asking for part of a file

HTTP lets a client ask for a slice of a file with a `Range` header:

```
GET /ubuntu.iso HTTP/1.1
Range: bytes=1000000-1999999
```

A server that supports this answers `206 Partial Content` with just those bytes. Vibe first probes the file: its size, whether ranges are allowed, and its validators (`ETag` and `Last-Modified`). If ranges work, it splits the file into pieces and gives each connection one. If they don't, it falls back to a single connection, which can still resume if the server allows it.

## The slow-piece problem

Splitting a file into, say, eight equal pieces sounds simple. But connections don't run at the same speed. With fixed pieces, the fast connections finish early and sit idle while one slow connection crawls through the last piece, and your download runs at the speed of the slowest connection for the final stretch.

## Dynamic splitting

Vibe doesn't let a connection sit idle. When a connection finishes its piece, it looks for the piece with the most bytes still left, splits the remaining part in two, and takes the second half. The slow connection keeps going on the first half and now has much less to do.

Repeat that every time a connection frees up and all connections stay busy until the file is nearly done. The pieces get smaller and smaller toward the end, which is exactly when you want the most help. The animation on the [home page](/) shows this happening.

Two details make it safe:

- **Bytes in flight are reserved.** A connection may have read data it hasn't written yet. Splits happen after those bytes, so two connections never write the same part of the file.
- **Each piece is written to its exact place** in a pre-allocated file, and progress is saved as it goes. That's what lets a download resume after a crash: Vibe knows which bytes of each piece are already on disk.

## When more connections don't help

- **The server doesn't support ranges.** Then there's only one connection to use.
- **Your line is the bottleneck.** If one connection already fills it, more just add overhead.
- **The server dislikes it.** Some servers refuse or slow down clients with many connections. Vibe retries politely and lets you lower the number per download.

The default of eight connections is a good balance. Raise it in Settings for fast lines and well-behaved servers; lower it for fragile ones.

## Resuming safely

When you resume, Vibe sends the saved `ETag` or `Last-Modified` with an `If-Range` header. If the file on the server changed, the server sends the whole new file instead of a slice, and Vibe starts over. That's how it avoids the classic broken download: half of version 1 glued to half of version 2.

Want to see it on your own downloads? [Get Vibe for Windows](/download/).
