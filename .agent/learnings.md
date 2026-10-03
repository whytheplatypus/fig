## 2026-10-02 Measure frame cost headlessly, not via the live wallpaper
**Category:** tooling
**Confidence:** confirmed
**Context:** src/main.rs (Stack, main loop)

The user usually has a live `fig` running from `~/.cargo/bin/fig` with GIFs in
`~/.wallpapers/`; check it with `ps`/`top` rather than launching a second one over
the desktop. To profile decode/upload cost, copy `Stack` into a scratch crate that
depends only on `gif` and `sdl2` (for `Rect`) and time it in release mode.
No Xvfb here, so end-to-end runs go on the real display: use `timeout -s INT` and
find the pid with `pgrep -nf '^<binary path> '` (plain `pgrep -f` also matches the
wrapper shell and the live instance).

## 2026-10-02 End-to-end CPU numbers are noisy; compare in alternating pairs
**Category:** tooling
**Confidence:** confirmed
**Context:** perf stat / perf record on fig

Run-to-run CPU varies by ±3 points depending on which part of the GIFs plays and CPU
clock state, and headless microbenchmarks run ~3x faster per frame than the idle-clocked
live process. Measure over a full GIF loop (~36s for the waneella pair) and alternate
old/new runs. RSS swings ~70MB from the NVIDIA driver; compare `RssAnon` instead.

## 2026-10-02 Tile-diffing uploads costs more CPU than it saves
**Category:** architecture
**Confidence:** confirmed
**Context:** src/main.rs (Stack::upload_next)

Inside each GIF frame's cropped sub-rect, 83-92% of 32px tiles still match what's on
screen. A correct diff (full-canvas CPU mirror, upload only changed tiles) cut upload
bytes 80-90% and made driver time in `texture.update` negligible, but ran 32% more
instructions and 8% more cycles overall, so it was reverted. Don't retry it to save
CPU; it would only be worth it to cut GPU/bus traffic. Remaining CPU is mostly
weezl's LZW decode, which is inherent to streaming.

## 2026-10-02 Use cycles:u, not task-clock, to compare CPU work
**Category:** tooling
**Confidence:** confirmed
**Context:** perf stat on fig

The CPU clocks down as fig does less work (~1.1 GHz average), so a build that saves
work can show the same `task-clock`/CPU% while unchanged code (e.g. weezl) appears
slower. Compare `perf stat -e cycles:u,instructions:u` across alternating runs.

## 2026-10-02 sdl2 Texture::update doesn't bounds-check pixel_data
**Category:** gotcha
**Confidence:** confirmed
**Context:** sdl2-0.35.2 render.rs, Texture::update

It passes the raw pointer straight to SDL_UpdateTexture, so a slice shorter than
`pitch * (rect.height - 1) + rect.width * 4` reads out of bounds instead of erroring.
Validate GIF frame rects against the canvas before uploading from a sub-slice.
