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
fig <wallpaper.gif>...
```

## Stopping
```
killall -9 fig
```
