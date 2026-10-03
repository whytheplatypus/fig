# fig
An animated wallpaper tool for gifs

![desktop](https://user-images.githubusercontent.com/410846/204144644-c3dfdf91-3512-43a5-a91c-1f2ce2e158b5.gif)
gifs in this example are by [waneella](https://www.patreon.com/waneella)

Using [paperview](https://github.com/glouw/paperview) as a prompt this is the second tool in the series for practicing writting the same behavior multiple times and in different languages.
The first was [zaper](https://github.com/whytheplatypus/zaper).
The goal for this round is to accept gif files without any preprocessing needed.

## Dependencies:
You may need `sdl2_image` in order to build.

On Arch: `sudo pacman -S sdl2_image`

On something with apt: `apt install libsdl2-image-dev`

## Building:
```
cargo build
```

## Installation:
```
cargo install --git https://github.com/whytheplatypus/fig.git
```

## Usage
```
fig [--no-cache] <wallpaper.gif>...
```

## Caching
The first time fig plays a GIF it decodes it once into a cache in
`$XDG_CACHE_HOME/fig` (or `~/.cache/fig`), storing only the parts of each frame
that change. Playing from the cache takes a fraction of the CPU of decoding,
at the cost of some disk space (roughly 10-80MB per GIF). Caches that haven't
been used for 30 days are deleted when fig starts.

`--no-cache` decodes the GIFs while they play instead, using no disk space.
GIFs whose first frame doesn't cover the whole image are always decoded.

## Stopping
```
killall -9 fig
```
