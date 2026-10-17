# flux-preview

QuickLook for Linux: press space in a file manager, see the file. A spinoff of flux, built on the
rules in [ZAP.txt](ZAP.txt).

Previews images, text, HTML (WebKitGTK, JS and network off), PDF (page by page), video and audio
(GStreamer), with an info page for everything else.

## Use

```sh
nix-shell
cargo build --release
target/release/flux-preview --index 2 a.png b.txt c.pdf   # toggle the preview of c.pdf
```

Keys: <kbd>Space</kbd>/<kbd>Esc</kbd> hide, <kbd>←</kbd>/<kbd>→</kbd> previous/next file,
<kbd>Enter</kbd> open in the default app, <kbd>Ctrl</kbd>+<kbd>Q</kbd> quit the daemon.

Images and PDFs zoom and pan:

| | Images | PDFs |
|---|---|---|
| Zoom | wheel, pinch, <kbd>+</kbd>/<kbd>-</kbd> | <kbd>Ctrl</kbd>+wheel, pinch, <kbd>+</kbd>/<kbd>-</kbd> |
| Fit / 100% | <kbd>0</kbd> / <kbd>1</kbd>, double-click toggles | same (double-click: fit ↔ 2×) |
| Pan | drag, touchpad scroll, <kbd>↑</kbd><kbd>↓</kbd> <kbd>j</kbd><kbd>k</kbd>, <kbd>Shift</kbd>+<kbd>←</kbd><kbd>→</kbd> | drag, wheel, same keys |
| Pages | | <kbd>PgUp</kbd>/<kbd>PgDn</kbd>, <kbd>Home</kbd>/<kbd>End</kbd>, scroll past the edge |

<kbd>←</kbd>/<kbd>→</kbd> always switch files, zoomed or not.

## How it stays fast

- **Two binaries.** `flux-preview` is a std-only client (~2 ms). If a daemon is listening on
  `$XDG_RUNTIME_DIR/flux-preview.sock`, it sends the request there and exits. If not, it execs
  `flux-preview-daemon` with the same arguments, and that process stays resident with its window
  hidden. Linking GTK/WebKit/GStreamer into the client would cost ~50 ms before `main` runs.
- **No GApplication id.** Single-instance comes from our own socket, not D-Bus (~200 ms, ZAP §1).
- **Show first.** The file name appears right away, then the cached freedesktop thumbnail, then the
  real preview. A spinner appears only if nothing has arrived after 150 ms.
- **Session ids.** Every show/step/hide bumps one id; work that comes back late is dropped. Stepping
  through files is debounced by 40 ms.
- **Off the GTK thread.** Sniffing, decoding (images scaled to the window), PDF rendering, tags and
  cover art all run on a capped pool. The UI thread only uploads `MemoryTexture`s.
- **Sharper only when zoomed.** Zoom is one transform in `snapshot()`, never a relayout. 150 ms
  after zooming stops, if the screen shows more pixels than the texture has, the full image
  (≤ 64 MP) or a re-render of the page (≤ 8192 px) replaces it in place. Page flips and zoom
  renders carry their own generation id on top of the session id.
- **Lazy pages.** Each viewer is built the first time it's needed. The WebView is warmed 75 ms
  after startup.
- **Measured.** `hit!()` probes from flux (`FLUX_PREVIEW_HWGA_OUT=auto|path`, debug builds), and
  `[slow AppMsg]` is logged for any message taking over 32 ms.

## Layout

```
src/main.rs, cli.rs, ipc.rs   client + shared std-only lib
src/daemon/main.rs, app.rs    daemon entry, relm4 window component
src/daemon/kind.rs            extension → 4 KiB sniff classification
src/daemon/load/              pool-side loaders (image, text, pdf, audio)
src/daemon/view/              lazily built pages (image, text, media, web, info); zoom.rs
src/daemon/thumbcache.rs      freedesktop thumbnail lookup
src/daemon/hwga/              probes copied from flux
```

## Known issues

- **Some ID3-tagged MP3s abort the daemon** inside GStreamer 1.28's decodebin3
  (`mq_slot_handle_stream_start: assertion failed: (collection)`). This is an upstream race in the
  playbin3 pipeline GTK's media backend uses, and GstPlay has no switch back to playbin2. The next
  invocation starts a fresh daemon. The real fix is the media subprocess below.

## TODO

- Run risky decoders (media, PDF) in a subprocess so a crash can't take the window down
- Neighbour prefetch (i±1) + byte-bounded texture LRU
- Keep the thumbnail on screen until the first video frame arrives
- Syntax highlighting (gtksourceview5) and Markdown rendering
- Animated GIF/WebP
- PDF: prefetch pages n±1; keep the `PopplerDocument` open between renders
- Tiled rendering for zooms past the 64 MP / 8192 px caps
- Replace the dead-proxy network block with a compiled WebKit content filter
- Validate thumbnails via `Thumb::MTime`
- flux integration: call `flux-preview --index N <selection>` on space
