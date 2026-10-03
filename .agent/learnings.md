## 2026-10-02 Measure frame cost headlessly, not via the live wallpaper
**Category:** tooling
**Confidence:** confirmed
**Context:** src/main.rs (main loop), src/frames.rs, src/cache.rs

The user usually has a live `fig` running from `~/.cargo/bin/fig` with GIFs in
`~/.wallpapers/`; check it with `ps`/`top` rather than launching a second one over
the desktop. To profile decode/cache cost headlessly, include `src/frames.rs` and
`src/cache.rs` in a scratch crate via `#[path]` and time them in release mode.
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

## 2026-10-02 Tile-diffing at runtime costs more CPU than it saves
**Category:** architecture
**Confidence:** confirmed
**Context:** src/main.rs (DecodedGif), src/cache.rs

Inside each GIF frame's cropped sub-rect, 83-92% of 32px tiles still match what's on
screen. A correct diff (full-canvas CPU mirror, upload only changed tiles) cut upload
bytes 80-90% and made driver time in `texture.update` negligible, but ran 32% more
instructions and 8% more cycles overall, so it was reverted. The diff pays off only
when done once ahead of time, which is what src/cache.rs does; don't add it back to
the `--no-cache` path.

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

## 2026-10-03 Delta cache: verify against the gif crate's RGBA output
**Category:** tooling
**Confidence:** confirmed
**Context:** src/cache.rs, src/frames.rs

Cached playback cuts cycles ~85% vs decoding (0.49G vs 3.2G per 36s). To test either
path headlessly, include the real modules in a scratch crate with `#[path]`, replay
two full loops into a plain buffer, and compare after every frame with the gif crate's
own `ColorOutput::RGBA` output; this also exercises the loop-wrap record. Keep test
caches out of /tmp (tmpfs here, so they use RAM) or delete them afterwards.
