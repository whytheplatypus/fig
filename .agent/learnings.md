## 2026-10-02 Measure frame cost headlessly, not via the live wallpaper
**Category:** tooling
**Confidence:** confirmed
**Context:** src/main.rs (Stack, main loop)

The user usually has a live `fig` running from `~/.cargo/bin/fig` with GIFs in
`~/.wallpapers/`; check it with `ps`/`top` rather than launching a second one over
the desktop. To profile decode/tiling cost, copy `Stack` into a scratch crate that
depends only on `gif` and `sdl2` (for `Rect`) and time `next()` in release mode.
No Xvfb here, so end-to-end runs go on the real display: use `timeout -s INT` and
find the pid with `pgrep -nf '^<binary path> '` (plain `pgrep -f` also matches the
wrapper shell and the live instance).

## 2026-10-02 Remaining CPU is in the NVIDIA GL driver, not our code
**Category:** architecture
**Confidence:** suspected
**Context:** src/main.rs (main loop, upload)

After present-only-on-change and deadline sleeping, two waneella GIFs on two
monitors cost ~11% of a core: ~2.5% decoding, the rest presumably texture upload
and present in the driver. `__GL_YIELD=USLEEP` and `SDL_RENDER_DRIVER=opengles2`
made no difference. Unverified without a profiler (`perf` isn't installed).

## 2026-10-02 GIF sub-frames are already cropped; tile diffing barely helps
**Category:** architecture
**Confidence:** confirmed
**Context:** src/main.rs (process_frame, unchanged)

Real GIFs (e.g. the waneella ones) already encode each frame as a cropped sub-rect
(~65% of the canvas). On those files the 32px tile diff skipped ~1% of tiles but
tripled per-frame CPU (~1 ms decode → ~3 ms). Before adding pixel-diffing back,
measure it against uploading `frame.buffer` straight to the sub-rect.
