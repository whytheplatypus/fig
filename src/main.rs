use crossbeam_channel::{after, bounded, select, Receiver};
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, Texture, TextureAccess};
use sdl2::video::Window;
use sdl2_sys::SDL_CreateWindowFrom;
use std::env;
use std::ffi::c_void;
use std::fs::File;
use std::io::BufReader;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xinerama::query_screens;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

const BYTES_PER_PIXEL: usize = 4;

/// How long to wait before retrying a wallpaper whose frame failed to load.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Streams a GIF one frame at a time instead of decoding the whole file
/// up front.
struct Stack {
    decoder: gif::Decoder<BufReader<File>>,
    file_path: String,
    width: u32,
    height: u32,
}

impl Stack {
    fn open(gif_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let decoder = open_decoder(gif_path)?;
        let width = decoder.width() as u32;
        let height = decoder.height() as u32;
        Ok(Stack {
            decoder,
            file_path: gif_path.to_string(),
            width,
            height,
        })
    }

    /// Decode the next frame into `texture` and return how long to show
    /// it, looping back to the start of the file once the GIF ends.
    /// `gif::Decoder` can't rewind, so looping means reopening the file --
    /// the OS page cache keeps that cheap after the first pass.
    fn upload_next(&mut self, texture: &mut Texture) -> Result<Duration, Box<dyn std::error::Error>> {
        if let Some(frame) = self.decoder.read_next_frame()? {
            return upload(frame, texture);
        }
        self.decoder = open_decoder(&self.file_path)?;
        let frame = self
            .decoder
            .read_next_frame()?
            .ok_or("gif has no frames")?;
        upload(frame, texture)
    }
}

/// Copy `frame` into its sub-rect of `texture` and return how long the
/// frame should be shown. GIF encoders already crop each frame to the
/// region that changed, so no further diffing is worth doing here.
fn upload(frame: &gif::Frame, texture: &mut Texture) -> Result<Duration, Box<dyn std::error::Error>> {
    let rect = Rect::new(
        frame.left as i32,
        frame.top as i32,
        frame.width as u32,
        frame.height as u32,
    );
    texture.update(rect, &frame.buffer, frame.width as usize * BYTES_PER_PIXEL)?;
    // Floor the delay so a GIF frame with delay=0 (common in poorly
    // optimized files) can't cause a runaway-fast loop.
    let centiseconds = frame.delay.max(2);
    Ok(Duration::from_millis(centiseconds as u64 * 10))
}

fn open_decoder(gif_path: &str) -> Result<gif::Decoder<BufReader<File>>, Box<dyn std::error::Error>> {
    let file_in = BufReader::new(
        File::open(gif_path)
            .map_err(|e| format!("Failed to open GIF file '{}': {}", gif_path, e))?,
    );
    let mut decoder = gif::DecodeOptions::new();
    decoder.set_color_output(gif::ColorOutput::RGBA);
    decoder
        .read_info(file_in)
        .map_err(|e| format!("Failed to read GIF info from '{}': {}", gif_path, e).into())
}

fn ctrl_channel() -> Result<Receiver<()>, ctrlc::Error> {
    let (sender, receiver) = bounded(100);
    ctrlc::set_handler(move || {
        let _ = sender.send(());
    })?;

    Ok(receiver)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    let gifs = &args[1..args.len()];
    let mut wallpapers = {
        let mut wallpapers = Vec::new();

        for gif in gifs {
            wallpapers.push(Stack::open(gif)?);
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
            let mut texture = texture_creator.create_texture(
                PixelFormatEnum::ABGR8888,
                TextureAccess::Streaming,
                wallpaper.width,
                wallpaper.height,
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
