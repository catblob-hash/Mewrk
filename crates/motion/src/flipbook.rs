//! A run of same-sized RGBA frames in one file, each with the time it ends at.
//!
//! Showing a frame is mapping it and handing the bytes to the GPU, with nothing to decode, so a long loop costs disk and no memory the
//! system can't take back. A book may be read while it is being written: the writer publishes each frame once it is whole, and a reader
//! never looks past that. That is what lets a video's glass be used as far as it has been baked.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use memmap2::{Advice, MmapOptions, MmapRaw};

const MAGIC: &[u8; 8] = b"MWRKFLIP";
const VERSION: u32 = 1;
const HEADER: usize = 64;
/// Frames start on a page, so a frame's first page holds nothing else.
const PAGE: usize = 16_384;

#[derive(Clone, Copy)]
struct Layout {
    width: u32,
    height: u32,
    capacity: u32,
}

impl Layout {
    fn frame_bytes(self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    fn frames_at(self) -> usize {
        (HEADER + self.capacity as usize * 8).next_multiple_of(PAGE)
    }

    fn file_bytes(self, frames: u32) -> usize {
        self.frames_at() + frames as usize * self.frame_bytes()
    }
}

/// A frame's bytes, for as long as anyone holds this: what `fluent::write_image` is given.
pub struct FrameBytes {
    map: Arc<MmapRaw>,
    at: usize,
    len: usize,
}

impl AsRef<[u8]> for FrameBytes {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: only `Flipbook::frame` makes these, for a frame already published, which nothing writes again. The range lies inside
        // the map the `Arc` keeps alive, and truncating the file only ever takes what lies past the last published frame.
        unsafe { std::slice::from_raw_parts(self.map.as_ptr().add(self.at), self.len) }
    }
}

pub struct Flipbook {
    map: Arc<MmapRaw>,
    layout: Layout,
    published: Arc<AtomicU32>,
    complete: Arc<AtomicBool>,
    /// Every pixel of every frame is solid. Not known of a book until it is finished.
    opaque: Arc<AtomicBool>,
}

impl Flipbook {
    /// A finished book. Anything that isn't one, an old format or a file cut short, is an error, and whoever asked bakes it again.
    pub fn open(path: &Path) -> Result<Arc<Self>, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let length = file.metadata().map_err(|e| e.to_string())?.len() as usize;
        if length < HEADER {
            return Err("a frame file cut short".to_owned());
        }
        let map = MmapOptions::new().map_raw_read_only(&file).map_err(|e| e.to_string())?;
        // SAFETY: the map is at least `HEADER` long, checked above.
        let header = unsafe { std::slice::from_raw_parts(map.as_ptr(), HEADER) };
        let word = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().expect("four bytes"));
        if &header[..8] != MAGIC || word(8) != VERSION || word(24) != 1 {
            return Err("not a finished frame file".to_owned());
        }
        let layout = Layout { width: word(12), height: word(16), capacity: word(20) };
        let count = word(28);
        if layout.width == 0 || layout.height == 0 || count == 0 || count > layout.capacity || length < layout.file_bytes(count) {
            return Err("a frame file cut short".to_owned());
        }
        Ok(Arc::new(Self { map: Arc::new(map), layout, published: Arc::new(AtomicU32::new(count)), complete: Arc::new(AtomicBool::new(true)), opaque: Arc::new(AtomicBool::new(word(32) == 1)) }))
    }

    /// Whether every pixel of every frame is known to be solid.
    pub fn is_opaque(&self) -> bool {
        self.opaque.load(Ordering::Acquire)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.layout.width, self.layout.height)
    }

    /// How many frames can be read now.
    pub fn count(&self) -> u32 {
        self.published.load(Ordering::Acquire)
    }

    /// Whether every frame is there. Past this it loops; before it, the time past the last frame has nothing to show.
    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    fn end_us_unchecked(&self, index: u32) -> u64 {
        // SAFETY: the table lies inside the map, and an entry below the published count was written before that count was.
        unsafe { (self.map.as_ptr().add(HEADER + index as usize * 8) as *const u64).read_unaligned() }
    }

    /// When a frame stops showing, counted from the start of the first.
    pub fn end_us(&self, index: u32) -> Option<u64> {
        (index < self.count()).then(|| self.end_us_unchecked(index))
    }

    /// The length of the whole loop, or of as much as there is so far.
    pub fn duration_us(&self) -> u64 {
        match self.count() {
            0 => 0,
            count => self.end_us_unchecked(count - 1),
        }
    }

    /// The frame showing at a time, `None` past the last one so far. A finished book's time wraps round.
    pub fn index_at(&self, time_us: u64) -> Option<u32> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let total = self.end_us_unchecked(count - 1);
        let time = if self.is_complete() && total > 0 { time_us % total } else { time_us };
        if time >= total {
            return if self.is_complete() { Some(count - 1) } else { None };
        }
        // The first frame whose end lies after this moment.
        let (mut low, mut high) = (0, count - 1);
        while low < high {
            let middle = (low + high) / 2;
            if self.end_us_unchecked(middle) > time {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        Some(low)
    }

    /// A published frame's bytes.
    pub fn frame(&self, index: u32) -> Option<FrameBytes> {
        (index < self.count()).then(|| FrameBytes { map: self.map.clone(), at: self.layout.file_bytes(index), len: self.layout.frame_bytes() })
    }

    /// Asks the system to read a frame in ahead of time, so that uploading it doesn't wait on the disk.
    pub fn prefetch(&self, index: u32) {
        if index < self.count() {
            let _ = self.map.advise_range(Advice::WillNeed, self.layout.file_bytes(index), self.layout.frame_bytes());
        }
    }
}

/// Writes a book under a temporary name, and gives it its real one when it is done. Dropped before that, it leaves nothing behind.
pub struct FlipWriter {
    file: File,
    map: Arc<MmapRaw>,
    layout: Layout,
    written: u32,
    published: Arc<AtomicU32>,
    complete: Arc<AtomicBool>,
    opaque: Arc<AtomicBool>,
    all_solid: bool,
    temporary: PathBuf,
    destination: PathBuf,
    finished: bool,
}

impl FlipWriter {
    /// Room for `capacity` frames is set aside at once, so readers can be handed a map that will never have to move.
    pub fn create(destination: &Path, width: u32, height: u32, capacity: u32) -> Result<Self, String> {
        if width == 0 || height == 0 || capacity == 0 {
            return Err("an empty frame file".to_owned());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let name = destination.file_name().and_then(|name| name.to_str()).unwrap_or("frames");
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let temporary = destination.with_file_name(format!(".{name}.{}-{}.partial", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let layout = Layout { width, height, capacity };
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&temporary).map_err(|e| e.to_string())?;
        let unmade = |reason: std::io::Error| {
            let _ = fs::remove_file(&temporary);
            reason.to_string()
        };
        // Sparse until written, so a capacity that turns out generous costs nothing.
        file.set_len(layout.file_bytes(capacity) as u64).map_err(unmade)?;
        let map = MmapRaw::map_raw(&file).map_err(unmade)?;
        // SAFETY: the map is the whole file, far longer than the header, and only this writer holds it yet.
        unsafe {
            let header = map.as_mut_ptr();
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), header, 8);
            for (at, value) in [(8, VERSION), (12, width), (16, height), (20, capacity)] {
                std::ptr::copy_nonoverlapping(value.to_le_bytes().as_ptr(), header.add(at), 4);
            }
        }
        Ok(Self { file, map: Arc::new(map), layout, written: 0, published: Arc::new(AtomicU32::new(0)), complete: Arc::new(AtomicBool::new(false)), opaque: Arc::new(AtomicBool::new(false)), all_solid: true, temporary, destination: destination.to_owned(), finished: false })
    }

    /// The book as far as it has been written, and as it goes on.
    pub fn book(&self) -> Arc<Flipbook> {
        Arc::new(Flipbook { map: self.map.clone(), layout: self.layout, published: self.published.clone(), complete: self.complete.clone(), opaque: self.opaque.clone() })
    }

    pub fn frame_bytes(&self) -> usize {
        self.layout.frame_bytes()
    }

    /// Adds a frame that shows until `end_us`, and publishes it. False once the room set aside is used up.
    pub fn push(&mut self, rgba: &[u8], end_us: u64) -> bool {
        if self.written >= self.layout.capacity || rgba.len() != self.layout.frame_bytes() {
            return false;
        }
        self.all_solid = self.all_solid && rgba.chunks_exact(4).all(|pixel| pixel[3] == 255);
        // SAFETY: both ranges lie inside the map, as `create` made it for `capacity` frames, and no reader looks at this frame or this
        // table entry until the count below says it may.
        unsafe {
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), self.map.as_mut_ptr().add(self.layout.file_bytes(self.written)), rgba.len());
            (self.map.as_mut_ptr().add(HEADER + self.written as usize * 8) as *mut u64).write_unaligned(end_us);
        }
        self.written += 1;
        self.published.store(self.written, Ordering::Release);
        true
    }

    /// Whatever was written is the book: it gets its name, and what was set aside for frames that never came goes back.
    pub fn finish(mut self) -> Result<Arc<Flipbook>, String> {
        if self.written == 0 {
            return Err("no frames".to_owned());
        }
        // SAFETY: the header lies inside the map. Nothing else writes these, and a reader of a book still being written doesn't read them.
        unsafe {
            let header = self.map.as_mut_ptr();
            std::ptr::copy_nonoverlapping(self.written.to_le_bytes().as_ptr(), header.add(28), 4);
            std::ptr::copy_nonoverlapping(u32::from(self.all_solid).to_le_bytes().as_ptr(), header.add(32), 4);
            std::ptr::copy_nonoverlapping(1u32.to_le_bytes().as_ptr(), header.add(24), 4);
        }
        self.file.set_len(self.layout.file_bytes(self.written) as u64).map_err(|e| e.to_string())?;
        fs::rename(&self.temporary, &self.destination).map_err(|e| e.to_string())?;
        self.finished = true;
        self.opaque.store(self.all_solid, Ordering::Release);
        self.complete.store(true, Ordering::Release);
        Ok(self.book())
    }
}

impl Drop for FlipWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

/// Removes the oldest files of a folder of books until it holds no more than `budget` bytes. `keep` is in use, whatever its age. A book
/// another run left half written goes first, and one this run is writing stays.
pub fn prune(folder: &Path, budget: u64, keep: &Path) {
    let Ok(entries) = fs::read_dir(folder) else { return };
    let own_partial = format!(".{}-", std::process::id());
    let mut books: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.ends_with(".partial") {
                if !name.contains(&own_partial) {
                    let _ = fs::remove_file(entry.path());
                }
                return None;
            }
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = books.iter().map(|book| book.1).sum();
    books.sort_by_key(|book| book.0);
    for (_, length, path) in books {
        if total <= budget {
            break;
        }
        if path != keep && fs::remove_file(&path).is_ok() {
            total -= length;
        }
    }
}

/// Marks a book as just used, for [`prune`].
pub fn touch(path: &Path) {
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("mewrk-flip-{name}-{}-{:x}", std::process::id(), fastrand::u64(..)));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn frame(value: u8) -> Vec<u8> {
        vec![value; 2 * 2 * 4]
    }

    #[test]
    fn a_book_can_be_read_while_it_is_written_and_only_as_far_as_it_is_written() {
        let dir = folder("progress");
        let path = dir.join("book.flip");
        let mut writer = FlipWriter::create(&path, 2, 2, 8).unwrap();
        let book = writer.book();
        assert_eq!(book.index_at(0), None);
        assert!(writer.push(&frame(1), 100));
        assert_eq!(book.index_at(0), Some(0));
        assert_eq!(book.index_at(99), Some(0));
        assert_eq!(book.index_at(100), None, "nothing is there yet");
        assert!(writer.push(&frame(2), 250));
        assert_eq!(book.index_at(100), Some(1));
        assert_eq!(book.frame(1).unwrap().as_ref(), &frame(2)[..]);
        assert!(book.frame(2).is_none());
        assert!(!path.exists(), "it has no name until it is done");
    }

    #[test]
    fn a_finished_book_loops_opens_again_and_gives_back_what_it_set_aside() {
        let dir = folder("finish");
        let path = dir.join("book.flip");
        let mut writer = FlipWriter::create(&path, 2, 2, 100).unwrap();
        writer.push(&frame(1), 100);
        writer.push(&frame(2), 200);
        let live = writer.book();
        writer.finish().unwrap();
        assert_eq!(live.index_at(250), Some(0), "the reader from before it finished loops as well");
        assert_eq!(live.frame(1).unwrap().as_ref(), &frame(2)[..], "and its map still reads");
        let book = Flipbook::open(&path).unwrap();
        assert!(book.is_complete());
        assert_eq!(book.count(), 2);
        assert_eq!(book.duration_us(), 200);
        assert_eq!(book.index_at(350), Some(1));
        assert_eq!(fs::metadata(&path).unwrap().len() as usize, (HEADER + 100 * 8).next_multiple_of(PAGE) + 2 * frame(0).len());
    }

    #[test]
    fn a_book_knows_once_finished_whether_all_of_it_is_solid() {
        let dir = folder("opaque");
        let mut solid = FlipWriter::create(&dir.join("solid.flip"), 2, 2, 2).unwrap();
        solid.push(&frame(255), 100);
        assert!(!solid.book().is_opaque(), "more frames may be coming");
        let live = solid.book();
        solid.push(&frame(255), 200);
        solid.finish().unwrap();
        assert!(live.is_opaque());
        assert!(Flipbook::open(&dir.join("solid.flip")).unwrap().is_opaque());

        let mut holed = FlipWriter::create(&dir.join("holed.flip"), 2, 2, 2).unwrap();
        holed.push(&frame(255), 100);
        holed.push(&frame(0), 200);
        holed.finish().unwrap();
        assert!(!Flipbook::open(&dir.join("holed.flip")).unwrap().is_opaque(), "one clear pixel in the last frame is enough");
    }

    #[test]
    fn nothing_is_left_of_a_book_given_up_on_and_a_ruined_one_does_not_open() {
        let dir = folder("abandon");
        let path = dir.join("book.flip");
        let mut writer = FlipWriter::create(&path, 2, 2, 4).unwrap();
        writer.push(&frame(1), 100);
        drop(writer);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::write(&path, b"MWRKFLIP not really").unwrap();
        assert!(Flipbook::open(&path).is_err());
        fs::write(&path, vec![0u8; 100]).unwrap();
        assert!(Flipbook::open(&path).is_err());

        let mut writer = FlipWriter::create(&path.with_extension("two"), 2, 2, 4).unwrap();
        writer.push(&frame(1), 100);
        writer.finish().unwrap();
        let whole = fs::read(path.with_extension("two")).unwrap();
        fs::write(&path, &whole[..whole.len() - 1]).unwrap();
        assert!(Flipbook::open(&path).is_err(), "cut short by a byte");
    }

    #[test]
    fn the_folder_stays_inside_its_budget_oldest_first_and_never_takes_what_is_in_use() {
        let dir = folder("prune");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(1000);
        for (index, name) in ["a", "b", "c"].iter().enumerate() {
            let path = dir.join(name);
            fs::write(&path, vec![0u8; 100]).unwrap();
            File::options().write(true).open(&path).unwrap().set_modified(old + std::time::Duration::from_secs(index as u64)).unwrap();
        }
        prune(&dir, 150, &dir.join("a"));
        assert!(dir.join("a").exists(), "the oldest, but in use");
        assert!(!dir.join("b").exists());
        assert!(!dir.join("c").exists());
    }

    #[test]
    fn a_half_written_book_goes_if_another_run_left_it_and_stays_if_this_one_is_writing_it() {
        let dir = folder("partials");
        let mut writer = FlipWriter::create(&dir.join("book.flip"), 2, 2, 2).unwrap();
        writer.push(&frame(1), 100);
        fs::write(dir.join(".old.flip.1-0.partial"), b"from a run long gone").unwrap();
        prune(&dir, u64::MAX, &dir.join("book.flip"));
        assert!(!dir.join(".old.flip.1-0.partial").exists());
        writer.finish().unwrap();
        assert!(dir.join("book.flip").exists(), "the name it was writing under was still there to rename");
    }
}
