mod cache;
mod frames;

use cache::DeltaCache;
use crossbeam_channel::{after, bounded, select, Receiver};
use frames::{GifFrames, BYTES_PER_PIXEL};
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, Texture, TextureAccess};
use sdl2::video::Window;
use sdl2_sys::SDL_CreateWindowFrom;
use std::env;
use std::error::Error;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xinerama::query_screens;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

/// How long to wait before retrying a wallpaper whose frame failed to load.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Makes every wallpaper decode its GIF as it plays instead of using a cache.
const NO_CACHE_FLAG: &str = "--no-cache";

/// An animation that plays into a texture one frame at a time.
trait Wallpaper {
    fn size(&self) -> (u32, u32);

    /// Upload the next frame into `texture` and return how long to show it.
    fn upload_next(&mut self, texture: &mut Texture) -> Result<Duration, Box<dyn Error>>;
}

/// Plays a GIF by decoding each frame as it's needed. Only the frame's
/// sub-rect is uploaded, as GIF encoders crop each frame to what changed.
struct DecodedGif {
    frames: GifFrames,
}

impl Wallpaper for DecodedGif {
    fn size(&self) -> (u32, u32) {
        (self.frames.width() as u32, self.frames.height() as u32)
    }

    fn upload_next(&mut self, texture: &mut Texture) -> Result<Duration, Box<dyn Error>> {
        let frame = match self.frames.next()? {
            Some(frame) => frame,
            None => {
                self.frames.rewind()?;
                self.frames.next()?.ok_or("gif has no frames")?
            }
        };
        // An empty frame has nothing to upload, and Rect can't be empty.
        if frame.width > 0 && frame.height > 0 {
            let rect = Rect::new(
                frame.left as i32,
                frame.top as i32,
                frame.width as u32,
                frame.height as u32,
            );
            texture.update(rect, self.frames.pixels(), frame.width * BYTES_PER_PIXEL)?;
        }
        Ok(frame.delay)
    }
}

/// Plays a GIF from its delta cache, uploading only the tiles each frame
/// changes. This avoids decoding during playback.
struct CachedGif {
    cache: DeltaCache,
    next_record: usize,
    // One tile's RGBA pixels, ready to upload.
    tile_pixels: Vec<u8>,
}

impl Wallpaper for CachedGif {
    fn size(&self) -> (u32, u32) {
        (self.cache.width(), self.cache.height())
    }

    fn upload_next(&mut self, texture: &mut Texture) -> Result<Duration, Box<dyn Error>> {
        let record = self.next_record;
        self.next_record = self.cache.following(record);
        for tile in self.cache.tiles(record) {
            tile.expand(&mut self.tile_pixels);
            texture.update(tile.rect, &self.tile_pixels, tile.rect.width() as usize * BYTES_PER_PIXEL)?;
        }
        Ok(self.cache.delay(record))
    }
}

/// Open `gif_path` for playback from its cache, or, if `use_cache` is off
/// or the GIF can't be cached, by decoding it.
fn open_wallpaper(gif_path: &str, use_cache: bool) -> Result<Box<dyn Wallpaper>, Box<dyn Error>> {
    if use_cache {
        match DeltaCache::open(gif_path) {
            Ok(cache) => {
                return Ok(Box::new(CachedGif {
                    cache,
                    next_record: 0,
                    tile_pixels: Vec::new(),
                }))
            }
            Err(e) => eprintln!("Not caching {}: {}; decoding it instead", gif_path, e),
        }
    }
    Ok(Box::new(DecodedGif {
        frames: GifFrames::open(gif_path)?,
    }))
}

fn ctrl_channel() -> Result<Receiver<()>, ctrlc::Error> {
    let (sender, receiver) = bounded(100);
    ctrlc::set_handler(move || {
        let _ = sender.send(());
    })?;

    Ok(receiver)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let use_cache = !args.iter().any(|arg| arg == NO_CACHE_FLAG);
    if use_cache {
        cache::remove_stale();
    }

    let mut wallpapers = {
        let mut wallpapers = Vec::new();

        for gif in args.iter().filter(|arg| *arg != NO_CACHE_FLAG) {
            wallpapers.push(open_wallpaper(gif, use_cache)?);
        }
        wallpapers
    };

    let (conn, screen_num) = x11rb::connect(None)?;

    let screens = query_screens(&conn)?.reply()?;
    let screen_rects = {
        let mut screen_rects = Vec::new();
        for screen in screens.screen_info {
            let rect = Rect::new(
                screen.x_org as i32,
                screen.y_org as i32,
                screen.width as u32,
                screen.height as u32,
            );
            screen_rects.push(rect);
        }
        screen_rects
    };

    let win_id = create_desktop(&conn, screen_num)?;
    set_desktop_atoms(&conn, win_id)?;
    show_desktop(&conn, win_id)?;

    println!("Created {:?}", win_id);

    let mut canvas = create_canvas(win_id)?;
    let texture_creator = canvas.texture_creator();

    let len = wallpapers.len();
    if len == 0 {
        return Err("No wallpapers provided".into());
    }

    let mut textures = {
        let mut textures = Vec::new();
        // One texture per wallpaper, not per screen -- screens that share
        // a wallpaper just blit from the same texture below.
        for wallpaper in &wallpapers {
            let (width, height) = wallpaper.size();
            let mut texture = texture_creator.create_texture(
                PixelFormatEnum::ABGR8888,
                TextureAccess::Streaming,
                width,
                height,
            )?;
            texture.set_blend_mode(sdl2::render::BlendMode::Blend);
            textures.push(texture);
        }
        textures
    };

    let ctrl_c_events = ctrl_channel()?;

    // When each wallpaper's next frame is due. Each wallpaper keeps its
    // own schedule, so GIFs with different frame rates play independently.
    let mut next_frame_at = vec![Instant::now(); len];

    loop {
        let now = Instant::now();
        let mut changed = false;
        // Advance each wallpaper at most once, regardless of how many
        // screens display it.
        for (stack_index, stack) in wallpapers.iter_mut().enumerate() {
            if now < next_frame_at[stack_index] {
                continue;
            }
            match stack.upload_next(&mut textures[stack_index]) {
                Ok(delay) => {
                    // Schedule from the previous due time, not `now`, so
                    // wake-up latency doesn't accumulate as drift. If we've
                    // fallen a whole frame behind (e.g. after a suspend),
                    // resync instead of racing to catch up.
                    let due = next_frame_at[stack_index] + delay;
                    next_frame_at[stack_index] = if due < now { now + delay } else { due };
                    changed = true;
                }
                Err(e) => {
                    eprintln!("frame loading error: {}", e);
                    next_frame_at[stack_index] = now + RETRY_DELAY;
                }
            }
        }

        // Only redraw when a texture changed; between GIF frames the
        // window already shows the right pixels.
        if changed {
            for (screen_index, rect) in screen_rects.iter().enumerate() {
                let texture_index = screen_index % textures.len();
                canvas.copy(&textures[texture_index], None, *rect)?;
            }
            canvas.present();
        }

        // Sleep until the earliest wallpaper is due rather than polling.
        let wake_at = *next_frame_at.iter().min().expect("at least one wallpaper");
        select! {
            recv(after(wake_at.saturating_duration_since(Instant::now()))) -> _ => {}
            recv(ctrl_c_events) -> _ => {
                println!();
                println!("Goodbye!");
                break Ok(());
            }
        }
    }
}

fn create_canvas(win_id: u32) -> Result<Canvas<Window>, Box<dyn std::error::Error>> {
    let sdl_context = sdl2::init()?;
    let video_subsystem = sdl_context.video()?;

    let win = unsafe {
        let sdl_win = SDL_CreateWindowFrom(win_id as *const c_void);
        Window::from_ll(video_subsystem, sdl_win)
    };

    let canvas: Canvas<Window> = win
        .into_canvas()
        .build()
        .expect("failed to build window's canvas");
    Ok(canvas)
}

#[derive(Debug)]
enum X11Error {
    ConnectionError(ConnectionError),
    ReplyError(ReplyError),
}

impl std::error::Error for X11Error {}

impl std::fmt::Display for X11Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            X11Error::ConnectionError(e) => write!(f, "ConnectionError: {}", e),
            X11Error::ReplyError(e) => write!(f, "ReplyError: {}", e),
        }
    }
}

impl From<ConnectionError> for X11Error {
    fn from(e: ConnectionError) -> Self {
        X11Error::ConnectionError(e)
    }
}

impl From<ReplyError> for X11Error {
    fn from(e: ReplyError) -> Self {
        X11Error::ReplyError(e)
    }
}

fn set_desktop_atoms(
    conn: &impl Connection,
    win_id: u32,
) -> Result<VoidCookie<'_, impl Connection>, X11Error> {
    let atom_wm_type = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE")?
        .reply()?
        .atom;
    let atom_wm_desktop = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DESKTOP")?
        .reply()?
        .atom;

    Ok(conn.change_property32(
        PropMode::REPLACE,
        win_id,
        atom_wm_type,
        AtomEnum::ATOM,
        &[atom_wm_desktop],
    )?)
}

fn show_desktop(conn: &impl Connection, win_id: u32) -> Result<(), ConnectionError> {
    conn.map_window(win_id)?;
    conn.flush()
}

fn create_desktop(conn: &impl Connection, screen_num: usize) -> Result<u32, ReplyOrIdError> {
    let screen = &conn.setup().roots[screen_num];
    let width = screen.width_in_pixels;
    let height = screen.height_in_pixels;
    let win_id = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win_id,
        screen.root,
        0,
        0,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(screen.white_pixel),
    )?;

    Ok(win_id)
}
