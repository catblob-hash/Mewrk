//! Reading a video's frames in order, through AVFoundation, which on every Mac this runs on means the hardware decoder.
//!
//! A frame comes back in an IOSurface, ready for the GPU to read as it is. Anywhere else there is no reader yet, so a video can't be
//! imported there.

use std::path::Path;

/// What a video is, as it is meant to be shown: a phone's sideways recording has its sizes turned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub frame_rate: f32,
}

#[cfg(target_os = "macos")]
pub use mac::{probe, VideoFrame, VideoReader};

#[cfg(not(target_os = "macos"))]
pub fn probe(_path: &Path) -> Result<VideoInfo, String> {
    Err("unreadable".to_owned())
}

/// The first frame, no larger than it is, for the library and for what shows until the first real frame arrives.
pub fn poster(path: &Path) -> Result<crate::Pixels, String> {
    #[cfg(target_os = "macos")]
    {
        let mut reader = VideoReader::open(path, None, 0.0).map_err(|_| "unreadable".to_owned())?;
        reader.next_frame().ok_or_else(|| "unreadable".to_owned())?.to_pixels()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err("unreadable".to_owned())
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::path::Path;
    use std::ptr::null_mut;
    use std::sync::mpsc;

    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSArray, NSError, NSMutableDictionary, NSNumber, NSSize, NSString, NSURL};

    use super::VideoInfo;
    use crate::Pixels;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    const CM_TIME_VALID: u32 = 1;

    impl CMTime {
        fn seconds(self) -> f64 {
            if self.flags & CM_TIME_VALID != 0 && self.timescale > 0 { self.value as f64 / f64::from(self.timescale) } else { 0.0 }
        }

        fn of_seconds(seconds: f64) -> Self {
            const SCALE: i32 = 600_000;
            Self { value: (seconds * f64::from(SCALE)) as i64, timescale: SCALE, flags: CM_TIME_VALID, epoch: 0 }
        }
    }

    // SAFETY: the same layout as CoreMedia's `CMTime`, which is an anonymous struct there.
    unsafe impl Encode for CMTime {
        const ENCODING: Encoding = Encoding::Struct("?", &[Encoding::LongLong, Encoding::Int, Encoding::UInt, Encoding::LongLong]);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTimeRange {
        start: CMTime,
        duration: CMTime,
    }

    // SAFETY: the same layout as CoreMedia's `CMTimeRange`.
    unsafe impl Encode for CMTimeRange {
        const ENCODING: Encoding = Encoding::Struct("?", &[CMTime::ENCODING, CMTime::ENCODING]);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGAffineTransform {
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        tx: f64,
        ty: f64,
    }

    // SAFETY: the same layout as CoreGraphics' struct on a 64-bit Mac.
    unsafe impl Encode for CGAffineTransform {
        const ENCODING: Encoding = Encoding::Struct("CGAffineTransform", &[f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING]);
    }

    #[repr(C)]
    struct OpaqueSampleBuffer(c_void);

    // SAFETY: what AVFoundation's headers declare `CMSampleBufferRef` as, for the encoding its methods are checked against.
    unsafe impl RefEncode for OpaqueSampleBuffer {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
    }

    type CFTypeRef = *const c_void;
    type CVPixelBufferRef = *const c_void;
    const K_CV_PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
    const K_CV_LOCK_READ_ONLY: u64 = 1;
    const READER_READING: isize = 1;
    const READER_COMPLETED: isize = 2;

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeVideo: &'static NSString;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRetain(object: CFTypeRef) -> CFTypeRef;
        fn CFRelease(object: CFTypeRef);
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMSampleBufferGetImageBuffer(buffer: *mut OpaqueSampleBuffer) -> CVPixelBufferRef;
        fn CMSampleBufferGetOutputPresentationTimeStamp(buffer: *mut OpaqueSampleBuffer) -> CMTime;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        static kCVPixelBufferPixelFormatTypeKey: &'static NSString;
        static kCVPixelBufferMetalCompatibilityKey: &'static NSString;
        static kCVPixelBufferIOSurfacePropertiesKey: &'static NSString;
        static kCVPixelBufferWidthKey: &'static NSString;
        static kCVPixelBufferHeightKey: &'static NSString;
        fn CVPixelBufferGetIOSurface(buffer: CVPixelBufferRef) -> *mut c_void;
        fn CVPixelBufferGetWidth(buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetHeight(buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetBytesPerRow(buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetBaseAddress(buffer: CVPixelBufferRef) -> *const u8;
        fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    }

    /// Holds a decoded frame's memory: the pool only gets the surface back once this goes.
    struct PixelBuffer(CVPixelBufferRef);

    // SAFETY: a pixel buffer is reference counted with atomics, and nothing here writes to its pixels.
    unsafe impl Send for PixelBuffer {}

    impl Drop for PixelBuffer {
        fn drop(&mut self) {
            // SAFETY: this owns the one reference `next_frame` took.
            unsafe { CFRelease(self.0) };
        }
    }

    pub struct VideoFrame {
        /// When it shows, in seconds from the start of the video.
        pub time: f64,
        pub width: u32,
        pub height: u32,
        buffer: PixelBuffer,
    }

    impl VideoFrame {
        /// The frame as the renderer takes it, for nothing but a reference count.
        pub fn into_surface(self) -> Option<fluent::SurfaceFrame> {
            // SAFETY: the buffer is alive, and it asked for Metal-compatible memory when the reader was made. The surface it returns is
            // kept alive by the buffer, which `keep` holds for as long as the renderer holds the surface.
            let surface = unsafe { CVPixelBufferGetIOSurface(self.buffer.0) };
            (!surface.is_null()).then(|| fluent::SurfaceFrame { surface, width: self.width, height: self.height, keep: Box::new(self.buffer) })
        }

        /// A copy of the frame in memory, as RGBA.
        pub fn to_pixels(&self) -> Result<Pixels, String> {
            let buffer = self.buffer.0;
            // SAFETY: the pixels are only read while locked, and the slice stays inside the size the buffer reports.
            unsafe {
                if CVPixelBufferLockBaseAddress(buffer, K_CV_LOCK_READ_ONLY) != 0 {
                    return Err("a frame that can't be read".to_owned());
                }
                let (width, height) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer));
                let stride = CVPixelBufferGetBytesPerRow(buffer);
                let base = CVPixelBufferGetBaseAddress(buffer);
                let mut data = Vec::with_capacity(width * height * 4);
                if !base.is_null() {
                    let source = std::slice::from_raw_parts(base, stride * height);
                    for row in source.chunks_exact(stride) {
                        for pixel in row[..width * 4].chunks_exact(4) {
                            data.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                        }
                    }
                }
                CVPixelBufferUnlockBaseAddress(buffer, K_CV_LOCK_READ_ONLY);
                if data.is_empty() {
                    return Err("a frame that can't be read".to_owned());
                }
                Ok(Pixels { width: width as u32, height: height as u32, data })
            }
        }
    }

    struct Asset {
        asset: Retained<AnyObject>,
        track: Retained<AnyObject>,
        info: VideoInfo,
        /// Needs turning to be shown as it is meant to, which the plain track output doesn't do.
        turned: bool,
    }

    struct SendTracks(Option<Retained<NSArray<AnyObject>>>);

    // SAFETY: an array of tracks only passes from the block to the thread waiting for it, and tracks are safe to use from any thread.
    unsafe impl Send for SendTracks {}

    fn open_asset(path: &Path) -> Result<Asset, String> {
        let path = path.to_str().ok_or_else(|| "a path that isn't text".to_owned())?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        // SAFETY: each of these is what AVFoundation declares, with those types. The asset and its track stay retained for as long as anything reads them.
        unsafe {
            let asset: Option<Retained<AnyObject>> = msg_send![class!(AVURLAsset), URLAssetWithURL: &*url, options: null_mut::<AnyObject>()];
            let asset = asset.ok_or_else(|| "no asset".to_owned())?;
            let (sender, receiver) = mpsc::channel();
            let block = block2::RcBlock::new(move |tracks: *mut NSArray<AnyObject>, _error: *mut NSError| {
                let tracks = if tracks.is_null() { None } else { Retained::retain(tracks) };
                let _ = sender.send(SendTracks(tracks));
            });
            let _: () = msg_send![&*asset, loadTracksWithMediaType: AVMediaTypeVideo, completionHandler: &*block];
            let tracks = receiver.recv().map_err(|e| e.to_string())?.0.ok_or_else(|| "no video in it".to_owned())?;
            let track = tracks.firstObject().ok_or_else(|| "no video in it".to_owned())?;
            let natural: NSSize = msg_send![&*track, naturalSize];
            let transform: CGAffineTransform = msg_send![&*track, preferredTransform];
            let range: CMTimeRange = msg_send![&*track, timeRange];
            let frame_rate: f32 = msg_send![&*track, nominalFrameRate];
            let turned = transform.a != 1.0 || transform.d != 1.0 || transform.b != 0.0 || transform.c != 0.0;
            let sideways = transform.a.abs() < transform.b.abs();
            let (width, height) = if sideways { (natural.height, natural.width) } else { (natural.width, natural.height) };
            if width < 1.0 || height < 1.0 {
                return Err("no video in it".to_owned());
            }
            let info = VideoInfo { width: width.round() as u32, height: height.round() as u32, duration: range.duration.seconds(), frame_rate };
            Ok(Asset { asset, track, info, turned })
        }
    }

    /// What a video is, without decoding any of it.
    pub fn probe(path: &Path) -> Result<VideoInfo, String> {
        autoreleasepool(|_| open_asset(path).map(|asset| asset.info))
    }

    /// One pass over a video from a given time. Asked for a size, it decodes to that size in hardware, so that a window of a video far
    /// larger than itself, or a bake that wants a few hundred pixels, doesn't pay for all of them.
    pub struct VideoReader {
        reader: Retained<AnyObject>,
        output: Retained<AnyObject>,
        pub info: VideoInfo,
        ended: bool,
    }

    impl VideoReader {
        pub fn open(path: &Path, size: Option<(u32, u32)>, from: f64) -> Result<Self, String> {
            autoreleasepool(|_| Self::open_inner(path, size, from))
        }

        fn open_inner(path: &Path, size: Option<(u32, u32)>, from: f64) -> Result<Self, String> {
            let Asset { asset, track, info, turned } = open_asset(path)?;
            // SAFETY: each of these is what AVFoundation and Foundation declare, and every object stays retained for as long as it is used.
            unsafe {
                let settings = NSMutableDictionary::<NSString, AnyObject>::new();
                let set = |key: &NSString, value: &AnyObject| {
                    let _: () = msg_send![&*settings, setObject: value, forKey: key];
                };
                set(kCVPixelBufferPixelFormatTypeKey, &NSNumber::new_u32(K_CV_PIXEL_FORMAT_BGRA));
                set(kCVPixelBufferMetalCompatibilityKey, &NSNumber::new_bool(true));
                let no_properties: Retained<AnyObject> = msg_send![class!(NSDictionary), dictionary];
                set(kCVPixelBufferIOSurfacePropertiesKey, &no_properties);
                // A turned video goes through a composition, which sets its own size.
                if let Some((width, height)) = size.filter(|_| !turned) {
                    set(kCVPixelBufferWidthKey, &NSNumber::new_u32(width));
                    set(kCVPixelBufferHeightKey, &NSNumber::new_u32(height));
                }
                let mut error: *mut NSError = null_mut();
                let reader: Option<Retained<AnyObject>> = msg_send![class!(AVAssetReader), assetReaderWithAsset: &*asset, error: &mut error];
                let reader = reader.ok_or_else(|| "no reader".to_owned())?;
                let output: Option<Retained<AnyObject>> = if turned {
                    let tracks = NSArray::from_retained_slice(std::slice::from_ref(&track));
                    let composition: Option<Retained<AnyObject>> = msg_send![class!(AVMutableVideoComposition), videoCompositionWithPropertiesOfAsset: &*asset];
                    let output: Option<Retained<AnyObject>> = msg_send![class!(AVAssetReaderVideoCompositionOutput), assetReaderVideoCompositionOutputWithVideoTracks: &*tracks, videoSettings: &*settings];
                    if let (Some(output), Some(composition)) = (&output, &composition) {
                        let _: () = msg_send![&**output, setVideoComposition: &**composition];
                    }
                    output
                } else {
                    msg_send![class!(AVAssetReaderTrackOutput), assetReaderTrackOutputWithTrack: &*track, outputSettings: &*settings]
                };
                let output = output.ok_or_else(|| "no output".to_owned())?;
                let _: () = msg_send![&*output, setAlwaysCopiesSampleData: false];
                let can_add: bool = msg_send![&*reader, canAddOutput: &*output];
                if !can_add {
                    return Err("this video can't be read".to_owned());
                }
                let _: () = msg_send![&*reader, addOutput: &*output];
                if from > 0.0 {
                    let range = CMTimeRange { start: CMTime::of_seconds(from), duration: CMTime::of_seconds((info.duration - from).max(0.0)) };
                    let _: () = msg_send![&*reader, setTimeRange: range];
                }
                let started: bool = msg_send![&*reader, startReading];
                if !started {
                    return Err("this video can't be read".to_owned());
                }
                Ok(Self { reader, output, info, ended: false })
            }
        }

        /// The next frame in the order they show, waiting for the decoder if it has to. `None` at the end.
        pub fn next_frame(&mut self) -> Option<VideoFrame> {
            if self.ended {
                return None;
            }
            autoreleasepool(|_| {
                // SAFETY: `copyNextSampleBuffer` hands over one reference, given back below. The frame takes its own on the pixel buffer.
                unsafe {
                    let sample: *mut OpaqueSampleBuffer = msg_send![&*self.output, copyNextSampleBuffer];
                    if sample.is_null() {
                        self.ended = true;
                        return None;
                    }
                    let image = CMSampleBufferGetImageBuffer(sample);
                    let frame = (!image.is_null()).then(|| {
                        CFRetain(image);
                        VideoFrame { time: CMSampleBufferGetOutputPresentationTimeStamp(sample).seconds(), width: CVPixelBufferGetWidth(image) as u32, height: CVPixelBufferGetHeight(image) as u32, buffer: PixelBuffer(image) }
                    });
                    CFRelease(sample as CFTypeRef);
                    if frame.is_none() {
                        self.ended = true;
                    }
                    frame
                }
            })
        }

        /// Whether it stopped because the video ended, and not because it couldn't go on.
        pub fn ended_cleanly(&self) -> bool {
            // SAFETY: a plain property of a live reader.
            let status: isize = unsafe { msg_send![&*self.reader, status] };
            status == READER_COMPLETED || status == READER_READING
        }
    }

    impl Drop for VideoReader {
        fn drop(&mut self) {
            // SAFETY: it may be called in any state, and a reader that has finished ignores it.
            let _: () = unsafe { msg_send![&*self.reader, cancelReading] };
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn a_video_says_what_it_is_without_being_decoded() {
        let info = probe(&fixture("counting.mp4")).unwrap();
        assert_eq!((info.width, info.height), (64, 36));
        assert!((info.duration - 2.0).abs() < 0.01, "{info:?}");
        assert!((info.frame_rate - 8.0).abs() < 0.5, "{info:?}");
        assert!(probe(Path::new("/nonexistent.mp4")).is_err());
    }

    #[test]
    fn frames_come_out_in_order_with_their_times_and_their_own_pixels() {
        let mut reader = VideoReader::open(&fixture("counting.mp4"), None, 0.0).unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = reader.next_frame() {
            frames.push(frame);
        }
        assert!(reader.ended_cleanly());
        assert_eq!(frames.len(), 16);
        assert!((frames[8].time - 1.0).abs() < 0.01, "{}", frames[8].time);
        let red = |frame: &VideoFrame| frame.to_pixels().unwrap().data[32 * 18 * 4];
        assert!(red(&frames[0]) < 40, "{}", red(&frames[0]));
        assert!(red(&frames[15]) > 200, "{}", red(&frames[15]));
        assert!(red(&frames[8]) > red(&frames[4]));
    }

    #[test]
    fn the_hardware_scales_on_the_way_out_when_asked_for_a_size() {
        let mut reader = VideoReader::open(&fixture("counting.mp4"), Some((32, 18)), 0.0).unwrap();
        let frame = reader.next_frame().unwrap();
        assert_eq!((frame.width, frame.height), (32, 18));
        assert_eq!(frame.to_pixels().unwrap().data.len(), 32 * 18 * 4);
    }

    #[test]
    fn reading_can_start_partway() {
        let mut reader = VideoReader::open(&fixture("counting.mp4"), None, 1.0).unwrap();
        let first = reader.next_frame().unwrap();
        assert!((first.time - 1.0).abs() < 0.2, "{}", first.time);
    }

    #[test]
    fn a_frame_is_handed_over_in_its_surface() {
        let mut reader = VideoReader::open(&fixture("counting.mp4"), None, 0.0).unwrap();
        let surface = reader.next_frame().unwrap().into_surface().unwrap();
        assert_eq!((surface.width, surface.height), (64, 36));
        assert!(!surface.surface.is_null());
    }

    #[test]
    fn a_sideways_recording_comes_out_turned() {
        let info = probe(&fixture("rotated.mp4")).unwrap();
        assert_eq!((info.width, info.height), (36, 64));
        let mut reader = VideoReader::open(&fixture("rotated.mp4"), None, 0.0).unwrap();
        let frame = reader.next_frame().unwrap();
        assert_eq!((frame.width, frame.height), (36, 64));
    }

    #[test]
    fn the_first_frame_is_the_poster_and_what_is_no_video_has_none() {
        let poster_of = poster(&fixture("counting.mp4")).unwrap();
        assert_eq!((poster_of.width, poster_of.height), (64, 36));
        assert_eq!(poster(&fixture("make_fixtures.swift")).err().as_deref(), Some("unreadable"));
    }
}
