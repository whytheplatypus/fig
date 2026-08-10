use crossbeam_channel::{bounded, select, tick, Receiver};
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureAccess};
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

#[derive(Clone)]
struct Square {
    rect: Rect,
    pitch: usize,
    pixels: Vec<u8>,
}

struct RawFrame {
    delay: u32,
    squares: Vec<Square>,
}

const DIMENSION: usize = 32;

/// Streams a GIF one frame at a time instead of decoding the whole file
/// up front. Only the previous frame's squares are kept around (to skip
/// re-uploading pixels that didn't change), not the full frame history.
struct Stack {
    decoder: gif::Decoder<BufReader<File>>,
    file_path: String,
    width: u32,
    height: u32,
    previous_squares: Vec<Square>,
    // Centiseconds, per the GIF spec. 0 means "no frame shown yet".
    last_frame_delay: u32,
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
            previous_squares: Vec::new(),
            last_frame_delay: 0,
        })
    }

    /// Decode and return the next frame, looping back to the start of the
    /// file once the GIF ends. `gif::Decoder` can't rewind, so looping
    /// means reopening the file -- the OS page cache keeps that cheap
    /// after the first pass.
    fn next(&mut self) -> Result<RawFrame, Box<dyn std::error::Error>> {
        // `read_next_frame()` returns a reference borrowed from
        // `self.decoder`, so pull everything we need out into owned data
        // here first -- that lets the borrow end before we call
        // `process_frame`, which needs its own `&mut self`.
        let frame_data = self.read_frame_data()?;
        let raw_frame = self.process_frame(frame_data);
        self.last_frame_delay = raw_frame.delay;
        Ok(raw_frame)
    }

    fn read_frame_data(&mut self) -> Result<FrameData, Box<dyn std::error::Error>> {
        let frame = match self.decoder.read_next_frame()? {
            Some(frame) => frame,
            None => {
                self.decoder = open_decoder(&self.file_path)?;
                self.previous_squares.clear();
                self.decoder
                    .read_next_frame()?
                    .ok_or("gif has no frames")?
            }
        };
        Ok(FrameData {
            delay: frame.delay,
            left: frame.left,
            top: frame.top,
            width: frame.width,
            height: frame.height,
            pixels: frame.buffer.to_vec(),
        })
    }

    fn process_frame(&mut self, frame: FrameData) -> RawFrame {
        let bytes_per_pixel = 4;
        // Floor the delay so a GIF frame with delay=0 (common in poorly
        // optimized files) can't cause a runaway-fast loop or, in the
        // timing check below, a divide/modulo-by-zero.
        let delay = (frame.delay as u32).max(2);
        let pixels = &frame.pixels;
        let pitch = frame.width as usize * bytes_per_pixel;
        let mut squares = Vec::new();

        for y in (0..frame.height).step_by(DIMENSION) {
            let height = if y + DIMENSION as u16 > frame.height {
                frame.height - y
            } else {
                DIMENSION as u16
            };
            for x in (0..frame.width).step_by(DIMENSION) {
                let width = if x + DIMENSION as u16 > frame.width {
                    frame.width - x
                } else {
                    DIMENSION as u16
                };
                let rect = Rect::new(
                    (frame.left + x) as i32,
                    (frame.top + y) as i32,
                    width as u32,
                    height as u32,
                );
                match chunk_frame(
                    y.into(),
                    x.into(),
                    width.into(),
                    height.into(),
                    pitch,
                    bytes_per_pixel,
                    &pixels,
                ) {
                    Ok(chunk) => {
                        let square = Square {
                            rect,
                            pitch: width as usize * bytes_per_pixel,
                            pixels: chunk,
                        };
                        // Skip squares whose pixels are identical to the
                        // same position in the previous frame -- the SDL
                        // texture already holds the right pixels there.
                        // Unlike the original check_square(), this only
                        // looks at the immediately previous frame (O(1)
                        // memory) instead of scanning the GIF's entire
                        // decoded history.
                        if !unchanged(&self.previous_squares, &square) {
                            squares.push(square);
                        }
                    }
                    Err(e) => {
                        eprintln!("chunk error: {}", e);
                        continue;
                    }
                }
            }
        }

        self.previous_squares = squares.clone();
        RawFrame { delay, squares }
    }
}

struct FrameData {
    delay: u16,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    pixels: Vec<u8>,
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

/// True if `square` is pixel-identical to the same-rect square in
/// `previous`. Bails out (false) as soon as it hits a previous square
/// that either has a different rect at the same grid slot or overlaps
/// without matching exactly, since that means the grid alignment shifted
/// between frames (GIF sub-frames can have different left/top offsets).
fn unchanged(previous: &[Square], square: &Square) -> bool {
    for previous_square in previous {
        if previous_square.rect == square.rect {
            return previous_square.pixels == square.pixels;
        }
        if previous_square.rect.has_intersection(square.rect) {
            return false;
        }
    }
    false
}

fn chunk_frame(
    top: usize,
    left: usize,
    width: usize,
    height: usize,
    pitch: usize,
    bytes_per_pixel: usize,
    raw_pixels: &Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut y = top;
    let local_pitch = width * bytes_per_pixel;
    let mut pixel_square = Vec::new();
    while y < height + top {
        let start = y * pitch + left * bytes_per_pixel;
        if start > raw_pixels.len() {
            return Err("bad start value".into());
        }
        let end = start + local_pitch;
        if end > raw_pixels.len() {
            return Err("bad end value".into());
        }
        let pixel_row = &raw_pixels[start..end];
        pixel_square.extend_from_slice(pixel_row);
        y += 1;
    }
    if pixel_square.len() % (width * bytes_per_pixel) != 0 || pixel_square.len() == 0 {
        return Err("bad pixel square".into());
    }
    Ok(pixel_square)
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
    let ticks = tick(Duration::from_millis(10));

    // Per-wallpaper timing, so frame advancement no longer depends on a
    // single shared counter (which drifted once more than one wallpaper
    // or screen was involved) and can't divide/modulo by a zero delay.
    let mut last_frame_times = vec![Instant::now(); len];

    loop {
        select! {
            recv(ticks) -> _ => {
                // Advance each wallpaper stack at most once per tick,
                // regardless of how many screens display it -- previously
                // a wallpaper shared by N screens was advanced N times
                // per tick and played back N times too fast.
                for (stack_index, stack) in wallpapers.iter_mut().enumerate() {
                    let delay_ms = (stack.last_frame_delay as u64) * 10;
                    let should_update = delay_ms == 0
                        || last_frame_times[stack_index].elapsed() >= Duration::from_millis(delay_ms);

                    if !should_update {
                        continue;
                    }

                    match stack.next() {
                        Ok(frame) => {
                            last_frame_times[stack_index] = Instant::now();
                            let texture = &mut textures[stack_index];
                            for square in &frame.squares {
                                if let Err(e) = texture.update(square.rect, &square.pixels, square.pitch) {
                                    eprintln!("texture update error: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("frame loading error: {}", e);
                        }
                    }
                }

                for (screen_index, rect) in screen_rects.iter().enumerate() {
                    let texture_index = screen_index % textures.len();
                    canvas.copy(&textures[texture_index], None, *rect)?;
                }
                canvas.present();
            }
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
