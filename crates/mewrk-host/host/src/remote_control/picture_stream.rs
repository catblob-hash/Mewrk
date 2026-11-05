//! A live picture from a phone, read as fast as it comes: the bytes off the connection, the frames in them.
//!
//! One capture at a time costs a phone hundreds of milliseconds a picture. These are what the
//! devices can do instead: Android's `screenrecord` writes H.264 (decoded here by the system's
//! hardware decoder), WebDriverAgent serves MJPEG (each frame is a whole JPEG). Neither has an end
//! of frame a reader can wait for, so the bytes come through [`Chunks`], which can say "nothing
//! more has arrived for a moment".

use std::{
    io::Read,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
};

use super::transport::ConnectionHandle;

/// Chunks in the queue between the reading thread and the decoder. A decoder this far behind
/// makes the reader wait, and the sender slow down, rather than letting a backlog grow.
const QUEUE: usize = 64;

/// What a wait for more bytes found.
pub enum Next {
    Bytes(Vec<u8>),
    /// Nothing arrived in time.
    Quiet,
    /// The other end closed, or the read failed.
    Ended,
}

/// A connection's bytes, read on a thread of their own. Dropping this closes the connection,
/// which ends the thread.
pub struct Chunks {
    receiver: Receiver<Vec<u8>>,
    handle: ConnectionHandle,
}

impl Chunks {
    pub fn new(mut reader: Box<dyn Read + Send>, handle: ConnectionHandle) -> Self {
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        thread::spawn(move || {
            let mut chunk = vec![0_u8; 256 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(count) => {
                        if sender.send(chunk[..count].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        });
        Self { receiver, handle }
    }

    pub fn next(&self, wait: Duration) -> Next {
        match self.receiver.recv_timeout(wait) {
            Ok(bytes) => Next::Bytes(bytes),
            Err(RecvTimeoutError::Timeout) => Next::Quiet,
            Err(RecvTimeoutError::Disconnected) => Next::Ended,
        }
    }
}

impl Drop for Chunks {
    fn drop(&mut self) {
        self.handle.close();
    }
}

// ----- H.264 ---------------------------------------------------------------------------------------

/// The NAL units of an Annex B byte stream.
#[derive(Default)]
pub struct AnnexB {
    buffer: Vec<u8>,
    /// Where the unit being read starts, after its start code.
    unit: Option<usize>,
    /// How far the search for the next start code has come.
    searched: usize,
}

impl AnnexB {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole unit, without its start code. A unit is only known to be whole when the next
    /// one starts; [`Self::flush`] says the bytes so far are all there is for now.
    pub fn next_unit(&mut self) -> Option<Vec<u8>> {
        loop {
            let found = self.find_start_code()?;
            let code_at = found;
            let body_at = found + 3;
            self.searched = body_at;
            let Some(start) = self.unit.replace(body_at) else {
                // What came before the first start code is no part of the stream (a warning, say).
                continue;
            };
            let mut end = code_at;
            // A four-byte start code's leading zero belongs to it, not to the unit before.
            while end > start && self.buffer[end - 1] == 0 {
                end -= 1;
            }
            let unit = self.buffer[start..end].to_vec();
            self.compact();
            if !unit.is_empty() {
                return Some(unit);
            }
        }
    }

    /// The unit still being read, taken as whole because nothing has followed it for a while.
    /// Without this a picture that stops changing would never show its last frame.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        let start = self.unit.take()?;
        let unit = self.buffer[start..].to_vec();
        self.buffer.clear();
        self.searched = 0;
        (!unit.is_empty()).then_some(unit)
    }

    fn find_start_code(&self) -> Option<usize> {
        let from = self.searched.saturating_sub(2);
        self.buffer[from..].windows(3).position(|window| window == [0, 0, 1]).map(|at| from + at)
    }

    /// Drops what has been handed out, once it is worth moving what is left.
    fn compact(&mut self) {
        let Some(start) = self.unit else { return };
        if start > 1 << 20 {
            self.buffer.drain(..start);
            self.searched -= start;
            self.unit = Some(0);
        }
    }
}

/// `nal_unit_type`.
pub fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |byte| byte & 0x1F)
}

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// Whether this slice starts a new picture: `first_mb_in_slice`, as Exp-Golomb, is zero exactly
/// when the first bit after the header is set.
fn starts_picture(unit: &[u8]) -> bool {
    unit.get(1).is_some_and(|byte| byte & 0x80 != 0)
}

/// Sorts NAL units into pictures. Answers a picture, as its slices, when it is known to be whole.
#[derive(Default)]
pub struct Pictures {
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    slices: Vec<Vec<u8>>,
}

impl Pictures {
    /// Takes one unit and answers a picture this unit showed to be finished.
    pub fn push(&mut self, unit: Vec<u8>) -> Option<Vec<Vec<u8>>> {
        match nal_type(&unit) {
            NAL_SPS => {
                let finished = self.take();
                self.sps = Some(unit);
                finished
            }
            NAL_PPS => {
                let finished = self.take();
                self.pps = Some(unit);
                finished
            }
            NAL_SLICE | NAL_IDR => {
                let finished = if starts_picture(&unit) { self.take() } else { None };
                self.slices.push(unit);
                finished
            }
            // Delimiters and SEI carry nothing a decoder here needs, but they do end a picture.
            _ => self.take(),
        }
    }

    /// The picture being gathered, whole or not.
    pub fn take(&mut self) -> Option<Vec<Vec<u8>>> {
        (!self.slices.is_empty()).then(|| std::mem::take(&mut self.slices))
    }
}

/// A picture's slices as a decoder's sample: each with a four-byte length instead of a start code.
pub fn length_prefixed(slices: &[Vec<u8>]) -> Vec<u8> {
    let mut sample = Vec::with_capacity(slices.iter().map(|slice| slice.len() + 4).sum());
    for slice in slices {
        sample.extend_from_slice(&(slice.len() as u32).to_be_bytes());
        sample.extend_from_slice(slice);
    }
    sample
}

// ----- MJPEG ---------------------------------------------------------------------------------------

/// The JPEGs of a `multipart/x-mixed-replace` stream. It looks for the images themselves rather
/// than trusting the parts around them: WebDriverAgent's server is not the only one a screen may be
/// behind, and a JPEG says where it starts and ends.
#[derive(Default)]
pub struct Jpegs {
    buffer: Vec<u8>,
    /// How far the search for the end of the image has come.
    searched: usize,
}

/// The most a stream may send without showing a whole image.
const MAX_JPEG: usize = 64 * 1024 * 1024;

impl Jpegs {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > MAX_JPEG {
            return Err("the picture stream sent more than any one picture can be".to_owned());
        }
        Ok(())
    }

    /// The next whole image. When several wait, only the newest is worth decoding, so those before
    /// it are skipped.
    pub fn newest(&mut self) -> Option<Vec<u8>> {
        let mut found = None;
        while let Some(image) = self.next_image() {
            found = Some(image);
        }
        found
    }

    fn next_image(&mut self) -> Option<Vec<u8>> {
        let start = self.buffer.windows(3).position(|window| window == [0xFF, 0xD8, 0xFF])?;
        if start > 0 {
            self.buffer.drain(..start);
            self.searched = self.searched.saturating_sub(start);
        }
        let from = self.searched.max(2);
        // The last marker of the image: the stream's next bytes are headers, and entropy-coded data
        // stuffs a zero after every 0xFF, so these two bytes cannot occur inside it. A thumbnail in
        // the metadata could end with them; none of these servers sends one, and it would only
        // cost a frame.
        let Some(end) = self.buffer[from..].windows(2).position(|window| window == [0xFF, 0xD9]) else {
            self.searched = self.buffer.len().saturating_sub(1);
            return None;
        };
        let end = from + end + 2;
        let image = self.buffer[..end].to_vec();
        self.buffer.drain(..end);
        self.searched = 0;
        Some(image)
    }
}

/// An RGBA picture from a JPEG.
pub fn decode_jpeg(jpeg: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(jpeg, options);
    let pixels = decoder.decode().map_err(|error| format!("a frame of the stream could not be read: {error}"))?;
    let (width, height) = decoder.dimensions().ok_or_else(|| "a frame of the stream has no size".to_owned())?;
    Ok((width as u32, height as u32, pixels))
}

// ----- the system's decoder ------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub use decoder::{Decoded, H264Decoder};

#[cfg(target_os = "macos")]
mod decoder {
    //! VideoToolbox, which on every Mac this runs on is a hardware decoder. The picture comes
    //! back as BGRA in a surface the GPU can read, and is handed on as RGBA: one copy, on the
    //! stream's own thread.

    use std::{
        ffi::c_void,
        mem::MaybeUninit,
        os::raw::c_int,
        ptr::{null, null_mut},
        sync::Arc,
    };

    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CMFormatDescriptionRef = *const c_void;
    type CMBlockBufferRef = *const c_void;
    type CMSampleBufferRef = *const c_void;
    type CVImageBufferRef = *const c_void;
    type VTDecompressionSessionRef = *const c_void;
    type OSStatus = i32;

    const K_CF_NUMBER_SINT32: isize = 3;
    const K_CV_PIXEL_FORMAT_BGRA: i32 = i32::from_be_bytes(*b"BGRA");
    const K_CV_LOCK_READ_ONLY: u64 = 1;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, CVImageBufferRef, CMTime, CMTime);

    #[repr(C)]
    struct OutputCallbackRecord {
        callback: OutputCallback,
        context: *mut c_void,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFAllocatorNull: CFAllocatorRef;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        fn CFRelease(object: CFTypeRef);
        fn CFNumberCreate(allocator: CFAllocatorRef, kind: isize, value: *const c_void) -> CFNumberRef;
        fn CFDictionaryCreate(allocator: CFAllocatorRef, keys: *const CFTypeRef, values: *const CFTypeRef, count: isize, key_callbacks: *const c_void, value_callbacks: *const c_void) -> CFDictionaryRef;
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMVideoFormatDescriptionCreateFromH264ParameterSets(allocator: CFAllocatorRef, count: usize, pointers: *const *const u8, sizes: *const usize, length_size: c_int, out: *mut CMFormatDescriptionRef) -> OSStatus;
        fn CMBlockBufferCreateWithMemoryBlock(allocator: CFAllocatorRef, memory: *mut c_void, length: usize, block_allocator: CFAllocatorRef, source: *const c_void, offset: usize, data_length: usize, flags: u32, out: *mut CMBlockBufferRef) -> OSStatus;
        fn CMSampleBufferCreateReady(allocator: CFAllocatorRef, data: CMBlockBufferRef, format: CMFormatDescriptionRef, samples: isize, timings: isize, timing: *const c_void, sizes_count: isize, sizes: *const usize, out: *mut CMSampleBufferRef) -> OSStatus;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
        fn CVPixelBufferLockBaseAddress(buffer: CVImageBufferRef, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(buffer: CVImageBufferRef, flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddress(buffer: CVImageBufferRef) -> *const u8;
        fn CVPixelBufferGetBytesPerRow(buffer: CVImageBufferRef) -> usize;
        fn CVPixelBufferGetWidth(buffer: CVImageBufferRef) -> usize;
        fn CVPixelBufferGetHeight(buffer: CVImageBufferRef) -> usize;
    }

    #[link(name = "VideoToolbox", kind = "framework")]
    unsafe extern "C" {
        fn VTDecompressionSessionCreate(allocator: CFAllocatorRef, format: CMFormatDescriptionRef, decoder: CFDictionaryRef, destination: CFDictionaryRef, callback: *const OutputCallbackRecord, out: *mut VTDecompressionSessionRef) -> OSStatus;
        fn VTDecompressionSessionDecodeFrame(session: VTDecompressionSessionRef, sample: CMSampleBufferRef, flags: u32, context: *mut c_void, info: *mut u32) -> OSStatus;
        fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    }

    /// A picture out of the decoder.
    pub struct Decoded {
        pub width: u32,
        pub height: u32,
        pub rgba: Arc<[u8]>,
    }

    /// Where the callback puts the picture. Decoding is synchronous, so it fills this before
    /// `VTDecompressionSessionDecodeFrame` returns.
    #[derive(Default)]
    struct Slot {
        decoded: Option<Decoded>,
        failed: Option<OSStatus>,
    }

    /// BGRA rows to tightly packed, opaque RGBA.
    fn to_rgba(source: &[u8], stride: usize, width: usize, height: usize) -> Arc<[u8]> {
        let row_bytes = width * 4;
        let mut picture: Arc<[MaybeUninit<u8>]> = Arc::new_uninit_slice(row_bytes * height);
        let target = Arc::get_mut(&mut picture).expect("just made, so it has no other owner");
        for (row, out) in target.chunks_exact_mut(row_bytes).enumerate() {
            let line = &source[row * stride..row * stride + row_bytes];
            for (pixel, out) in line.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                out[0].write(pixel[2]);
                out[1].write(pixel[1]);
                out[2].write(pixel[0]);
                out[3].write(255);
            }
        }
        // SAFETY: the loop above wrote every byte: rows of `row_bytes`, `height` of them, which is
        // the whole slice.
        unsafe { picture.assume_init() }
    }

    extern "C" fn on_decoded(context: *mut c_void, _frame: *mut c_void, status: OSStatus, _info: u32, image: CVImageBufferRef, _pts: CMTime, _duration: CMTime) {
        // SAFETY: `context` is the `Slot` the session was created with, boxed for as long as the
        // session lives, and only the decoding thread touches it during the synchronous call.
        let slot = unsafe { &mut *(context as *mut Slot) };
        if status != 0 || image.is_null() {
            slot.failed = Some(status);
            return;
        }
        // SAFETY: `image` is a live pixel buffer for the length of this call; it is locked before
        // its memory is read, and the slice stays inside what it reports as its size.
        unsafe {
            if CVPixelBufferLockBaseAddress(image, K_CV_LOCK_READ_ONLY) != 0 {
                slot.failed = Some(-1);
                return;
            }
            let (width, height) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
            let stride = CVPixelBufferGetBytesPerRow(image);
            let base = CVPixelBufferGetBaseAddress(image);
            if !base.is_null() && stride >= width * 4 {
                let source = std::slice::from_raw_parts(base, stride * height);
                slot.decoded = Some(Decoded { width: width as u32, height: height as u32, rgba: to_rgba(source, stride, width, height) });
            }
            CVPixelBufferUnlockBaseAddress(image, K_CV_LOCK_READ_ONLY);
        }
    }

    /// One H.264 stream's decoder, remade whenever its parameter sets change.
    pub struct H264Decoder {
        session: VTDecompressionSessionRef,
        format: CMFormatDescriptionRef,
        slot: Box<Slot>,
        parameters: (Vec<u8>, Vec<u8>),
    }

    // SAFETY: the session and the format description are thread-safe CoreFoundation objects, and
    // this value is only ever used from one thread at a time, which `&mut self` enforces.
    unsafe impl Send for H264Decoder {}

    impl H264Decoder {
        pub fn new(sps: &[u8], pps: &[u8]) -> Result<Self, String> {
            let mut slot = Box::<Slot>::default();
            let mut format: CMFormatDescriptionRef = null();
            let pointers = [sps.as_ptr(), pps.as_ptr()];
            let sizes = [sps.len(), pps.len()];
            // SAFETY: the two arrays both hold two valid entries for the length of the call, which
            // copies them into the description it returns.
            let status = unsafe { CMVideoFormatDescriptionCreateFromH264ParameterSets(null(), 2, pointers.as_ptr(), sizes.as_ptr(), 4, &mut format) };
            if status != 0 {
                return Err(format!("the stream's parameter sets were refused ({status})"));
            }
            let pixel_format = K_CV_PIXEL_FORMAT_BGRA;
            let record = OutputCallbackRecord { callback: on_decoded, context: (&mut *slot as *mut Slot).cast() };
            let mut session: VTDecompressionSessionRef = null();
            // SAFETY: every object passed is live for the call, and the session retains what it
            // keeps. The record is copied. The slot the context points at is boxed and moves into
            // the value that owns the session, so it outlives every callback.
            let status = unsafe {
                let number = CFNumberCreate(null(), K_CF_NUMBER_SINT32, (&pixel_format as *const i32).cast());
                let key = kCVPixelBufferPixelFormatTypeKey;
                let destination = CFDictionaryCreate(null(), &key, &number, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
                let status = VTDecompressionSessionCreate(null(), format, null(), destination, &record, &mut session);
                CFRelease(destination);
                CFRelease(number);
                status
            };
            if status != 0 {
                // SAFETY: made above and not handed to anything.
                unsafe { CFRelease(format) };
                return Err(format!("the system's decoder would not start ({status})"));
            }
            Ok(Self { session, format, slot, parameters: (sps.to_vec(), pps.to_vec()) })
        }

        pub fn is_for(&self, sps: &[u8], pps: &[u8]) -> bool {
            self.parameters.0 == sps && self.parameters.1 == pps
        }

        /// Decodes one picture, its slices each with a four-byte length.
        pub fn decode(&mut self, sample: &mut [u8]) -> Result<Option<Decoded>, String> {
            let mut block: CMBlockBufferRef = null();
            let mut buffer: CMSampleBufferRef = null();
            let size = sample.len();
            let mut info = 0_u32;
            // SAFETY: `sample` outlives both buffers, which wrap it without copying and are released
            // here; `kCFAllocatorNull` says not to free it. Decoding is synchronous (flags 0), so
            // nothing reads it after this block.
            let status = unsafe {
                let status = CMBlockBufferCreateWithMemoryBlock(null(), sample.as_mut_ptr().cast(), size, kCFAllocatorNull, null(), 0, size, 0, &mut block);
                if status != 0 {
                    return Err(format!("a frame could not be wrapped ({status})"));
                }
                let status = CMSampleBufferCreateReady(null(), block, self.format, 1, 0, null(), 1, &size, &mut buffer);
                if status != 0 {
                    CFRelease(block);
                    return Err(format!("a frame could not be wrapped ({status})"));
                }
                let status = VTDecompressionSessionDecodeFrame(self.session, buffer, 0, null_mut(), &mut info);
                CFRelease(buffer);
                CFRelease(block);
                status
            };
            if status != 0 {
                return Err(format!("the decoder refused a frame ({status})"));
            }
            if let Some(failed) = self.slot.failed.take() {
                return Err(format!("the decoder could not read a frame ({failed})"));
            }
            Ok(self.slot.decoded.take())
        }
    }

    impl Drop for H264Decoder {
        fn drop(&mut self) {
            // SAFETY: both were made by `new`, and this is the last use of them.
            unsafe {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                CFRelease(self.format);
            }
        }
    }
}

/// An H.264 stream of flat frames, as a phone's `screenrecord` would write it.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn test_stream(width: usize, height: usize, colours: &[[u8; 3]]) -> Vec<u8> {
    tests::encode::annex_b(width, height, colours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_splits_units_across_chunks_and_both_start_codes() {
        let mut stream = AnnexB::default();
        stream.push(b"warning\n\x00\x00\x00\x01\x67abc\x00\x00");
        assert_eq!(stream.next_unit(), None);
        stream.push(b"\x01\x68de\x00\x00\x00\x01\x65fgh");
        assert_eq!(stream.next_unit().as_deref(), Some(&b"\x67abc"[..]));
        assert_eq!(stream.next_unit().as_deref(), Some(&b"\x68de"[..]));
        assert_eq!(stream.next_unit(), None);
        assert_eq!(stream.flush().as_deref(), Some(&b"\x65fgh"[..]));
        assert_eq!(stream.flush(), None);
    }

    #[test]
    fn a_start_code_split_over_two_chunks_is_still_found() {
        let mut stream = AnnexB::default();
        stream.push(b"\x00\x00\x01\x67a\x00");
        assert_eq!(stream.next_unit(), None);
        stream.push(b"\x00");
        assert_eq!(stream.next_unit(), None);
        stream.push(b"\x01\x68b");
        assert_eq!(stream.next_unit().as_deref(), Some(&b"\x67a"[..]));
    }

    #[test]
    fn pictures_end_at_the_next_pictures_first_slice() {
        let mut pictures = Pictures::default();
        assert!(pictures.push(vec![0x67, 1]).is_none());
        assert!(pictures.push(vec![0x68, 2]).is_none());
        assert_eq!(pictures.sps.as_deref(), Some(&[0x67, 1][..]));
        // First slice of a picture, and a later slice of the same one.
        assert!(pictures.push(vec![0x65, 0x80, 1]).is_none());
        assert!(pictures.push(vec![0x65, 0x40, 2]).is_none());
        let whole = pictures.push(vec![0x41, 0x80, 3]).expect("the next picture ended it");
        assert_eq!(whole.len(), 2);
        assert_eq!(length_prefixed(&whole)[..7], [0, 0, 0, 3, 0x65, 0x80, 1]);
        assert_eq!(pictures.take().map(|slices| slices.len()), Some(1));
    }

    #[test]
    fn jpegs_come_out_of_their_parts_newest_first() {
        let image = |n: u8| vec![0xFF, 0xD8, 0xFF, 0xE0, n, 0xFF, 0x00, 7, 0xFF, 0xD9];
        let part = |n: u8| {
            let mut part = b"--BoundaryString\r\nContent-type: image/jpg\r\nContent-Length: 10\r\n\r\n".to_vec();
            part.extend(image(n));
            part.extend_from_slice(b"\r\n\r\n");
            part
        };
        let mut jpegs = Jpegs::default();
        jpegs.push(b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace\r\n\r\n").unwrap();
        assert_eq!(jpegs.newest(), None);
        let first = part(1);
        jpegs.push(&first[..first.len() - 8]).unwrap();
        assert_eq!(jpegs.newest(), None);
        jpegs.push(&first[first.len() - 8..]).unwrap();
        assert_eq!(jpegs.newest(), Some(image(1)));
        jpegs.push(&part(2)).unwrap();
        jpegs.push(&part(3)).unwrap();
        assert_eq!(jpegs.newest(), Some(image(3)), "only the newest of what waited is worth decoding");
    }

    /// The real decoder against the real encoder of the same machine: frames in, the same colours out.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_decoder_reads_an_annex_b_stream_back_to_its_colours() {
        let colours = [[220_u8, 30, 30], [30, 200, 40], [30, 40, 220]];
        let stream = encode::annex_b(320, 240, &colours);
        let mut split = AnnexB::default();
        split.push(&stream);
        let mut pictures = Pictures::default();
        let mut decoder: Option<H264Decoder> = None;
        let mut seen = Vec::new();
        let mut finish = |pictures: &Pictures, decoder: &mut Option<H264Decoder>, slices: Vec<Vec<u8>>| {
            let (sps, pps) = (pictures.sps.as_ref().unwrap(), pictures.pps.as_ref().unwrap());
            let decoder = decoder.get_or_insert_with(|| H264Decoder::new(sps, pps).unwrap());
            let mut sample = length_prefixed(&slices);
            if let Some(picture) = decoder.decode(&mut sample).unwrap() {
                let middle = (picture.height as usize / 2 * picture.width as usize + picture.width as usize / 2) * 4;
                seen.push((picture.width, picture.height, [picture.rgba[middle], picture.rgba[middle + 1], picture.rgba[middle + 2], picture.rgba[middle + 3]]));
            }
        };
        while let Some(unit) = split.next_unit() {
            if let Some(slices) = pictures.push(unit) {
                finish(&pictures, &mut decoder, slices);
            }
        }
        let last = split.flush().and_then(|unit| pictures.push(unit)).into_iter().chain(pictures.take()).collect::<Vec<_>>();
        for slices in last {
            finish(&pictures, &mut decoder, slices);
        }
        assert!(seen.len() >= colours.len(), "{} pictures", seen.len());
        for (width, height, pixel) in &seen {
            assert_eq!((*width, *height), (320, 240));
            assert_eq!(pixel[3], 255);
        }
        // Colour accuracy is the encoder's business; which channel leads is ours to get right.
        let leads = |pixel: &[u8; 4]| (0..3).max_by_key(|&channel| pixel[channel]).unwrap();
        assert_eq!(leads(&seen[0].2), 0, "{:?}", seen[0].2);
        let last = &seen[seen.len() - 1].2;
        assert_eq!(leads(last), 2, "{last:?}");
    }

    #[cfg(target_os = "macos")]
    pub(super) mod encode {
        //! VideoToolbox's encoder, to have a real stream without a phone.

        use std::{ffi::c_void, ptr::null};

        type CFTypeRef = *const c_void;

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CMTime {
            value: i64,
            timescale: i32,
            flags: u32,
            epoch: i64,
        }

        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            static kCFBooleanTrue: CFTypeRef;
            static kCFBooleanFalse: CFTypeRef;
            fn CFRelease(object: CFTypeRef);
        }

        #[link(name = "CoreVideo", kind = "framework")]
        unsafe extern "C" {
            fn CVPixelBufferCreate(allocator: CFTypeRef, width: usize, height: usize, format: u32, attributes: CFTypeRef, out: *mut CFTypeRef) -> i32;
            fn CVPixelBufferLockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32;
            fn CVPixelBufferUnlockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32;
            fn CVPixelBufferGetBaseAddress(buffer: CFTypeRef) -> *mut u8;
            fn CVPixelBufferGetBytesPerRow(buffer: CFTypeRef) -> usize;
        }

        #[link(name = "CoreMedia", kind = "framework")]
        unsafe extern "C" {
            fn CMSampleBufferGetDataBuffer(sample: CFTypeRef) -> CFTypeRef;
            fn CMSampleBufferGetFormatDescription(sample: CFTypeRef) -> CFTypeRef;
            fn CMBlockBufferGetDataLength(block: CFTypeRef) -> usize;
            fn CMBlockBufferCopyDataBytes(block: CFTypeRef, offset: usize, length: usize, out: *mut c_void) -> i32;
            fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format: CFTypeRef, index: usize, out: *mut *const u8, size: *mut usize, count: *mut usize, length: *mut i32) -> i32;
        }

        #[link(name = "VideoToolbox", kind = "framework")]
        unsafe extern "C" {
            static kVTCompressionPropertyKey_RealTime: CFTypeRef;
            static kVTCompressionPropertyKey_AllowFrameReordering: CFTypeRef;
            fn VTCompressionSessionCreate(allocator: CFTypeRef, width: i32, height: i32, codec: u32, specification: CFTypeRef, source: CFTypeRef, compressed: CFTypeRef, callback: extern "C" fn(*mut c_void, *mut c_void, i32, u32, CFTypeRef), context: *mut c_void, out: *mut CFTypeRef) -> i32;
            fn VTCompressionSessionEncodeFrame(session: CFTypeRef, image: CFTypeRef, pts: CMTime, duration: CMTime, properties: CFTypeRef, context: *mut c_void, info: *mut u32) -> i32;
            fn VTCompressionSessionCompleteFrames(session: CFTypeRef, until: CMTime) -> i32;
            fn VTCompressionSessionInvalidate(session: CFTypeRef);
            fn VTSessionSetProperty(session: CFTypeRef, key: CFTypeRef, value: CFTypeRef) -> i32;
        }

        extern "C" fn on_encoded(context: *mut c_void, _frame: *mut c_void, status: i32, _info: u32, sample: CFTypeRef) {
            assert_eq!(status, 0);
            // SAFETY: the context is the `Vec` in `annex_b`, alive until the session is invalidated.
            let out = unsafe { &mut *(context as *mut Vec<u8>) };
            // SAFETY: `sample` is live for this call, and the buffers and pointers it gives are
            // read within it, inside the lengths they report.
            unsafe {
                let format = CMSampleBufferGetFormatDescription(sample);
                for index in 0..2 {
                    let (mut pointer, mut size): (*const u8, usize) = (null(), 0);
                    assert_eq!(CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, index, &mut pointer, &mut size, std::ptr::null_mut(), std::ptr::null_mut()), 0);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(std::slice::from_raw_parts(pointer, size));
                }
                let block = CMSampleBufferGetDataBuffer(sample);
                let length = CMBlockBufferGetDataLength(block);
                let mut bytes = vec![0_u8; length];
                assert_eq!(CMBlockBufferCopyDataBytes(block, 0, length, bytes.as_mut_ptr().cast()), 0);
                let mut at = 0;
                while at + 4 <= length {
                    let size = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(&bytes[at + 4..at + 4 + size]);
                    at += 4 + size;
                }
            }
        }

        /// A stream of flat frames in these colours, as a phone's `screenrecord` would write them.
        pub fn annex_b(width: usize, height: usize, colours: &[[u8; 3]]) -> Vec<u8> {
            let mut out: Vec<u8> = Vec::new();
            let forever = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
            // SAFETY: each object is made here, used while alive, and released here; the output
            // vector outlives the session, which is invalidated before it is returned.
            unsafe {
                let mut session: CFTypeRef = null();
                assert_eq!(VTCompressionSessionCreate(null(), width as i32, height as i32, u32::from_be_bytes(*b"avc1"), null(), null(), null(), on_encoded, (&mut out as *mut Vec<u8>).cast(), &mut session), 0);
                VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
                // `screenrecord` writes no B-frames, so decode order is display order.
                VTSessionSetProperty(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
                for (index, colour) in colours.iter().enumerate() {
                    let mut pixels: CFTypeRef = null();
                    assert_eq!(CVPixelBufferCreate(null(), width, height, u32::from_be_bytes(*b"BGRA"), null(), &mut pixels), 0);
                    CVPixelBufferLockBaseAddress(pixels, 0);
                    let base = CVPixelBufferGetBaseAddress(pixels);
                    let stride = CVPixelBufferGetBytesPerRow(pixels);
                    for y in 0..height {
                        let row = std::slice::from_raw_parts_mut(base.add(y * stride), width * 4);
                        for pixel in row.chunks_exact_mut(4) {
                            pixel.copy_from_slice(&[colour[2], colour[1], colour[0], 255]);
                        }
                    }
                    CVPixelBufferUnlockBaseAddress(pixels, 0);
                    let pts = CMTime { value: index as i64, timescale: 30, flags: 1, epoch: 0 };
                    let duration = CMTime { value: 1, timescale: 30, flags: 1, epoch: 0 };
                    assert_eq!(VTCompressionSessionEncodeFrame(session, pixels, pts, duration, null(), std::ptr::null_mut(), std::ptr::null_mut()), 0);
                    CFRelease(pixels);
                }
                assert_eq!(VTCompressionSessionCompleteFrames(session, forever), 0);
                VTCompressionSessionInvalidate(session);
                CFRelease(session);
            }
            assert!(!out.is_empty());
            out
        }
    }
}
