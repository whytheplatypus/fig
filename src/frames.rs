use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::time::Duration;

pub const BYTES_PER_PIXEL: usize = 4;

/// Decodes a GIF to RGBA one frame at a time instead of decoding the whole
/// file up front. The frame buffers are reused, so steady-state decoding
/// doesn't allocate.
pub struct GifFrames {
    decoder: gif::Decoder<BufReader<File>>,
    file_path: String,
    width: usize,
    height: usize,
    global_palette: Vec<u8>,
    // Palette indices for the current frame, as decoded.
    indices: Vec<u8>,
    // The current frame expanded to RGBA.
    pixels: Vec<u8>,
}

/// Where a decoded frame goes in the image, and how long to show it.
pub struct Frame {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub delay: Duration,
}

impl GifFrames {
    pub fn open(gif_path: &str) -> Result<Self, Box<dyn Error>> {
        let decoder = open_decoder(gif_path)?;
        Ok(GifFrames {
            width: decoder.width() as usize,
            height: decoder.height() as usize,
            global_palette: decoder.global_palette().unwrap_or(&[]).to_vec(),
            decoder,
            file_path: gif_path.to_string(),
            indices: Vec::new(),
            pixels: Vec::new(),
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Decode the next frame, or return `None` at the end of the GIF. The
    /// frame's pixels are then available from `pixels()`.
    pub fn next(&mut self) -> Result<Option<Frame>, Box<dyn Error>> {
        let (frame, colors) = match self.decoder.next_frame_info()? {
            None => return Ok(None),
            Some(frame) => (
                Frame {
                    left: frame.left as usize,
                    top: frame.top as usize,
                    width: frame.width as usize,
                    height: frame.height as usize,
                    // Floor the delay so a GIF frame with delay=0 (common in
                    // poorly optimized files) can't cause a runaway-fast loop.
                    delay: Duration::from_millis(frame.delay.max(2) as u64 * 10),
                },
                rgba_table(
                    frame.palette.as_deref().unwrap_or(&self.global_palette),
                    frame.transparent,
                ),
            ),
        };
        if frame.left + frame.width > self.width || frame.top + frame.height > self.height {
            return Err("gif frame extends past the image".into());
        }
        self.indices.resize(frame.width * frame.height, 0);
        self.decoder.read_into_buffer(&mut self.indices)?;

        // Expanding indices to RGBA ourselves with a lookup table is about
        // twice as fast as the gif crate's per-channel RGBA output.
        self.pixels.resize(self.indices.len() * BYTES_PER_PIXEL, 0);
        for (pixel, &index) in self.pixels.chunks_exact_mut(BYTES_PER_PIXEL).zip(&self.indices) {
            pixel.copy_from_slice(&colors[index as usize]);
        }
        Ok(Some(frame))
    }

    /// Start again from the first frame. `gif::Decoder` can't rewind, so
    /// this reopens the file -- the OS page cache keeps that cheap.
    pub fn rewind(&mut self) -> Result<(), Box<dyn Error>> {
        self.decoder = open_decoder(&self.file_path)?;
        Ok(())
    }

    /// The RGBA pixels of the frame `next()` last returned, row by row.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

fn open_decoder(gif_path: &str) -> Result<gif::Decoder<BufReader<File>>, Box<dyn Error>> {
    let file_in = BufReader::new(
        File::open(gif_path)
            .map_err(|e| format!("Failed to open GIF file '{}': {}", gif_path, e))?,
    );
    let mut decoder = gif::DecodeOptions::new();
    decoder.set_color_output(gif::ColorOutput::Indexed);
    decoder
        .read_info(file_in)
        .map_err(|e| format!("Failed to read GIF info from '{}': {}", gif_path, e).into())
}

/// The RGBA color of each palette index. Indices past the end of the
/// palette stay transparent black, as they do in the gif crate's RGBA output.
fn rgba_table(palette: &[u8], transparent: Option<u8>) -> [[u8; BYTES_PER_PIXEL]; 256] {
    let mut table = [[0; BYTES_PER_PIXEL]; 256];
    for (index, (color, rgb)) in table.iter_mut().zip(palette.chunks_exact(3)).enumerate() {
        let alpha = if transparent == Some(index as u8) { 0x00 } else { 0xFF };
        *color = [rgb[0], rgb[1], rgb[2], alpha];
    }
    table
}
