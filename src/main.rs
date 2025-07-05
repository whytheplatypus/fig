use crossbeam_channel::{bounded, select, tick, Receiver};
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureAccess};
use sdl2::video::Window;
use sdl2_sys::SDL_CreateWindowFrom;

use std::collections::VecDeque;
use std::env;
use std::ffi::c_void;
use std::fs::File;

use std::time::Duration;
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

#[derive(Clone)]
struct RawFrame {
    delay: u32,
    squares: Vec<Square>,
    disposal_method: gif::DisposalMethod,
    frame_rect: Rect,
}

struct LazyStack {
    decoder: Option<gif::Decoder<File>>,
    current_frame_index: usize,
    frame_cache: VecDeque<RawFrame>,
    cache_start_index: usize,
    width: u32,
    height: u32,
    total_frames: Option<usize>,
    is_first_cycle: bool,
    cache_size: usize,
    gif_finished: bool,
    file_path: String,
    frames_loaded_from_file: usize,
    decoder_at_end: bool,
    last_frame_delay: u32,
}

impl LazyStack {
    fn new(gif_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        println!("Loading GIF: {}", gif_path);
        let file_in = File::open(gif_path)
            .map_err(|e| format!("Failed to open GIF file '{}': {}", gif_path, e))?;
        let mut decoder = gif::DecodeOptions::new();
        decoder.set_color_output(gif::ColorOutput::RGBA);
        let decoder = decoder
            .read_info(file_in)
            .map_err(|e| format!("Failed to read GIF info from '{}': {}", gif_path, e))?;

        let width = decoder.width() as u32;
        let height = decoder.height() as u32;
        let cache_size = 25; // Keep 5 frames in cache

        let stack = LazyStack {
            decoder: Some(decoder),
            current_frame_index: 0,
            frame_cache: VecDeque::with_capacity(cache_size),
            cache_start_index: 0,
            width,
            height,
            total_frames: None,
            is_first_cycle: true,
            cache_size,
            gif_finished: false,
            file_path: gif_path.to_string(),
            frames_loaded_from_file: 0,
            decoder_at_end: false,
            last_frame_delay: 0,
        };

        // Don't pre-load any frames - load them on demand
        println!(
            "Loaded GIF: {} ({}x{}) ready for lazy loading",
            gif_path, width, height
        );

        Ok(stack)
    }

    fn next(&mut self) -> Result<RawFrame, Box<dyn std::error::Error>> {
        // Check if we've reached the end of the GIF and need to restart
        if let Some(total) = self.total_frames {
            if self.current_frame_index >= total {
                self.current_frame_index = 0;
                self.restart_gif()?;
            }
        }

        // Ensure the current frame is in cache
        self.ensure_frame_in_cache(self.current_frame_index)?;

        // Get current frame from cache
        let cache_pos = self.current_frame_index - self.cache_start_index;
        if cache_pos >= self.frame_cache.len() {
            return Err(format!(
                "Frame not in cache after ensuring: cache_pos={}, cache_len={}",
                cache_pos,
                self.frame_cache.len()
            )
            .into());
        }

        // Clone the frame to avoid borrow checker issues
        let frame = self.frame_cache[cache_pos].clone();

        // Store the frame delay for timing
        self.last_frame_delay = frame.delay;

        // If this is the first frame being processed, mark it as no longer first cycle
        if self.is_first_cycle && self.current_frame_index == 0 {
            self.is_first_cycle = false;
        }

        // Advance to next frame
        self.current_frame_index += 1;

        Ok(frame)
    }

    fn last_frame_delay(&self) -> u32 {
        self.last_frame_delay
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn load_next_frame(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.decoder_at_end {
            return Err("GIF has finished loading".into());
        }

        if let Some(mut decoder) = self.decoder.take() {
            if let Some(frame) = decoder.read_next_frame()? {
                let raw_frame = self.process_frame(&frame)?;

                // If cache is full, remove oldest frame and advance window
                if self.frame_cache.len() >= self.cache_size {
                    self.frame_cache.pop_front();
                    self.cache_start_index += 1;
                }

                self.frame_cache.push_back(raw_frame);
                self.frames_loaded_from_file += 1;
                self.decoder = Some(decoder);
                Ok(())
            } else {
                // End of GIF - mark as finished and set total frames
                self.decoder_at_end = true;
                self.gif_finished = true;
                self.total_frames = Some(self.frames_loaded_from_file);
                self.decoder = Some(decoder);
                Err("End of GIF reached".into())
            }
        } else {
            Err("No decoder available".into())
        }
    }

    fn ensure_frame_in_cache(
        &mut self,
        frame_index: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Check if frame is already in cache
        if !self.frame_cache.is_empty()
            && frame_index >= self.cache_start_index
            && frame_index < self.cache_start_index + self.frame_cache.len()
        {
            return Ok(());
        }

        // If we're going backwards, this should only happen on restart
        if frame_index < self.cache_start_index {
            return Err("Requested frame is before cache window".into());
        }

        // Frame is ahead of cache, need to advance cache window
        while frame_index >= self.cache_start_index + self.frame_cache.len() {
            // Try to load next frame
            match self.load_next_frame() {
                Ok(()) => {
                    // Successfully loaded a frame, continue
                }
                Err(_) => {
                    // Could not load more frames
                    if self.decoder_at_end && self.total_frames.is_some() {
                        // We've reached the end and know the total frames
                        return Err("Requested frame beyond end of GIF".into());
                    } else {
                        // This is the first time we've hit the end
                        break;
                    }
                }
            }
        }

        // Final check - is the frame now in cache?
        if !self.frame_cache.is_empty()
            && frame_index >= self.cache_start_index
            && frame_index < self.cache_start_index + self.frame_cache.len()
        {
            Ok(())
        } else {
            Err(format!(
                "Could not load frame {} into cache (cache: {}-{}, len={}, gif_finished: {})",
                frame_index,
                self.cache_start_index,
                self.cache_start_index + self.frame_cache.len().saturating_sub(1),
                self.frame_cache.len(),
                self.gif_finished
            )
            .into())
        }
    }

    fn restart_gif(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Reopen the file and recreate the decoder
        let file_in = File::open(&self.file_path)
            .map_err(|e| format!("Failed to reopen GIF file '{}': {}", self.file_path, e))?;
        let mut decoder = gif::DecodeOptions::new();
        decoder.set_color_output(gif::ColorOutput::RGBA);
        let decoder = decoder
            .read_info(file_in)
            .map_err(|e| format!("Failed to read GIF info from '{}': {}", self.file_path, e))?;

        // Reset state
        self.decoder = Some(decoder);
        self.frame_cache.clear();
        self.cache_start_index = 0;
        self.gif_finished = false;
        self.is_first_cycle = true;
        self.frames_loaded_from_file = 0;
        self.decoder_at_end = false;
        self.last_frame_delay = 0;

        // Don't pre-load frames on restart - load them on demand

        Ok(())
    }

    fn process_frame(
        &mut self,
        frame: &gif::Frame,
    ) -> Result<RawFrame, Box<dyn std::error::Error>> {
        let delay = frame.delay as u32;
        let disposal_method = frame.dispose;
        let frame_rect = Rect::new(
            frame.left as i32,
            frame.top as i32,
            frame.width as u32,
            frame.height as u32,
        );

        // Handle GIF disposal methods properly
        let pixels = frame.buffer.to_vec();

        let pitch = frame.width as usize * 4; // 4 bytes per pixel (RGBA)
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
                    y as usize,
                    x as usize,
                    width as usize,
                    height as usize,
                    pitch,
                    4,
                    &pixels,
                ) {
                    Ok(chunk) => {
                        // Include all squares, let texture update handle transparency
                        let square = Square {
                            rect,
                            pitch: width as usize * 4,
                            pixels: chunk,
                        };
                        squares.push(square);
                    }
                    Err(_) => continue,
                }
            }
        }

        Ok(RawFrame {
            delay,
            squares,
            disposal_method,
            frame_rect,
        })
    }
}

const DIMENSION: usize = 64;

fn update_texture_with_transparency(
    texture: &mut sdl2::render::Texture,
    rect: sdl2::rect::Rect,
    pixels: &[u8],
    pitch: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    // Always update the texture - SDL will handle transparency properly
    // This ensures we don't miss any pixels that should be updated
    texture.update(rect, pixels, pitch)?;
    Ok(())
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
            let stack = LazyStack::new(gif)?;
            wallpapers.push(stack);
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
        // Create one texture per wallpaper, not per screen
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
    let ticks = tick(Duration::from_millis(10)); // 100 FPS for better timing granularity

    // Track last frame update time for each stack independently
    let mut last_frame_times = vec![std::time::Instant::now(); len];

    loop {
        select! {
            recv(ticks) -> _ => {
                // Update each wallpaper stack independently
                for stack_index in 0..wallpapers.len() {
                    let stack = &mut wallpapers[stack_index];

                    // Check if we should update this stack based on last frame delay
                    let should_update = if stack.last_frame_delay() > 0 {
                        let delay_ms = (stack.last_frame_delay().max(2) * 10) as u64; // Convert centiseconds to milliseconds, minimum 20ms
                        let elapsed = last_frame_times[stack_index].elapsed();
                        elapsed >= Duration::from_millis(delay_ms)
                    } else {
                        // No frame processed yet, forcing update
                        true
                    };

                    if should_update {
                        // Get values before mutable borrow
                        let is_first_cycle = stack.is_first_cycle;
                        let width = stack.width();
                        let height = stack.height();

                        match stack.next() {
                            Ok(frame) => {
                                // Update last frame time for this stack
                                last_frame_times[stack_index] = std::time::Instant::now();

                                // Processing frame - use stack index for texture
                                let texture = &mut textures[stack_index];

                                // Clear texture completely if this is the first frame of a new cycle
                                if is_first_cycle {
                                    let clear_pixels = vec![0u8; (width * height * 4) as usize];
                                    if let Err(e) = texture.update(
                                        None,
                                        &clear_pixels,
                                        (width * 4) as usize
                                    ) {
                                        eprintln!("Texture clear error: {}", e);
                                    }
                                }

                                // Handle GIF disposal method for proper frame composition
                                match frame.disposal_method {
                                    gif::DisposalMethod::Background => {
                                        // Clear only the frame area to background (transparent)
                                        let clear_size = (frame.frame_rect.width() * frame.frame_rect.height() * 4) as usize;
                                        let clear_pixels = vec![0u8; clear_size];
                                        if let Err(e) = texture.update(
                                            Some(frame.frame_rect),
                                            &clear_pixels,
                                            (frame.frame_rect.width() * 4) as usize
                                        ) {
                                            eprintln!("Texture clear error: {}", e);
                                        }
                                    }
                                    gif::DisposalMethod::Previous => {
                                        // For Previous disposal, we would need to restore to previous frame
                                        // For now, treat like Any (no disposal) - this is complex to implement fully
                                        // But we can at least avoid clearing the frame area
                                    }
                                    _ => {
                                        // No disposal (Any) - leave frame as is for differential composition
                                        // This is the most common case for animated GIFs
                                    }
                                }

                                // Update with the current frame squares, handling transparency properly
                                for square in &frame.squares {
                                    if let Err(e) = update_texture_with_transparency(
                                        texture,
                                        square.rect,
                                        &square.pixels,
                                        square.pitch,
                                    ) {
                                        eprintln!("Texture update error: {}", e);
                                        continue;
                                    }
                                }

                            }
                            Err(e) => {
                                eprintln!("Frame loading error: {}", e);
                            }
                        }
                    }
                }

                // Now render all textures to their respective screens
                for (screen_index, rect) in screen_rects.iter().enumerate() {
                    let texture_index = screen_index % textures.len();
                    let texture = &textures[texture_index];

                    if let Err(e) = canvas.copy(texture, None, *rect) {
                        eprintln!("Canvas copy error: {}", e);
                    }
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
