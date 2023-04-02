use std::io::{Write, Cursor};
use std::ffi::c_void;
use std::fs::File;
use std::{thread, time::Duration};
use std::env;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xinerama::query_screens;
use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::errors::{ReplyOrIdError, ConnectionError};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::cookie::VoidCookie;
use image::codecs::gif::GifDecoder;
use image::RgbaImage;
use image::AnimationDecoder;
use image::ImageOutputFormat;
use sdl2::video::Window;
use sdl2::render::Canvas;
use sdl2::image::LoadTexture;
use sdl2::rect::Rect;
use sdl2_sys::SDL_CreateWindowFrom;


/*
struct Frame<'a> {
    delay: u32,
    texture: Texture<'a>,
}
*/

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RawFrame {
    index: usize,
    delay: u32,
    pixels: Vec::<u8>,
}

struct Stack {
    count: usize,
    index: usize,
    frames: Vec::<RawFrame>,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {

    let args: Vec<String> = env::args().collect();

    let gifs = dbg!(&args[1..args.len()]);
    let mut wallpapers = {
        let mut wallpapers = Vec::new();

        for gif in gifs {
            let stack = load_raw_frames(gif)?;
            wallpapers.push(Stack{ count: stack.len(), index: 0, frames: stack});
        }
        wallpapers
    };

    let (conn, screen_num) = x11rb::connect(None).unwrap();

    let screens = dbg!(query_screens(&conn).unwrap().reply().unwrap());

    let win_id = create_desktop(&conn, screen_num).unwrap();
    set_desktop_atoms(&conn, win_id)?;
    show_desktop(&conn, win_id)?;

    println!("Created {:?}", win_id);

    let mut canvas = create_canvas(win_id)?;
    let texture_creator = canvas.texture_creator();

    let screen_rects = {
        let mut screen_rects = Vec::new();
        for screen in screens.screen_info {
            let rect = Rect::new(screen.x_org as i32, screen.y_org as i32, screen.width as u32, screen.height as u32);
            screen_rects.push(rect);
        }
        screen_rects
    };

    let len = wallpapers.len();
    let mut count: u32 = 0;
    let max: u32 = dbg!(wallpapers.iter().map(|wallpaper| wallpaper.total_time()).sum());
    loop {
        for (i, rect) in screen_rects.iter().enumerate() {
            let stack = &mut wallpapers[i%len];
            let delay = stack.peek().delay;
            if count % delay == 0 {
                let frame = stack.next();
                let texture = texture_creator.load_texture_bytes(&frame.pixels)?;
                canvas.copy(&texture, None, *rect).unwrap();
                canvas.present();
                drop(texture);
            }
        }
        count = (count + 1) % max;
        thread::sleep(Duration::from_millis(1));
    }
}

fn load_raw_frames(gif: &String) -> Result<Vec::<RawFrame>, Box<dyn std::error::Error>> {
    let file_in = File::open(gif)?;
    let decoder = GifDecoder::new(file_in)?;
    let frames = decoder.into_frames();

    let mut raw_stack = Vec::new();
    for (index, result) in frames.enumerate() {
        let frame = result?;
        let (delay, _) = frame.delay().numer_denom_ms();
        let pixels = load_frame(frame.buffer())?;
        raw_stack.push(RawFrame{ index, delay, pixels });
        print!("\rLoading frame {} ", index);
        std::io::stdout().flush()?;
    }


    raw_stack.sort();
    Ok(raw_stack)
}

fn load_frame(buffer: &RgbaImage) -> Result<Vec::<u8>, Box<dyn std::error::Error>> {
    let mut buf = Cursor::new(Vec::new());
    buffer.write_to(&mut buf, ImageOutputFormat::Bmp)?;
    Ok(buf.into_inner())
}

fn create_canvas(win_id: u32) -> Result<Canvas<Window>, Box<dyn std::error::Error>>{
    let sdl_context = sdl2::init()?;
    let video_subsystem = sdl_context.video()?;

    let win = unsafe {
        let sdl_win = SDL_CreateWindowFrom(win_id as *const c_void);
        Window::from_ll(video_subsystem, sdl_win)
    };

    // We get the canvas from which we can get the `TextureCreator`.
    let canvas: Canvas<Window> = win.into_canvas()
        .build()
        .expect("failed to build window's canvas");
    Ok(canvas)
}

fn set_desktop_atoms(conn: &impl Connection, win_id: u32) -> Result<VoidCookie<'_, impl Connection>, ConnectionError> {
    let atom_wm_type = conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE").unwrap().reply().unwrap().atom;
    let atom_wm_desktop = conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE_DESKTOP").unwrap().reply().unwrap().atom;

    conn.change_property32(
        PropMode::REPLACE,
        win_id,
        atom_wm_type,
        AtomEnum::ATOM,
        &[atom_wm_desktop],
    )
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
