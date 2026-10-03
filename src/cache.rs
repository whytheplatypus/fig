//! A per-GIF cache of the tiles each frame changes. It's built once by
//! decoding the GIF, then memory-mapped for playback, so playing a frame
//! needs no decompression or diffing.
//!
//! Layout (little-endian):
//!   header: MAGIC, width u32, height u32
//!   records, back to back until the end of the file:
//!     tile count u32, delay in ms u32, then each tile:
//!       column u16, row u16, color count u16, then either
//!       color count RGBA colors and one palette index per pixel, or,
//!       if color count is 0, RGBA per pixel.
//!
//! Record 0 is the whole first frame, records 1..n-1 are the rest of the
//! frames, and record n replays frame 0 over the last frame, so looping
//! plays 0, 1, ..., n, 1, ..., n, ...

use crate::frames::{Frame, GifFrames, BYTES_PER_PIXEL};
use memmap2::Mmap;
use sdl2::rect::Rect;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Width and height, in pixels, of the squares the cache stores.
const TILE: usize = 32;
/// Hashed into cache names; bump it when the layout changes.
const FORMAT_VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"FIGDELTA";
const HEADER_LEN: usize = 8 + 4 + 4;
const TILE_HEADER_LEN: usize = 2 + 2 + 2;
/// Caches unused for this long are deleted by `remove_stale`.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub struct DeltaCache {
    map: Mmap,
    width: usize,
    height: usize,
    // Byte offset of each record in `map`.
    records: Vec<usize>,
}

/// One changed tile of a record.
pub struct Tile<'a> {
    pub rect: Rect,
    // RGBA palette for `pixels`, or empty if `pixels` is already RGBA.
    colors: &'a [u8],
    pixels: &'a [u8],
}

impl DeltaCache {
    /// Open the cache for `gif_path`, building it first if there isn't one
    /// for the GIF's current contents.
    pub fn open(gif_path: &str) -> Result<Self, Box<dyn Error>> {
        let cache_path = cache_path(gif_path)?;
        if !cache_path.exists() {
            println!("Caching {} in {}", gif_path, cache_path.display());
            build(gif_path, &cache_path)?;
        }
        // Mark the cache as used, so `remove_stale` keeps it.
        File::options()
            .write(true)
            .open(&cache_path)?
            .set_modified(SystemTime::now())?;
        load(&cache_path)
    }

    pub fn width(&self) -> u32 {
        self.width as u32
    }

    pub fn height(&self) -> u32 {
        self.height as u32
    }

    /// The record to play after `record`.
    pub fn following(&self, record: usize) -> usize {
        if record + 1 < self.records.len() {
            record + 1
        } else {
            1
        }
    }

    pub fn delay(&self, record: usize) -> Duration {
        Duration::from_millis(self.u32_at(self.records[record] + 4) as u64)
    }

    /// The tiles `record` changes.
    pub fn tiles(&self, record: usize) -> impl Iterator<Item = Tile<'_>> {
        let offset = self.records[record];
        let mut at = offset + 8;
        (0..self.u32_at(offset)).map(move |_| {
            let (tile, next) = self
                .read_tile(at)
                .expect("cache records are validated when loaded");
            at = next;
            tile
        })
    }

    /// Read the tile starting at byte `at`, and where the next one starts,
    /// or `None` if it's malformed.
    fn read_tile(&self, at: usize) -> Option<(Tile<'_>, usize)> {
        let header = self.map.get(at..at + TILE_HEADER_LEN)?;
        let x = u16::from_le_bytes([header[0], header[1]]) as usize * TILE;
        let y = u16::from_le_bytes([header[2], header[3]]) as usize * TILE;
        let color_count = u16::from_le_bytes([header[4], header[5]]) as usize;
        if x >= self.width || y >= self.height || color_count > 256 {
            return None;
        }
        let (width, height) = (TILE.min(self.width - x), TILE.min(self.height - y));
        let colors_start = at + TILE_HEADER_LEN;
        let pixels_start = colors_start + color_count * BYTES_PER_PIXEL;
        let pixels_len = match color_count {
            0 => width * height * BYTES_PER_PIXEL,
            _ => width * height,
        };
        let tile = Tile {
            rect: Rect::new(x as i32, y as i32, width as u32, height as u32),
            colors: self.map.get(colors_start..pixels_start)?,
            pixels: self.map.get(pixels_start..pixels_start + pixels_len)?,
        };
        Some((tile, pixels_start + pixels_len))
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.map[at..at + 4].try_into().unwrap())
    }
}

impl Tile<'_> {
    /// Write the tile's RGBA pixels, row by row, into `rgba`.
    pub fn expand(&self, rgba: &mut Vec<u8>) {
        rgba.clear();
        if self.colors.is_empty() {
            rgba.extend_from_slice(self.pixels);
            return;
        }
        // Indices without a color (only in a corrupt cache) stay transparent.
        let mut table = [[0; BYTES_PER_PIXEL]; 256];
        for (entry, color) in table.iter_mut().zip(self.colors.chunks_exact(BYTES_PER_PIXEL)) {
            entry.copy_from_slice(color);
        }
        rgba.resize(self.pixels.len() * BYTES_PER_PIXEL, 0);
        for (pixel, &index) in rgba.chunks_exact_mut(BYTES_PER_PIXEL).zip(self.pixels) {
            pixel.copy_from_slice(&table[index as usize]);
        }
    }
}

/// Delete caches that haven't been used for `MAX_AGE`, and temporary
/// files left by interrupted builds. Best effort: errors are ignored.
pub fn remove_stale() {
    let Some(entries) = cache_dir().and_then(|dir| fs::read_dir(dir).ok()) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        let max_age = match path.extension().and_then(|extension| extension.to_str()) {
            Some("delta") => MAX_AGE,
            // Old enough that no running build still owns it.
            Some("tmp") => Duration::from_secs(24 * 60 * 60),
            _ => continue,
        };
        let age = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok());
        if age.is_some_and(|age| age > max_age) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
        .map(|dir| dir.join("fig"))
}

/// Where the cache for `gif_path` lives. The name hashes the GIF's path,
/// size and modification time, so an edited GIF gets a fresh cache. (The
/// hash may also change with the Rust version; orphaned caches age out.)
fn cache_path(gif_path: &str) -> Result<PathBuf, Box<dyn Error>> {
    let metadata = fs::metadata(gif_path)?;
    let mut hasher = DefaultHasher::new();
    FORMAT_VERSION.hash(&mut hasher);
    fs::canonicalize(gif_path)?.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata.modified()?.hash(&mut hasher);
    let dir = cache_dir().ok_or("no cache directory: set HOME or XDG_CACHE_HOME")?;
    Ok(dir.join(format!("{:016x}.delta", hasher.finish())))
}

fn load(cache_path: &Path) -> Result<DeltaCache, Box<dyn Error>> {
    let corrupt = || format!("{} is corrupt; delete it to rebuild", cache_path.display());
    let file = File::open(cache_path)?;
    // Safety: caches are written under a temporary name and renamed into
    // place, so fig never modifies a cache that's mapped.
    let map = unsafe { Mmap::map(&file)? };
    if map.len() < HEADER_LEN || &map[..8] != MAGIC {
        return Err(corrupt().into());
    }
    let mut cache = DeltaCache {
        width: u32::from_le_bytes(map[8..12].try_into().unwrap()) as usize,
        height: u32::from_le_bytes(map[12..16].try_into().unwrap()) as usize,
        map,
        records: Vec::new(),
    };
    // Check every record now, so playback can't run off the end of the map.
    let mut at = HEADER_LEN;
    while at < cache.map.len() {
        if cache.map.len() < at + 8 {
            return Err(corrupt().into());
        }
        cache.records.push(at);
        let tile_count = cache.u32_at(at);
        at += 8;
        for _ in 0..tile_count {
            at = cache.read_tile(at).ok_or_else(corrupt)?.1;
        }
    }
    if cache.records.len() < 2 {
        return Err(corrupt().into());
    }
    Ok(cache)
}

fn build(gif_path: &str, cache_path: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(cache_path.parent().unwrap())?;
    // Unique per process, so two figs building the same cache don't collide.
    let temp_path = cache_path.with_extension(format!("{}.tmp", std::process::id()));
    let result = write_cache(gif_path, &temp_path)
        .and_then(|()| fs::rename(&temp_path, cache_path).map_err(Into::into));
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_cache(gif_path: &str, path: &Path) -> Result<(), Box<dyn Error>> {
    let mut frames = GifFrames::open(gif_path)?;
    let mut screen = Screen::new(frames.width(), frames.height());
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(MAGIC)?;
    out.write_all(&(frames.width() as u32).to_le_bytes())?;
    out.write_all(&(frames.height() as u32).to_le_bytes())?;

    let mut first_frame_screen = None;
    while let Some(frame) = frames.next()? {
        screen.draw(&frame, frames.pixels());
        screen.write_changes(&mut out, frame.delay)?;
        if first_frame_screen.is_none() {
            first_frame_screen = Some(screen.pixels.clone());
        }
    }
    let first_frame_screen = first_frame_screen.ok_or("gif has no frames")?;

    // The loop record: frame 0 drawn over the last frame.
    frames.rewind()?;
    let frame = frames.next()?.ok_or("gif has no frames")?;
    screen.draw(&frame, frames.pixels());
    screen.write_changes(&mut out, frame.delay)?;
    // Records 1.. were diffed against the first pass's frame 0, so they
    // only hold on later loops if replaying frame 0 restores that screen.
    if screen.pixels != first_frame_screen {
        return Err("its first frame doesn't cover the whole image".into());
    }

    out.into_inner()?.sync_all()?;
    Ok(())
}

/// What fig's texture holds after each frame, and which tiles changed.
struct Screen {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    // Tiles per row of the image.
    columns: usize,
    // Tiles changed since the last record, in row-major order.
    changed: Vec<bool>,
    // Scratch space for palettizing a tile.
    palette: HashMap<[u8; BYTES_PER_PIXEL], u8>,
    colors: Vec<u8>,
    indices: Vec<u8>,
}

impl Screen {
    fn new(width: usize, height: usize) -> Self {
        let columns = width.div_ceil(TILE);
        Screen {
            width,
            height,
            pixels: vec![0; width * height * BYTES_PER_PIXEL],
            columns,
            // The texture starts undefined, so the first record holds
            // every tile.
            changed: vec![true; columns * height.div_ceil(TILE)],
            palette: HashMap::new(),
            colors: Vec::new(),
            indices: Vec::new(),
        }
    }

    /// Draw a frame's RGBA `pixels` onto the screen.
    fn draw(&mut self, frame: &Frame, pixels: &[u8]) {
        let row_len = frame.width * BYTES_PER_PIXEL;
        for (y, row) in pixels.chunks_exact(row_len.max(1)).enumerate() {
            self.write_row(frame.left, frame.top + y, row);
        }
    }

    /// Copy a row of RGBA pixels in at (`x`, `y`), noting which tiles change.
    fn write_row(&mut self, x: usize, y: usize, row: &[u8]) {
        let end = x + row.len() / BYTES_PER_PIXEL;
        let mut start = x;
        // Compare one tile-wide segment at a time.
        while start < end {
            let stop = ((start / TILE + 1) * TILE).min(end);
            let new = &row[(start - x) * BYTES_PER_PIXEL..(stop - x) * BYTES_PER_PIXEL];
            let offset = (y * self.width + start) * BYTES_PER_PIXEL;
            let old = &mut self.pixels[offset..offset + new.len()];
            if old != new {
                old.copy_from_slice(new);
                self.changed[(y / TILE) * self.columns + start / TILE] = true;
            }
            start = stop;
        }
    }

    /// Write a record of the tiles changed since the last one.
    fn write_changes(&mut self, out: &mut impl Write, delay: Duration) -> io::Result<()> {
        let tiles: Vec<usize> = (0..self.changed.len()).filter(|&tile| self.changed[tile]).collect();
        out.write_all(&(tiles.len() as u32).to_le_bytes())?;
        out.write_all(&(delay.as_millis() as u32).to_le_bytes())?;
        for tile in tiles {
            self.write_tile(out, tile)?;
            self.changed[tile] = false;
        }
        Ok(())
    }

    /// Write one tile, with its own palette if it has at most 256 colors.
    fn write_tile(&mut self, out: &mut impl Write, tile: usize) -> io::Result<()> {
        let (column, row) = (tile % self.columns, tile / self.columns);
        let (x, y) = (column * TILE, row * TILE);
        let row_len = TILE.min(self.width - x) * BYTES_PER_PIXEL;
        let rows = y..(y + TILE).min(self.height);

        self.palette.clear();
        self.colors.clear();
        self.indices.clear();
        let mut fits = true;
        'rows: for pixel_row in rows.clone() {
            let offset = (pixel_row * self.width + x) * BYTES_PER_PIXEL;
            for pixel in self.pixels[offset..offset + row_len].chunks_exact(BYTES_PER_PIXEL) {
                let color: [u8; BYTES_PER_PIXEL] = pixel.try_into().unwrap();
                let index = match self.palette.get(&color) {
                    Some(&index) => index,
                    None if self.palette.len() < 256 => {
                        let index = self.palette.len() as u8;
                        self.palette.insert(color, index);
                        self.colors.extend_from_slice(&color);
                        index
                    }
                    None => {
                        fits = false;
                        break 'rows;
                    }
                };
                self.indices.push(index);
            }
        }

        let color_count = if fits { self.palette.len() as u16 } else { 0 };
        out.write_all(&(column as u16).to_le_bytes())?;
        out.write_all(&(row as u16).to_le_bytes())?;
        out.write_all(&color_count.to_le_bytes())?;
        if fits {
            out.write_all(&self.colors)?;
            out.write_all(&self.indices)?;
        } else {
            for pixel_row in rows {
                let offset = (pixel_row * self.width + x) * BYTES_PER_PIXEL;
                out.write_all(&self.pixels[offset..offset + row_len])?;
            }
        }
        Ok(())
    }
}
