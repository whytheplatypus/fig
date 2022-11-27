use std::io::Write;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::errors::{ReplyOrIdError, ConnectionError};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::cookie::VoidCookie;
use image::codecs::gif::{GifDecoder, GifEncoder};
use image::{ImageDecoder, AnimationDecoder};
use std::ffi::c_void;
use std::fs::File;
use std::{thread, time::Duration};
use sdl2::video::Window;
use sdl2::render::Canvas;
use sdl2::render::TextureCreator;
use sdl2::surface::Surface;
use sdl2::image::LoadTexture;
use sdl2_sys::SDL_CreateWindowFrom;
use sdl2::pixels::PixelFormatEnum;
use std::env::temp_dir;
use std::env;
use sdl2::render::Texture;
use sdl2::rect::Rect;
use x11rb::protocol::xinerama::query_screens;


struct Frame<'a> {
    delay: u32,
    texture: Texture<'a>,
}

struct Stack<'a> {
    count: usize,
    index: usize,
    frames: Vec::<Frame<'a>>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {

    let args: Vec<String> = env::args().collect();

    // switch to a slice of files
    let gifs = dbg!(&args[1..args.len()]);
    let gif = &gifs[0];

    println!("Rendering file {}", gif);

    let (conn, screen_num) = x11rb::connect(None).unwrap();

    let screens = dbg!(query_screens(&conn).unwrap().reply().unwrap());

    let win_id = create_desktop(&conn, screen_num).unwrap();
    set_desktop_atoms(&conn, win_id);
    show_desktop(&conn, win_id);


    println!("Created {:?}", win_id);
    let sdl_context = sdl2::init()?;
    let video_subsystem = sdl_context.video()?;

    unsafe {
        let sdl_win = SDL_CreateWindowFrom(win_id as *const c_void);
        let win = Window::from_ll(video_subsystem, sdl_win);

        // We get the canvas from which we can get the `TextureCreator`.
        let mut canvas: Canvas<Window> = win.into_canvas()
            .build()
            .expect("failed to build window's canvas");
        let texture_creator = canvas.texture_creator();


        let mut wallpapers = Vec::new();
        for gif in gifs {
            let file_in = File::open(gif)?;
            let mut decoder = GifDecoder::new(file_in).unwrap();
            let frames = decoder.into_frames();

            let mut stack = Vec::new();
            for (i, result) in frames.enumerate() {
                let tmp_frame = format!("{}/fig_frame.bmp", temp_dir().to_str().unwrap());

                print!("\rLoading frame {} ", i);
                std::io::stdout().flush();

                let frame = result.unwrap();
                let (delay, _) = frame.delay().numer_denom_ms();
                let buffer = frame.into_buffer();
                buffer.save(&tmp_frame).unwrap();
                // can do this to load all textures
                let texture = texture_creator.load_texture(&tmp_frame).unwrap();
                stack.push(Frame{ delay, texture });
                // can we scale early?
            }
            wallpapers.push(Stack{ count: stack.len(), index: 0, frames: stack});
        }

        let mut count: u32 = 0;
        let max: u32 = dbg!(wallpapers.iter().map(|wallpaper| wallpaper.frames.iter().map(|frame| frame.delay).sum::<u32>()).sum());
        loop {
            for (i, screen) in screens.screen_info.iter().enumerate() {
                let len = wallpapers.len();
                let stack = &mut wallpapers[i%len];
                let delay = stack.frames[stack.index].delay;
                if count % delay == 0 {
                    let rect = Rect::new(screen.x_org as i32, screen.y_org as i32, screen.width as u32, screen.height as u32);
                    let texture = &stack.frames[stack.index].texture;
                    // then move to this
                    canvas.copy(&texture, None, rect);
                    canvas.present();
                    stack.index = (stack.index + 1) % stack.count;
                }
            }
            count = (count + 1) % max;
            thread::sleep(Duration::from_millis(1));
        }


    }
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
