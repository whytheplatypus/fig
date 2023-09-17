use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureAccess};
use sdl2::video::Window;
use sdl2_sys::SDL_CreateWindowFrom;
use std::env;
use std::ffi::c_void;
use std::fs::File;
use std::{thread, time::Duration};
use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xinerama::query_screens;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

struct Square {
    rect: Rect,
    pitch: usize,
    pixels: Vec<u8>,
}

struct RawFrame {
    delay: u32,
    squares: Vec<Square>,
}

struct Stack {
    count: usize,
    index: usize,
    // what if we split these up?
    frames: Vec<RawFrame>,
    width: u32,
    height: u32,
}

impl Stack {
    fn next(&mut self) -> &mut RawFrame {
        let frame = &mut self.frames[self.index];
        self.index = (self.index + 1) % self.count;
        frame
    }

    fn peek(&self) -> &RawFrame {
        &self.frames[self.index]
    }

    fn total_time(&self) -> u32 {
        self.frames.iter().map(|frame| frame.delay).sum()
    }
}

const DIMENSION: usize = 30;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    let gifs = dbg!(&args[1..args.len()]);
    let mut wallpapers = {
        let mut wallpapers = Vec::new();

        for gif in gifs {
            let (width, height, frames) = load_raw_frames(gif)?;
            wallpapers.push(Stack {
                count: frames.len(),
                index: 0,
                frames,
                width,
                height,
            });
        }
        wallpapers
    };

    let (conn, screen_num) = x11rb::connect(None)?;

    let screens = dbg!(query_screens(&conn)?.reply()?);
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

    let mut textures = {
        let mut textures = Vec::new();
        // this would have to be gif rects, not screen rects, i think... this was probably part of the old problem
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

    let len = wallpapers.len();
    let mut count: u32 = 0;
    let max: u32 = dbg!(wallpapers
        .iter()
        .map(|wallpaper| wallpaper.total_time())
        .sum());
    loop {
        for (i, rect) in screen_rects.iter().enumerate() {
            let stack = &mut wallpapers[i % len];
            let delay = stack.peek().delay;
            if count % delay == 0 {
                let frame = stack.next();
                let texture = &mut textures[i % len];
                for square in &frame.squares {
                    texture.update(square.rect, &square.pixels, square.pitch)?;
                }
                canvas.copy(&texture, None, *rect)?;
                canvas.present();
            }
        }
        count = (count + 1) % max;
        thread::sleep(Duration::from_millis(10));
    }
}

fn chunck_frame(
    top: i32,
    left: i32,
    width: u32,
    height: u32,
    pitch: usize,
    raw_pixels: &Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut y = top as usize;
    let local_pitch = width as usize * 4;
    let mut pixel_square = Vec::new();
    while y < (height + top as u32) as usize {
        let start = y * pitch + left as usize * 4;
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
    if pixel_square.len() % (width * 4) as usize != 0 || pixel_square.len() == 0 {
        return Err("bad pixel square".into());
    }
    return Ok(pixel_square);
}

fn check_square(frames: &Vec<RawFrame>, square: &Square) -> bool {
    for previoud_frame in frames {
        for previous_square in &previoud_frame.squares {
            if previous_square.rect == square.rect {
                if previous_square.pixels == square.pixels {
                    return true;
                }
                return false;
            }
        }
    }
    return false;
}

fn load_raw_frames(gif: &String) -> Result<(u32, u32, Vec<RawFrame>), Box<dyn std::error::Error>> {
    let file_in = File::open(gif)?;
    let mut decoder = gif::DecodeOptions::new();
    // Configure the decoder such that it will expand the image to RGBA.
    decoder.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = decoder.read_info(file_in)?;
    let mut frames = Vec::new();
    while let Some(frame) = decoder.read_next_frame()? {
        // print the line_length
        let delay = frame.delay as u32;
        // Process every frame
        let pixels = frame.buffer.to_vec();
        let pitch = frame.width as usize * 4;
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
                //let pitch = frame.width as usize * 4;
                let chunk = chunck_frame(
                    y as i32,
                    x as i32,
                    width as u32,
                    height as u32,
                    pitch,
                    &pixels,
                );
                match chunk {
                    Ok(chunk) => {
                        let square = Square {
                            rect,
                            pitch: width as usize * 4,
                            pixels: chunk,
                        };
                        if check_square(&frames, &square) {
                            continue;
                        }
                        squares.push(square);
                    }
                    Err(e) => {
                        println!("error: {}", dbg!(e));
                        continue;
                    }
                }
            }
        }

        frames.push(RawFrame { delay, squares });
    }

    let width = decoder.width();
    let height = decoder.height();

    Ok((width as u32, height as u32, frames))
}

fn create_canvas(win_id: u32) -> Result<Canvas<Window>, Box<dyn std::error::Error>> {
    let sdl_context = sdl2::init()?;
    let video_subsystem = sdl_context.video()?;

    let win = unsafe {
        let sdl_win = SDL_CreateWindowFrom(win_id as *const c_void);
        Window::from_ll(video_subsystem, sdl_win)
    };

    // We get the canvas from which we can get the `TextureCreator`.
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

// if I understand this right I could make this a trait on conn
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
