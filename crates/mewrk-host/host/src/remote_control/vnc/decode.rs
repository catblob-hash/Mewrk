//! Turning RFB rectangles into pixels.
//!
//! The client asks every server for one pixel format ([`super::PIXEL_FORMAT`]):
//! 32 bits per pixel, little-endian, red in the lowest byte. A pixel on the wire
//! is therefore R, G, B, X — RGBA with the alpha byte still to be set — and
//! ZRLE's compressed pixel is just R, G, B. Every decoder below relies on that
//! and nothing else, which is why none of them reads a pixel format.
//!
//! Decoders never touch the framebuffer. They hand each decoded tile to a sink
//! (`FnMut(Area, &[u8])`), and the reader thread's sink takes the screen's lock
//! for one tile at a time: a slow network trickling a large rectangle in never
//! holds the lock the viewer and the model read frames under.
//!
//! The server is not trusted to be well formed. Every size comes from the wire,
//! so every one is checked before it decides how much to read or where to
//! write: a rectangle against the screen, a subrectangle against its tile, a
//! run against what is left of its tile, a palette index against its palette.

use std::io::{self, BufRead, BufReader, Read};

use flate2::{Decompress, FlushDecompress, Status};

/// The largest side of a screen this client accepts. Real desktops stop well
/// short of it (an 8K display is 7680 wide); a larger number is a broken or
/// hostile server asking for a huge allocation.
pub const MAX_SIDE: u32 = 8192;
/// The most pixels a screen may have: 8192 × 4096, 128 MiB of RGBA.
pub const MAX_PIXELS: u64 = 8192 * 4096;

/// Raw rows are read in pieces of about this many bytes, so a full-screen raw
/// rectangle needs no buffer the size of the screen.
const RAW_CHUNK: usize = 256 * 1024;
/// Decompressed bytes a ZRLE rectangle may leave unread after its last tile.
/// A correct server leaves none; this only keeps a wrong one from making the
/// client inflate without end.
const ZRLE_LEFTOVER: usize = 64 * 1024;

/// Why reading a server message stopped.
#[derive(Debug)]
pub enum WireError {
    /// The connection itself failed or ended.
    Io(io::Error),
    /// The server sent something this client cannot make sense of. The text
    /// says what, for the log and the model.
    Protocol(String),
}

impl From<io::Error> for WireError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type WireResult<T> = Result<T, WireError>;

fn protocol<T>(message: impl Into<String>) -> WireResult<T> {
    Err(WireError::Protocol(message.into()))
}

/// A rectangle of the screen, as a rectangle header gives it. Held as `u32`
/// so that `x + width` of two `u16`s cannot overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Area {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x: u32::from(x),
            y: u32::from(y),
            width: u32::from(width),
            height: u32::from(height),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Whether the area lies inside a `width` × `height` screen.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.x + self.width <= width && self.y + self.height <= height
    }

    fn pixels(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Whether a screen of this size is one this client will hold.
pub fn check_size(width: u32, height: u32) -> Result<(), String> {
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "the VNC server's screen is {width}x{height}, larger than this client accepts \
             ({MAX_SIDE} pixels a side, {MAX_PIXELS} pixels in all)"
        ));
    }
    Ok(())
}

/// The screen as the client knows it: RGBA, alpha always 255.
pub struct Framebuffer {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Framebuffer {
    /// A black screen. The size must have passed [`check_size`].
    pub fn new(width: u32, height: u32) -> Self {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        set_opaque(&mut rgba);
        Self {
            width,
            height,
            rgba,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Changes the size, keeping what the old and new screens share; the rest
    /// is black until the server paints it.
    pub fn resize(&mut self, width: u32, height: u32) {
        let mut resized = Self::new(width, height);
        let shared = Area {
            x: 0,
            y: 0,
            width: width.min(self.width),
            height: height.min(self.height),
        };
        if !shared.is_empty() {
            let row = shared.width as usize * 4;
            for line in 0..shared.height as usize {
                let from = line * self.width as usize * 4;
                let to = line * width as usize * 4;
                resized.rgba[to..to + row].copy_from_slice(&self.rgba[from..from + row]);
            }
        }
        *self = resized;
    }

    /// Paints `rgba` (tightly packed, `area.width` pixels a row) at `area`. An
    /// area outside the screen is ignored; the reader checks every rectangle
    /// before decoding it, so that would be a bug here, not the server's.
    pub fn blit(&mut self, area: Area, rgba: &[u8]) {
        if area.is_empty() || !area.fits(self.width, self.height) {
            debug_assert!(area.is_empty(), "blit outside the screen: {area:?}");
            return;
        }
        let row = area.width as usize * 4;
        if rgba.len() < row * area.height as usize {
            debug_assert!(false, "blit of too few pixels for {area:?}");
            return;
        }
        let stride = self.width as usize * 4;
        for line in 0..area.height as usize {
            let to = (area.y as usize + line) * stride + area.x as usize * 4;
            self.rgba[to..to + row].copy_from_slice(&rgba[line * row..(line + 1) * row]);
        }
    }

    /// Copies the `area`-sized block at `(source_x, source_y)` to `area`, the
    /// two possibly overlapping (CopyRect). Both must be on the screen.
    pub fn copy_within(&mut self, source_x: u32, source_y: u32, area: Area) {
        let source = Area {
            x: source_x,
            y: source_y,
            ..area
        };
        if area.is_empty()
            || !area.fits(self.width, self.height)
            || !source.fits(self.width, self.height)
        {
            return;
        }
        let row = area.width as usize * 4;
        let stride = self.width as usize * 4;
        let line_at =
            |area: &Area, line: usize| (area.y as usize + line) * stride + area.x as usize * 4;
        // Rows are copied in the order that never reads a row already overwritten.
        let lines: Box<dyn Iterator<Item = usize>> = if source.y < area.y {
            Box::new((0..area.height as usize).rev())
        } else {
            Box::new(0..area.height as usize)
        };
        for line in lines {
            let from = line_at(&source, line);
            self.rgba
                .copy_within(from..from + row, line_at(&area, line));
        }
    }
}

/// Sets every fourth byte, the X of an R, G, B, X pixel, to an opaque alpha.
pub fn set_opaque(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

pub fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

pub fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

/// Reads and drops `count` bytes without buffering them all.
pub fn skip(reader: &mut impl Read, count: u64) -> io::Result<()> {
    let copied = io::copy(&mut reader.by_ref().take(count), &mut io::sink())?;
    if copied < count {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// One full pixel, as Raw and Hextile send it.
fn read_pixel(reader: &mut impl Read) -> io::Result<[u8; 4]> {
    let mut pixel = [0u8; 4];
    reader.read_exact(&mut pixel)?;
    pixel[3] = 255;
    Ok(pixel)
}

/// One compressed pixel, as ZRLE sends it: the three bytes that carry colour.
fn read_cpixel(reader: &mut impl Read) -> io::Result<[u8; 4]> {
    let mut pixel = [0u8; 4];
    reader.read_exact(&mut pixel[..3])?;
    pixel[3] = 255;
    Ok(pixel)
}

fn fill(rgba: &mut [u8], colour: [u8; 4]) {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&colour);
    }
}

/// Splits `area` into tiles of `side`, left to right and top to bottom, the
/// order both Hextile and ZRLE send them in.
fn tiles(area: Area, side: u32) -> impl Iterator<Item = Area> {
    let rows = area.height.div_ceil(side);
    let columns = area.width.div_ceil(side);
    (0..rows).flat_map(move |row| {
        (0..columns).map(move |column| {
            let x = column * side;
            let y = row * side;
            Area {
                x: area.x + x,
                y: area.y + y,
                width: side.min(area.width - x),
                height: side.min(area.height - y),
            }
        })
    })
}

/// Raw: every pixel of the rectangle, row by row.
pub fn raw(
    reader: &mut impl Read,
    area: Area,
    sink: &mut dyn FnMut(Area, &[u8]),
) -> WireResult<()> {
    if area.is_empty() {
        return Ok(());
    }
    let row = area.width as usize * 4;
    let rows_per_chunk = (RAW_CHUNK / row).clamp(1, area.height as usize);
    let mut buffer = vec![0u8; row * rows_per_chunk];
    let mut line = 0;
    while line < area.height {
        let rows = (rows_per_chunk as u32).min(area.height - line);
        let chunk = &mut buffer[..row * rows as usize];
        reader.read_exact(chunk)?;
        set_opaque(chunk);
        sink(
            Area {
                x: area.x,
                y: area.y + line,
                width: area.width,
                height: rows,
            },
            chunk,
        );
        line += rows;
    }
    Ok(())
}

mod hextile_flags {
    pub const RAW: u8 = 1;
    pub const BACKGROUND_SPECIFIED: u8 = 1 << 1;
    pub const FOREGROUND_SPECIFIED: u8 = 1 << 2;
    pub const ANY_SUBRECTS: u8 = 1 << 3;
    pub const SUBRECTS_COLOURED: u8 = 1 << 4;
}

/// Hextile: 16×16 tiles, each raw or a background with subrectangles. The
/// background and foreground carry over from one tile to the next within the
/// rectangle, which is what makes a tile with no flags at all mean "the same
/// colour as before".
pub fn hextile(
    reader: &mut impl Read,
    area: Area,
    sink: &mut dyn FnMut(Area, &[u8]),
) -> WireResult<()> {
    use hextile_flags::*;
    let mut background = [0, 0, 0, 255];
    let mut foreground = [0, 0, 0, 255];
    let mut buffer = [0u8; 16 * 16 * 4];
    for tile in tiles(area, 16) {
        let pixels = &mut buffer[..tile.pixels() * 4];
        let mask = read_u8(reader)?;
        if mask & RAW != 0 {
            reader.read_exact(pixels)?;
            set_opaque(pixels);
        } else {
            if mask & BACKGROUND_SPECIFIED != 0 {
                background = read_pixel(reader)?;
            }
            if mask & FOREGROUND_SPECIFIED != 0 {
                foreground = read_pixel(reader)?;
            }
            fill(pixels, background);
            if mask & ANY_SUBRECTS != 0 {
                let count = read_u8(reader)?;
                for _ in 0..count {
                    let colour = if mask & SUBRECTS_COLOURED != 0 {
                        read_pixel(reader)?
                    } else {
                        foreground
                    };
                    let mut place = [0u8; 2];
                    reader.read_exact(&mut place)?;
                    let sub = Area {
                        x: u32::from(place[0] >> 4),
                        y: u32::from(place[0] & 0x0f),
                        width: u32::from(place[1] >> 4) + 1,
                        height: u32::from(place[1] & 0x0f) + 1,
                    };
                    if !sub.fits(tile.width, tile.height) {
                        return protocol(format!(
                            "the VNC server sent a Hextile subrectangle ({},{} {}x{}) outside its {}x{} tile",
                            sub.x, sub.y, sub.width, sub.height, tile.width, tile.height
                        ));
                    }
                    for line in sub.y..sub.y + sub.height {
                        let start = ((line * tile.width + sub.x) * 4) as usize;
                        fill(&mut pixels[start..start + sub.width as usize * 4], colour);
                    }
                }
            }
        }
        sink(tile, pixels);
    }
    Ok(())
}

/// The compressed bytes of one ZRLE rectangle, read through the connection's
/// one inflate stream. ZRLE never resets its zlib stream: each rectangle's
/// data continues the dictionary of every rectangle before it, so the
/// [`Decompress`] lives as long as the connection, and this reader only lends
/// it the next `remaining` bytes of the socket.
struct Inflating<'a, R: Read> {
    source: &'a mut R,
    inflate: &'a mut Decompress,
    /// Compressed bytes of this rectangle not yet taken from the socket.
    remaining: usize,
    input: Box<[u8]>,
    start: usize,
    end: usize,
    /// Whether the socket ended before the rectangle did, which is the
    /// connection closing rather than a malformed rectangle.
    source_ended: bool,
}

impl<R: Read> Read for Inflating<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let before_in = self.inflate.total_in();
            let before_out = self.inflate.total_out();
            let status = self
                .inflate
                .decompress(
                    &self.input[self.start..self.end],
                    output,
                    FlushDecompress::Sync,
                )
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("the VNC server's ZRLE data is not valid zlib ({error})"),
                    )
                })?;
            let consumed = (self.inflate.total_in() - before_in) as usize;
            let produced = (self.inflate.total_out() - before_out) as usize;
            self.start += consumed;
            if produced > 0 {
                return Ok(produced);
            }
            if status == Status::StreamEnd {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the VNC server ended its ZRLE zlib stream, which ZRLE never does",
                ));
            }
            if consumed > 0 {
                continue;
            }
            // No progress on what is buffered: the inflater needs more input.
            if self.remaining == 0 {
                return Ok(0);
            }
            if self.start > 0 {
                self.input.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            if self.end == self.input.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the VNC server's ZRLE data made no progress",
                ));
            }
            let want = (self.input.len() - self.end).min(self.remaining);
            let got = match self.source.read(&mut self.input[self.end..self.end + want]) {
                Ok(0) => {
                    self.source_ended = true;
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                Ok(got) => got,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.source_ended = true;
                    return Err(error);
                }
            };
            self.end += got;
            self.remaining -= got;
        }
    }
}

/// The most compressed bytes a ZRLE rectangle of `area` could honestly need:
/// four bytes a pixel (plain RLE of runs of one), a full palette per tile,
/// and deflate's framing on top.
fn zrle_bound(area: Area) -> u64 {
    let tiles = u64::from(area.width.div_ceil(64)) * u64::from(area.height.div_ceil(64));
    let decompressed = area.pixels() as u64 * 4 + tiles * (1 + 127 * 3);
    decompressed + decompressed / 8 + 64 * 1024
}

/// ZRLE: a length, then zlib data that inflates to 64×64 tiles, each raw,
/// solid, packed against a palette, or run-length encoded with or without one.
pub fn zrle(
    reader: &mut impl Read,
    area: Area,
    inflate: &mut Decompress,
    sink: &mut dyn FnMut(Area, &[u8]),
) -> WireResult<()> {
    let length = read_u32(reader)?;
    if u64::from(length) > zrle_bound(area) {
        return protocol(format!(
            "the VNC server sent {length} bytes of ZRLE data for a {}x{} rectangle, more than it can need",
            area.width, area.height
        ));
    }
    let mut stream = BufReader::with_capacity(
        32 * 1024,
        Inflating {
            source: reader,
            inflate,
            remaining: length as usize,
            input: vec![0u8; 32 * 1024].into_boxed_slice(),
            start: 0,
            end: 0,
            source_ended: false,
        },
    );
    let decoded = zrle_tiles(&mut stream, area, sink).and_then(|()| {
        // Whatever the tiles did not use still belongs to the shared stream: it is
        // inflated (and dropped) so the next rectangle starts where the server's does.
        let mut leftover = stream.buffer().len();
        stream.consume(leftover);
        let mut scratch = [0u8; 4096];
        loop {
            let read = stream.get_mut().read(&mut scratch)?;
            if read == 0 {
                return Ok(());
            }
            leftover += read;
            if leftover > ZRLE_LEFTOVER {
                return protocol(
                    "the VNC server's ZRLE rectangle inflates to far more than its tiles",
                );
            }
        }
    });
    match decoded {
        Err(WireError::Io(error)) if !stream.get_ref().source_ended => {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                protocol("the VNC server's ZRLE rectangle ended before its last tile")
            } else {
                protocol(error.to_string())
            }
        }
        other => other,
    }
}

fn zrle_tiles(
    stream: &mut impl Read,
    area: Area,
    sink: &mut dyn FnMut(Area, &[u8]),
) -> WireResult<()> {
    let mut buffer = vec![0u8; 64 * 64 * 4];
    let mut palette = [[0u8; 4]; 127];
    for tile in tiles(area, 64) {
        let count = tile.pixels();
        let pixels = &mut buffer[..count * 4];
        let subencoding = read_u8(stream)?;
        match subencoding {
            0 => {
                let mut raw = [0u8; 64 * 64 * 3];
                let raw = &mut raw[..count * 3];
                stream.read_exact(raw)?;
                for (pixel, colour) in pixels.chunks_exact_mut(4).zip(raw.chunks_exact(3)) {
                    pixel[..3].copy_from_slice(colour);
                    pixel[3] = 255;
                }
            }
            1 => fill(pixels, read_cpixel(stream)?),
            2..=16 => {
                let size = usize::from(subencoding);
                for entry in &mut palette[..size] {
                    *entry = read_cpixel(stream)?;
                }
                let bits = match size {
                    2 => 1,
                    3 | 4 => 2,
                    _ => 4,
                };
                let row_bytes = (tile.width as usize * bits).div_ceil(8);
                let mut row = [0u8; 32];
                let per_byte = 8 / bits;
                let mask = (1u8 << bits) - 1;
                for line in 0..tile.height as usize {
                    stream.read_exact(&mut row[..row_bytes])?;
                    for column in 0..tile.width as usize {
                        let byte = row[column / per_byte];
                        let shift = 8 - bits * (column % per_byte + 1);
                        let index = usize::from((byte >> shift) & mask);
                        if index >= size {
                            return protocol(format!(
                                "the VNC server sent ZRLE palette index {index} for a palette of {size}"
                            ));
                        }
                        let at = (line * tile.width as usize + column) * 4;
                        pixels[at..at + 4].copy_from_slice(&palette[index]);
                    }
                }
            }
            128 => {
                let mut done = 0;
                while done < count {
                    let colour = read_cpixel(stream)?;
                    let run = read_run(stream, count - done)?;
                    fill(&mut pixels[done * 4..(done + run) * 4], colour);
                    done += run;
                }
            }
            130..=255 => {
                let size = usize::from(subencoding - 128);
                for entry in &mut palette[..size] {
                    *entry = read_cpixel(stream)?;
                }
                let mut done = 0;
                while done < count {
                    let byte = read_u8(stream)?;
                    let index = usize::from(byte & 0x7f);
                    if index >= size {
                        return protocol(format!(
                            "the VNC server sent ZRLE palette index {index} for a palette of {size}"
                        ));
                    }
                    let run = if byte & 0x80 != 0 {
                        read_run(stream, count - done)?
                    } else {
                        1
                    };
                    fill(&mut pixels[done * 4..(done + run) * 4], palette[index]);
                    done += run;
                }
            }
            other => {
                return protocol(format!(
                    "the VNC server sent ZRLE tile subencoding {other}, which ZRLE does not define"
                ));
            }
        }
        sink(tile, pixels);
    }
    Ok(())
}

/// A ZRLE run length: one plus the sum of its bytes, every 255 meaning another
/// byte follows. It may not run past the end of its tile.
fn read_run(stream: &mut impl Read, left: usize) -> WireResult<usize> {
    let mut run = 1usize;
    loop {
        let byte = read_u8(stream)?;
        run += usize::from(byte);
        if run > left {
            return protocol(format!(
                "the VNC server sent a ZRLE run of {run} pixels where only {left} were left in the tile"
            ));
        }
        if byte != 255 {
            return Ok(run);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use flate2::{write::ZlibEncoder, Compression};

    use super::*;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    /// A test framebuffer with every blit recorded.
    fn painted(
        width: u32,
        height: u32,
        decode: impl FnOnce(&mut dyn FnMut(Area, &[u8])),
    ) -> Framebuffer {
        let mut framebuffer = Framebuffer::new(width, height);
        decode(&mut |area, pixels| framebuffer.blit(area, pixels));
        framebuffer
    }

    fn pixel_at(framebuffer: &Framebuffer, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * framebuffer.width() + x) * 4) as usize;
        framebuffer.rgba()[at..at + 4].try_into().unwrap()
    }

    fn wire(pixel: [u8; 4]) -> [u8; 4] {
        // The X byte is whatever the server leaves there; decoders must ignore it.
        [pixel[0], pixel[1], pixel[2], 0x5a]
    }

    #[test]
    fn raw_rows_become_opaque_pixels_in_place() {
        let area = Area::new(1, 1, 2, 2);
        let mut bytes = Vec::new();
        for colour in [RED, GREEN, BLUE, RED] {
            bytes.extend_from_slice(&wire(colour));
        }
        let framebuffer = painted(4, 3, |sink| {
            raw(&mut Cursor::new(bytes), area, sink).unwrap()
        });
        assert_eq!(pixel_at(&framebuffer, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&framebuffer, 1, 1), RED);
        assert_eq!(pixel_at(&framebuffer, 2, 1), GREEN);
        assert_eq!(pixel_at(&framebuffer, 1, 2), BLUE);
        assert_eq!(pixel_at(&framebuffer, 2, 2), RED);
    }

    #[test]
    fn copy_within_handles_overlap_in_both_directions() {
        let mut framebuffer = Framebuffer::new(1, 4);
        let colours = [RED, GREEN, BLUE, [9, 9, 9, 255]];
        for (y, colour) in colours.iter().enumerate() {
            framebuffer.blit(Area::new(0, y as u16, 1, 1), colour);
        }
        framebuffer.copy_within(0, 0, Area::new(0, 1, 1, 3));
        assert_eq!(pixel_at(&framebuffer, 0, 1), RED);
        assert_eq!(pixel_at(&framebuffer, 0, 2), GREEN);
        assert_eq!(pixel_at(&framebuffer, 0, 3), BLUE);
        framebuffer.copy_within(0, 1, Area::new(0, 0, 1, 3));
        assert_eq!(pixel_at(&framebuffer, 0, 0), RED);
        assert_eq!(pixel_at(&framebuffer, 0, 1), GREEN);
        assert_eq!(pixel_at(&framebuffer, 0, 2), BLUE);
    }

    #[test]
    fn resizing_keeps_the_shared_corner() {
        let mut framebuffer = Framebuffer::new(2, 2);
        framebuffer.blit(Area::new(0, 0, 2, 2), &[RED, GREEN, BLUE, RED].concat());
        framebuffer.resize(3, 1);
        assert_eq!((framebuffer.width(), framebuffer.height()), (3, 1));
        assert_eq!(pixel_at(&framebuffer, 0, 0), RED);
        assert_eq!(pixel_at(&framebuffer, 1, 0), GREEN);
        assert_eq!(pixel_at(&framebuffer, 2, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn hextile_uses_every_flag_and_carries_colours_between_tiles() {
        use hextile_flags::*;
        // 20x18: tiles (0,0 16x16), (16,0 4x16), (0,16 16x2), (16,16 4x2).
        let area = Area::new(0, 0, 20, 18);
        let mut bytes = Vec::new();
        // Tile 1: red background, green foreground, two plain subrects.
        bytes.push(BACKGROUND_SPECIFIED | FOREGROUND_SPECIFIED | ANY_SUBRECTS);
        bytes.extend_from_slice(&wire(RED));
        bytes.extend_from_slice(&wire(GREEN));
        bytes.push(2);
        bytes.extend_from_slice(&[(1 << 4) | 1, (2 << 4) | 1]); // 1,1 3x2
        bytes.extend_from_slice(&[(10 << 4) | 12, (5 << 4) | 3]); // 10,12 6x4
                                                                  // Tile 2: background carried over (red), one blue coloured subrect.
        bytes.push(ANY_SUBRECTS | SUBRECTS_COLOURED);
        bytes.push(1);
        bytes.extend_from_slice(&wire(BLUE));
        bytes.extend_from_slice(&[0, 3 << 4]); // 0,0 4x1
                                               // Tile 3: raw.
        bytes.push(RAW);
        for index in 0..32u8 {
            bytes.extend_from_slice(&[index, 2 * index, 3 * index, 0]);
        }
        // Tile 4: no flags: all background, still red.
        bytes.push(0);

        let framebuffer = painted(20, 18, |sink| {
            hextile(&mut Cursor::new(bytes), area, sink).unwrap()
        });
        assert_eq!(pixel_at(&framebuffer, 0, 0), RED);
        assert_eq!(pixel_at(&framebuffer, 1, 1), GREEN);
        assert_eq!(pixel_at(&framebuffer, 3, 2), GREEN);
        assert_eq!(pixel_at(&framebuffer, 4, 2), RED);
        assert_eq!(pixel_at(&framebuffer, 10, 12), GREEN);
        assert_eq!(pixel_at(&framebuffer, 15, 15), GREEN);
        assert_eq!(pixel_at(&framebuffer, 9, 15), RED);
        assert_eq!(pixel_at(&framebuffer, 16, 0), BLUE);
        assert_eq!(pixel_at(&framebuffer, 19, 0), BLUE);
        assert_eq!(pixel_at(&framebuffer, 19, 1), RED);
        assert_eq!(pixel_at(&framebuffer, 0, 16), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&framebuffer, 1, 16), [1, 2, 3, 255]);
        assert_eq!(pixel_at(&framebuffer, 15, 17), [31, 62, 93, 255]);
        assert_eq!(pixel_at(&framebuffer, 16, 16), RED);
        assert_eq!(pixel_at(&framebuffer, 19, 17), RED);
    }

    #[test]
    fn a_hextile_subrect_outside_its_tile_is_refused() {
        use hextile_flags::*;
        // A 4x4 tile with a subrect at 2,2 of 3x1.
        let bytes = vec![ANY_SUBRECTS, 1, (2 << 4) | 2, 2 << 4];
        let error = hextile(
            &mut Cursor::new(bytes),
            Area::new(0, 0, 4, 4),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(matches!(error, WireError::Protocol(message) if message.contains("outside")));
    }

    /// The pixels of a tile, as R, G, B triples, in a few colours.
    fn pattern(width: usize, height: usize, colours: &[[u8; 4]]) -> Vec<[u8; 4]> {
        (0..width * height)
            .map(|index| colours[(index / 3 + index / width) % colours.len()])
            .collect()
    }

    fn cpixel(colour: [u8; 4]) -> [u8; 3] {
        [colour[0], colour[1], colour[2]]
    }

    fn run_bytes(mut run: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        run -= 1;
        while run >= 255 {
            bytes.push(255);
            run -= 255;
        }
        bytes.push(run as u8);
        bytes
    }

    /// Test encoders for the ZRLE tile kinds.
    pub(crate) mod zrle_encode {
        use super::*;

        pub fn raw(pixels: &[[u8; 4]]) -> Vec<u8> {
            let mut bytes = vec![0];
            for pixel in pixels {
                bytes.extend_from_slice(&cpixel(*pixel));
            }
            bytes
        }

        pub fn solid(colour: [u8; 4]) -> Vec<u8> {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&cpixel(colour));
            bytes
        }

        pub fn packed(pixels: &[[u8; 4]], width: usize, palette: &[[u8; 4]]) -> Vec<u8> {
            let bits = match palette.len() {
                2 => 1,
                3 | 4 => 2,
                _ => 4,
            };
            let mut bytes = vec![palette.len() as u8];
            for colour in palette {
                bytes.extend_from_slice(&cpixel(*colour));
            }
            for row in pixels.chunks(width) {
                let mut byte = 0u8;
                let mut used = 0;
                for pixel in row {
                    let index = palette.iter().position(|colour| colour == pixel).unwrap() as u8;
                    byte |= index << (8 - bits - used);
                    used += bits;
                    if used == 8 {
                        bytes.push(byte);
                        byte = 0;
                        used = 0;
                    }
                }
                if used > 0 {
                    bytes.push(byte);
                }
            }
            bytes
        }

        fn runs(pixels: &[[u8; 4]]) -> Vec<([u8; 4], usize)> {
            let mut runs: Vec<([u8; 4], usize)> = Vec::new();
            for pixel in pixels {
                match runs.last_mut() {
                    Some((colour, count)) if colour == pixel => *count += 1,
                    _ => runs.push((*pixel, 1)),
                }
            }
            runs
        }

        pub fn plain_rle(pixels: &[[u8; 4]]) -> Vec<u8> {
            let mut bytes = vec![128];
            for (colour, count) in runs(pixels) {
                bytes.extend_from_slice(&cpixel(colour));
                bytes.extend_from_slice(&run_bytes(count));
            }
            bytes
        }

        pub fn palette_rle(pixels: &[[u8; 4]], palette: &[[u8; 4]]) -> Vec<u8> {
            let mut bytes = vec![128 + palette.len() as u8];
            for colour in palette {
                bytes.extend_from_slice(&cpixel(*colour));
            }
            for (colour, count) in runs(pixels) {
                let index = palette.iter().position(|entry| *entry == colour).unwrap() as u8;
                if count == 1 {
                    bytes.push(index);
                } else {
                    bytes.push(index | 0x80);
                    bytes.extend_from_slice(&run_bytes(count));
                }
            }
            bytes
        }
    }

    fn compressed(encoder: &mut ZlibEncoder<Vec<u8>>, tiles: &[u8]) -> Vec<u8> {
        encoder.write_all(tiles).unwrap();
        encoder.flush().unwrap();
        let data = std::mem::take(encoder.get_mut());
        let mut bytes = (data.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&data);
        bytes
    }

    fn assert_tile(framebuffer: &Framebuffer, area: Area, expected: &[[u8; 4]]) {
        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(
                    pixel_at(framebuffer, area.x + x, area.y + y),
                    expected[(y * area.width + x) as usize],
                    "pixel {x},{y} of {area:?}"
                );
            }
        }
    }

    #[test]
    fn zrle_decodes_every_tile_kind_and_shares_one_zlib_stream_across_rectangles() {
        // 128x70: tiles (0,0 64x64) (64,0 64x64) (0,64 64x6) (64,64 64x6).
        let big = pattern(
            64,
            64,
            &[RED, GREEN, BLUE, [7, 8, 9, 255], [200, 100, 50, 255]],
        );
        let two = pattern(64, 64, &[RED, BLUE]);
        let three = pattern(64, 6, &[RED, GREEN, BLUE]);
        let palette5 = [RED, GREEN, BLUE, [7, 8, 9, 255], [200, 100, 50, 255]];
        let mut first = Vec::new();
        first.extend(zrle_encode::raw(&big));
        first.extend(zrle_encode::packed(&two, 64, &[RED, BLUE]));
        first.extend(zrle_encode::packed(&three, 64, &[RED, GREEN, BLUE]));
        first.extend(zrle_encode::solid(GREEN));
        // The second rectangle: the same picture, other kinds of tile.
        let mut second = Vec::new();
        second.extend(zrle_encode::plain_rle(&big));
        second.extend(zrle_encode::palette_rle(&two, &[RED, BLUE]));
        second.extend(zrle_encode::packed(
            &three,
            64,
            &[RED, GREEN, BLUE, [1, 1, 1, 255], [2, 2, 2, 255]],
        ));
        second.extend(zrle_encode::palette_rle(&vec![GREEN; 64 * 6], &palette5));

        // Three rectangles of one stream: `first`, `first` again (which deflate encodes as
        // references back into the previous rectangle, so only an inflater that saw it can
        // read it), then `second`.
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let mut wire = compressed(&mut encoder, &first);
        wire.extend(compressed(&mut encoder, &first));
        wire.extend(compressed(&mut encoder, &second));

        let mut inflate = Decompress::new(true);
        let mut cursor = Cursor::new(wire);
        let framebuffer = painted(128, 210, |sink| {
            for top in [0, 70, 140] {
                zrle(&mut cursor, Area::new(0, top, 128, 70), &mut inflate, sink).unwrap();
            }
        });
        assert_eq!(cursor.position() as usize, cursor.get_ref().len());
        for offset in [0u16, 70] {
            assert_tile(&framebuffer, Area::new(0, offset, 64, 64), &big);
            assert_tile(&framebuffer, Area::new(64, offset, 64, 64), &two);
            assert_tile(&framebuffer, Area::new(0, offset + 64, 64, 6), &three);
            assert_tile(
                &framebuffer,
                Area::new(64, offset + 64, 64, 6),
                &vec![GREEN; 64 * 6],
            );
        }
        assert_tile(&framebuffer, Area::new(0, 140, 64, 64), &big);
        assert_tile(&framebuffer, Area::new(64, 140, 64, 64), &two);
        assert_tile(&framebuffer, Area::new(0, 204, 64, 6), &three);
        assert_tile(
            &framebuffer,
            Area::new(64, 204, 64, 6),
            &vec![GREEN; 64 * 6],
        );
    }

    #[test]
    fn a_fresh_inflater_cannot_read_a_later_rectangle() {
        let tiles = zrle_encode::raw(&pattern(8, 8, &[RED, GREEN, BLUE]));
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let _first = compressed(&mut encoder, &tiles);
        let second = compressed(&mut encoder, &tiles);
        let mut inflate = Decompress::new(true);
        let result = zrle(
            &mut Cursor::new(second),
            Area::new(0, 0, 8, 8),
            &mut inflate,
            &mut |_, _| {},
        );
        assert!(result.is_err());
    }

    #[test]
    fn zrle_refuses_runs_and_indexes_that_do_not_fit() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        // A 2x2 tile with a plain run of 5.
        let mut tiles = vec![128];
        tiles.extend_from_slice(&cpixel(RED));
        tiles.push(4);
        let wire = compressed(&mut encoder, &tiles);
        let mut inflate = Decompress::new(true);
        let error = zrle(
            &mut Cursor::new(wire),
            Area::new(0, 0, 2, 2),
            &mut inflate,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(&error, WireError::Protocol(message) if message.contains("run")),
            "{error:?}"
        );

        // A palette RLE tile naming entry 3 of a palette of 2.
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let mut tiles = vec![130];
        tiles.extend_from_slice(&cpixel(RED));
        tiles.extend_from_slice(&cpixel(BLUE));
        tiles.push(3);
        let wire = compressed(&mut encoder, &tiles);
        let mut inflate = Decompress::new(true);
        let error = zrle(
            &mut Cursor::new(wire),
            Area::new(0, 0, 1, 1),
            &mut inflate,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(&error, WireError::Protocol(message) if message.contains("index")),
            "{error:?}"
        );
    }

    #[test]
    fn a_short_zrle_rectangle_is_a_protocol_error_not_a_hang_up() {
        let tiles = zrle_encode::solid(RED);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let wire = compressed(&mut encoder, &tiles);
        let mut inflate = Decompress::new(true);
        // One solid tile cannot paint two tiles' worth of rectangle.
        let error = zrle(
            &mut Cursor::new(wire.clone()),
            Area::new(0, 0, 65, 1),
            &mut inflate,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(&error, WireError::Protocol(message) if message.contains("ended")),
            "{error:?}"
        );
        // The connection ending in the middle is the connection ending.
        let mut inflate = Decompress::new(true);
        let cut = &wire[..wire.len() - 2];
        let error = zrle(
            &mut Cursor::new(cut),
            Area::new(0, 0, 1, 1),
            &mut inflate,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(&error, WireError::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof),
            "{error:?}"
        );
    }

    #[test]
    fn sizes_beyond_the_caps_are_refused() {
        assert!(check_size(1920, 1080).is_ok());
        assert!(check_size(0, 0).is_ok());
        assert!(check_size(8192, 4096).is_ok());
        assert!(check_size(8193, 10).is_err());
        assert!(check_size(8192, 4097).is_err());
        let error = zrle(
            &mut Cursor::new(u32::MAX.to_be_bytes().to_vec()),
            Area::new(0, 0, 1, 1),
            &mut Decompress::new(true),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(error, WireError::Protocol(message) if message.contains("more than it can need"))
        );
    }
}
