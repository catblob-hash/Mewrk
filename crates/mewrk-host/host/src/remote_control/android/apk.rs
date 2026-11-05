//! The package name an APK declares, read from its `AndroidManifest.xml` before anything is
//! sent: an install that must not replace an installed app asks the device about that package
//! first.
//!
//! An APK is a ZIP archive, and its manifest is Android's binary XML. Only what finding one
//! attribute takes is read: the archive's central directory, the manifest's entry (stored or
//! deflated), the string pool and the first element's attributes. Everything is bounded, and
//! anything unexpected answers `None` — the caller then lets pm decide.

use std::io::{self, Read, Seek, SeekFrom};

use flate2::read::DeflateDecoder;

const MANIFEST: &[u8] = b"AndroidManifest.xml";
/// The end-of-central-directory record is the last thing in the file, after a comment of at most
/// 64 KiB.
const EOCD_SEARCH: u64 = 22 + 0xffff;
const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ENTRY_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
/// A central directory larger than this (hundreds of thousands of files) is not read.
const DIRECTORY_MAX: u32 = 16 * 1024 * 1024;
/// A manifest larger than this, packed or unpacked, is not read.
const MANIFEST_MAX: u32 = 4 * 1024 * 1024;

const RES_XML_TYPE: u16 = 0x0003;
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const UTF8_FLAG: u32 = 1 << 8;
const NO_ENTRY: u32 = 0xffff_ffff;
const TYPE_STRING: u8 = 0x03;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([pair[0], pair[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]))
}

fn read_exactly(file: &mut (impl Read + Seek), at: u64, length: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The `package` attribute of the APK's manifest, `None` when it cannot be read. `size` is the
/// file's length. The file's position is left wherever the reading ended.
pub(super) fn package_name(file: &mut (impl Read + Seek), size: u64) -> Option<String> {
    let manifest = manifest_bytes(file, size).ok()??;
    manifest_package(&manifest)
}

/// The manifest's bytes, unpacked.
fn manifest_bytes(file: &mut (impl Read + Seek), size: u64) -> io::Result<Option<Vec<u8>>> {
    let tail = size.min(EOCD_SEARCH);
    let end = read_exactly(file, size - tail, tail as usize)?;
    let Some(eocd) = (0..end.len().saturating_sub(21))
        .rev()
        .find(|&at| u32_at(&end, at) == Some(EOCD_SIGNATURE))
    else {
        return Ok(None);
    };
    let (Some(directory_size), Some(directory_at)) =
        (u32_at(&end, eocd + 12), u32_at(&end, eocd + 16))
    else {
        return Ok(None);
    };
    if directory_size > DIRECTORY_MAX || u64::from(directory_at) + u64::from(directory_size) > size
    {
        return Ok(None);
    }
    let directory = read_exactly(file, u64::from(directory_at), directory_size as usize)?;
    let mut at = 0usize;
    let entry = loop {
        if u32_at(&directory, at) != Some(ENTRY_SIGNATURE) {
            return Ok(None);
        }
        let field = |offset: usize| u16_at(&directory, at + offset).map(usize::from);
        let (Some(name_length), Some(extra_length), Some(comment_length)) =
            (field(28), field(30), field(32))
        else {
            return Ok(None);
        };
        let name = directory.get(at + 46..at + 46 + name_length);
        if name == Some(MANIFEST) {
            break at;
        }
        at += 46 + name_length + extra_length + comment_length;
    };
    let (Some(method), Some(packed), Some(unpacked), Some(local_at)) = (
        u16_at(&directory, entry + 10),
        u32_at(&directory, entry + 20),
        u32_at(&directory, entry + 24),
        u32_at(&directory, entry + 42),
    ) else {
        return Ok(None);
    };
    if packed > MANIFEST_MAX || unpacked > MANIFEST_MAX {
        return Ok(None);
    }
    let local = read_exactly(file, u64::from(local_at), 30)?;
    let (Some(LOCAL_SIGNATURE), Some(name_length), Some(extra_length)) =
        (u32_at(&local, 0), u16_at(&local, 26), u16_at(&local, 28))
    else {
        return Ok(None);
    };
    let data_at = u64::from(local_at) + 30 + u64::from(name_length) + u64::from(extra_length);
    if data_at + u64::from(packed) > size {
        return Ok(None);
    }
    let data = read_exactly(file, data_at, packed as usize)?;
    match method {
        0 => Ok(Some(data)),
        8 => {
            let mut manifest = Vec::with_capacity(unpacked as usize);
            DeflateDecoder::new(data.as_slice())
                .take(u64::from(MANIFEST_MAX))
                .read_to_end(&mut manifest)?;
            Ok(Some(manifest))
        }
        _ => Ok(None),
    }
}

/// The first element's `package` attribute in Android's binary XML: a `RES_XML_TYPE` chunk
/// holding a string pool and then the elements, each attribute naming its name and raw value by
/// their index in the pool.
fn manifest_package(xml: &[u8]) -> Option<String> {
    if u16_at(xml, 0)? != RES_XML_TYPE {
        return None;
    }
    let end = (u32_at(xml, 4)? as usize).min(xml.len());
    let mut at = usize::from(u16_at(xml, 2)?);
    let mut strings: Option<StringPool<'_>> = None;
    while at + 8 <= end {
        let kind = u16_at(xml, at)?;
        let header = usize::from(u16_at(xml, at + 2)?);
        let size = u32_at(xml, at + 4)? as usize;
        if size < 8 || at.checked_add(size)? > end {
            return None;
        }
        let chunk = &xml[at..at + size];
        match kind {
            RES_STRING_POOL_TYPE => strings = Some(StringPool::new(chunk, header)?),
            RES_XML_START_ELEMENT_TYPE => {
                let strings = strings.as_ref()?;
                // The extension follows the 16-byte node header: namespace, name, then where the
                // attributes start, their size and their count.
                let ext = header;
                if strings.get(u32_at(chunk, ext + 4)?)? != "manifest" {
                    return None;
                }
                let start = usize::from(u16_at(chunk, ext + 8)?);
                let step = usize::from(u16_at(chunk, ext + 10)?);
                let count = usize::from(u16_at(chunk, ext + 12)?);
                for index in 0..count {
                    let attribute = ext + start + index * step;
                    if u32_at(chunk, attribute)? != NO_ENTRY
                        || strings.get(u32_at(chunk, attribute + 4)?)? != "package"
                    {
                        continue;
                    }
                    let raw = u32_at(chunk, attribute + 8)?;
                    let value = if raw != NO_ENTRY {
                        raw
                    } else if *chunk.get(attribute + 15)? == TYPE_STRING {
                        u32_at(chunk, attribute + 16)?
                    } else {
                        return None;
                    };
                    return strings.get(value);
                }
                return None;
            }
            _ => {}
        }
        at += size;
    }
    None
}

/// A `ResStringPool` chunk: its strings by index, UTF-8 or UTF-16.
struct StringPool<'a> {
    chunk: &'a [u8],
    count: u32,
    offsets_at: usize,
    strings_at: usize,
    utf8: bool,
}

impl<'a> StringPool<'a> {
    fn new(chunk: &'a [u8], header: usize) -> Option<Self> {
        Some(Self {
            chunk,
            count: u32_at(chunk, 8)?,
            offsets_at: header,
            strings_at: u32_at(chunk, 20)? as usize,
            utf8: u32_at(chunk, 16)? & UTF8_FLAG != 0,
        })
    }

    fn get(&self, index: u32) -> Option<String> {
        if index >= self.count {
            return None;
        }
        let offset = u32_at(self.chunk, self.offsets_at + index as usize * 4)? as usize;
        let at = self.strings_at.checked_add(offset)?;
        if self.utf8 {
            // The length in UTF-16 units, then in bytes; each one or two bytes.
            let length = |at: usize| -> Option<(usize, usize)> {
                let first = usize::from(*self.chunk.get(at)?);
                if first & 0x80 == 0 {
                    Some((first, 1))
                } else {
                    Some((
                        ((first & 0x7f) << 8) | usize::from(*self.chunk.get(at + 1)?),
                        2,
                    ))
                }
            };
            let (_, skip) = length(at)?;
            let (bytes, more) = length(at + skip)?;
            let start = at + skip + more;
            let text = self.chunk.get(start..start.checked_add(bytes)?)?;
            String::from_utf8(text.to_vec()).ok()
        } else {
            let first = usize::from(u16_at(self.chunk, at)?);
            let (units, start) = if first & 0x8000 == 0 {
                (first, at + 2)
            } else {
                (
                    ((first & 0x7fff) << 16) | usize::from(u16_at(self.chunk, at + 2)?),
                    at + 4,
                )
            };
            let text: Option<Vec<u16>> = (0..units)
                .map(|unit| u16_at(self.chunk, start + unit * 2))
                .collect();
            String::from_utf16(&text?).ok()
        }
    }
}
