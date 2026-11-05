//! The RFB (VNC) driver: a desktop or a virtual machine whose server pushes
//! its picture, driven with a pointer and keysyms.
//!
//! One connection, two directions. A reader thread owns the reading half: it
//! decodes every FramebufferUpdate into the framebuffer, tells the session a
//! frame is ready, and asks for the next incremental update — so the picture
//! follows the screen on its own, the way every VNC viewer's does. Everything
//! the client sends (those requests, pointer and key events) goes through one
//! locked writer, because the model's input and the viewer's can arrive at the
//! same time.
//!
//! RFB never ties an update to a request or an input, so "the screen after my
//! click" has to be waited for: [`Driver::frame`] with `fresh` asks for an
//! update, waits for one that began after the call to complete, and then for
//! the screen to stay quiet for a moment (`docs/remote-control.md` §3.2).
//!
//! The client asks for one pixel format and a short list of encodings that
//! every server of note speaks (ZRLE, Hextile, CopyRect, Raw), plus the
//! pseudo-encodings that report a resized screen and the Extended Clipboard.
//! It never asks for the cursor as a separate image, so servers draw the
//! pointer into the picture the model sees.
//!
//! The clipboard ([`DesktopControl`], behind the `desktop_*` tools) is the
//! server's Extended Clipboard when it offers one — UTF-8 text, asked for and
//! answered — and otherwise the classic cut text: Latin-1 only, and only what
//! the server sends when its clipboard changes ([`clipboard`]). Neither ever
//! reaches the log.

mod clipboard;
mod decode;
mod handshake;
#[cfg(test)]
mod tests;

use std::{
    io::{BufReader, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

use flate2::Decompress;

use self::decode::{Area, Framebuffer, WireError, WireResult};
use super::{
    driver::{
        ClipboardChannel, ClipboardText, DesktopControl, Driver, DriverError, DriverEvent,
        DriverEvents, DriverInfo, DriverResult, Frame, Platform, ScreenUpdate,
    },
    keys::{is_latin1_keysym, keysym, misread_in_sixteen_bits, Key, NamedKey},
    transport::{ConnectionHandle, Connector},
};

/// What the session knows to sign in with.
#[derive(Clone, Default)]
pub struct VncAuth {
    /// The account for Apple Remote Desktop authentication, when the entry or
    /// the person at the pane named one.
    pub username: Option<String>,
    pub password: Option<zeroize::Zeroizing<String>>,
}

impl std::fmt::Debug for VncAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VncAuth")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "…"))
            .finish()
    }
}

/// How long reaching the port may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the whole handshake may take, sign-in included. A server that is
/// slower than this is not a VNC server, or not a working one.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the screen must stay without updates before a fresh frame counts
/// as settled.
const SETTLE: Duration = Duration::from_millis(200);
/// The longest a fresh frame waits for the screen to settle.
const FRESH_LIMIT: Duration = Duration::from_millis(1500);
/// The largest clipboard message a server may send, classic or extended; a
/// larger one ends the connection, as a server that sends one is broken.
const MAX_CUT_TEXT: u32 = 16 * 1024 * 1024;
/// The most of a server's clipboard text kept, in bytes; the rest is read past.
const MAX_KEPT_CLIPBOARD: usize = 1024 * 1024;
/// The most clipboard text this client sends, in bytes as it goes: Latin-1
/// with LF line ends, or UTF-8 with CRLF line ends and the NUL. TigerVNC drops
/// a cut text, an Extended Clipboard message or a text in it above its
/// MaxCutText, 256 KiB by default, with nothing but a line in its log; QEMU and
/// libvncserver close the connection above 1 MiB. A KiB below 256 KiB leaves
/// room for the zlib stream around text that does not compress. The desktop
/// tools' longest text, 60,000 characters, is at most 240,001 bytes.
pub(crate) const MAX_SENT_CLIPBOARD: usize = 256 * 1024 - 1024;
/// How long, right after connecting, a clipboard call waits for the first
/// update: a server announces its Extended Clipboard as it reads the client's
/// encodings, before it answers the update request sent with them, so once an
/// update has come, a server that said nothing has none (research-rfb §7.1).
const CAPS_WAIT: Duration = Duration::from_secs(1);
/// Key events sent per write when typing text.
const TEXT_EVENTS_PER_WRITE: usize = 512;

mod encoding {
    pub const RAW: i32 = 0;
    pub const COPY_RECT: i32 = 1;
    pub const HEXTILE: i32 = 5;
    pub const ZRLE: i32 = 16;
    pub const DESKTOP_SIZE: i32 = -223;
    pub const LAST_RECT: i32 = -224;
    pub const EXTENDED_DESKTOP_SIZE: i32 = -308;
    /// 0xC0A1E5CE: the server answers it with its clipboard caps, and from then
    /// on cut text may be extended (`clipboard`).
    pub const EXTENDED_CLIPBOARD: i32 = 0xC0A1_E5CEu32 as i32;
}

/// The encodings asked for, most preferred first. No Cursor (-239): with it
/// the server would leave the pointer out of the picture.
const ENCODINGS: [i32; 8] = [
    encoding::ZRLE,
    encoding::HEXTILE,
    encoding::COPY_RECT,
    encoding::RAW,
    encoding::DESKTOP_SIZE,
    encoding::EXTENDED_DESKTOP_SIZE,
    encoding::LAST_RECT,
    encoding::EXTENDED_CLIPBOARD,
];

/// The pixel format asked for: 32 bits per pixel, depth 24, little-endian,
/// true colour, 8 bits a channel with red in the lowest byte — so a pixel on
/// the wire is R, G, B, X, and ZRLE's compressed pixel is R, G, B.
const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

/// The keysym of the left Shift key.
const SHIFT: u32 = 0xffe1;
/// Meta_L, which Apple's server takes for Option (see [`VncDriver::named_keysym`]).
const META_L: u32 = 0xffe7;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connects to the RFB server on `port` of the connector's place and signs in.
pub fn connect(
    connector: Arc<dyn Connector>,
    port: u16,
    auth: VncAuth,
    events: DriverEvents,
) -> DriverResult<Arc<dyn Driver>> {
    connect_within(connector.as_ref(), port, &auth, events, HANDSHAKE_TIMEOUT)
}

fn connect_within(
    connector: &dyn Connector,
    port: u16,
    auth: &VncAuth,
    events: DriverEvents,
    handshake_timeout: Duration,
) -> DriverResult<Arc<dyn Driver>> {
    let mut connection = connector
        .connect(port, CONNECT_TIMEOUT)
        .map_err(DriverError::Disconnected)?;
    if connector.cancelled() {
        connection.close();
        return Err(DriverError::Failed(
            "the session was stopped while it was connecting".to_owned(),
        ));
    }
    connection.handle().set_deadline(Some(handshake_timeout));
    let log = |line: String| events(DriverEvent::Log(line));
    let init = match handshake::run(&mut connection, auth, port, handshake_timeout, &log) {
        Ok(init) => init,
        Err(error) => {
            connection.close();
            return Err(error);
        }
    };
    connection.handle().set_deadline(None);
    let (reader, writer, handle) = connection.split();
    let shared = Arc::new(Shared {
        screen: Mutex::new(Screen {
            framebuffer: Framebuffer::new(init.width, init.height),
            snapshot: None,
            dirty: None,
            dirty_whole: false,
            seq: 0,
            started: 0,
            completed: 0,
            in_update: false,
            last_update: None,
            gone: None,
        }),
        changed: Condvar::new(),
        clipboard: Mutex::new(Clipboard::default()),
        clipboard_changed: Condvar::new(),
        writer: Mutex::new(writer),
        handle,
        events,
        // QEMU's ServerInit name is always "QEMU" or "QEMU (<vm name>)".
        sixteen_bit_keysyms: init.name.starts_with("QEMU"),
        apple: init.apple,
        name: if init.name.is_empty() {
            "VNC desktop".to_owned()
        } else {
            init.name
        },
        unicode_noted: AtomicBool::new(false),
    });

    let mut setup = Vec::with_capacity(20 + 4 + 4 * ENCODINGS.len() + 10);
    setup.extend_from_slice(&[0, 0, 0, 0]);
    setup.extend_from_slice(&PIXEL_FORMAT);
    setup.extend_from_slice(&[2, 0]);
    setup.extend_from_slice(&(ENCODINGS.len() as u16).to_be_bytes());
    for encoding in ENCODINGS {
        setup.extend_from_slice(&encoding.to_be_bytes());
    }
    setup.extend_from_slice(&update_request(false, init.width, init.height));
    if let Err(error) = shared.send(&setup) {
        shared.end(error.message().to_owned());
        return Err(error);
    }

    let reading = shared.clone();
    if let Err(error) = thread::Builder::new()
        .name("remote-control-vnc".into())
        .spawn(move || read_loop(reading, reader))
    {
        shared.end("the VNC connection was closed".to_owned());
        return Err(DriverError::Failed(format!(
            "could not start reading from the VNC server: {error}"
        )));
    }
    Ok(Arc::new(VncDriver { shared }))
}

/// A FramebufferUpdateRequest for the whole screen.
fn update_request(incremental: bool, width: u32, height: u32) -> [u8; 10] {
    area_request(incremental, 0, 0, width, height)
}

fn area_request(incremental: bool, x: u32, y: u32, width: u32, height: u32) -> [u8; 10] {
    let mut message = [0u8; 10];
    message[0] = 3;
    message[1] = u8::from(incremental);
    // Every size here passed `check_size`, so it fits sixteen bits.
    for (at, value) in [(2, x), (4, y), (6, width), (8, height)] {
        message[at..at + 2].copy_from_slice(&(value.min(u32::from(u16::MAX)) as u16).to_be_bytes());
    }
    message
}

/// What the reader thread and the callers share.
struct Shared {
    screen: Mutex<Screen>,
    /// Signalled when an update completes and when the connection ends.
    changed: Condvar,
    /// Never locked while `screen` is held by the same thread.
    clipboard: Mutex<Clipboard>,
    /// Signalled when the server's clipboard text arrives, when it says its
    /// clipboard holds none, and when the connection ends.
    clipboard_changed: Condvar,
    writer: Mutex<Box<dyn Write + Send>>,
    handle: ConnectionHandle,
    events: DriverEvents,
    name: String,
    /// Whether the server looks keysyms up in sixteen bits, as QEMU does: it
    /// turns them into scancodes of its keyboard layout, so it types nothing
    /// beyond Latin-1, and reads some Unicode keysyms as BackSpace, Return or
    /// Caps_Lock ([`misread_in_sixteen_bits`]). Nothing beyond Latin-1 is sent
    /// to it.
    sixteen_bit_keysyms: bool,
    /// Whether this is Apple's server, macOS Screen Sharing: it offered Apple
    /// Remote Desktop authentication (type 30) or announced RFB 003.889.
    apple: bool,
    /// Whether the log already says that some typed text went out as Unicode
    /// keysyms, which servers take on trust.
    unicode_noted: AtomicBool,
}

/// What this connection knows of the clipboard (`clipboard`). Its texts never
/// reach the log: a clipboard can hold a password.
#[derive(Default)]
struct Clipboard {
    /// The server's Extended Clipboard caps (its flags), once it sent them.
    caps: Option<u32>,
    /// The last text the server sent, classic or extended, and when; `None`
    /// before any, and once it said its clipboard holds no text.
    received: Option<ClipboardText>,
    /// Texts the server provided, so a read waits for one after its request.
    provides: u64,
    /// Times the server said its clipboard holds no text.
    emptied: u64,
    /// Reads waiting for the server's text: a change it announces meanwhile is
    /// asked for again.
    waiting: usize,
    /// The text this client last put on the clipboard, which answers the
    /// server's requests for it.
    offered: Option<String>,
    /// That text and when it went, while it is what the clipboard holds: until
    /// the server says its clipboard changed. A server that holds a client's
    /// text answers no request for it (TigerVNC, QEMU), so a read gets it here.
    ours: Option<ClipboardText>,
}

impl Clipboard {
    fn extended(&self) -> bool {
        self.caps.is_some_and(|caps| caps & clipboard::TEXT != 0)
    }

    /// Whether the server's caps take `action`.
    fn takes(&self, action: u32) -> bool {
        self.caps.is_some_and(|caps| caps & action != 0)
    }
}

struct Screen {
    framebuffer: Framebuffer,
    /// The framebuffer as a shareable buffer, made on the first frame asked for
    /// after a change and handed out until the next one.
    snapshot: Option<Arc<[u8]>>,
    /// What changed since a viewer in this process last took it, as one rectangle.
    dirty: Option<Area>,
    /// The size changed, so nothing a viewer holds can be patched.
    dirty_whole: bool,
    seq: u64,
    /// FramebufferUpdates begun and finished. Updates come one after another,
    /// so `completed > n` means an update that began after the `n`th is done.
    started: u64,
    completed: u64,
    in_update: bool,
    last_update: Option<Instant>,
    /// Why the connection ended, once it has.
    gone: Option<String>,
}

impl Screen {
    fn mark_dirty(&mut self, area: Area) {
        if area.is_empty() {
            return;
        }
        self.dirty = Some(match self.dirty {
            None => area,
            Some(so_far) => {
                let (x, y) = (so_far.x.min(area.x), so_far.y.min(area.y));
                let right = (so_far.x + so_far.width).max(area.x + area.width);
                let bottom = (so_far.y + so_far.height).max(area.y + area.height);
                Area { x, y, width: right - x, height: bottom - y }
            }
        });
    }

    fn whole(&mut self) -> Option<ScreenUpdate> {
        let (width, height) = (self.framebuffer.width(), self.framebuffer.height());
        if width == 0 || height == 0 {
            return None;
        }
        self.dirty = None;
        self.dirty_whole = false;
        let rgba = match &self.snapshot {
            Some(rgba) => rgba.clone(),
            None => {
                let rgba: Arc<[u8]> = Arc::from(self.framebuffer.rgba());
                self.snapshot = Some(rgba.clone());
                rgba
            }
        };
        Some(ScreenUpdate::Full { width, height, rgba })
    }
}

impl Shared {
    /// Writes one or more client messages. Writes are bounded without a
    /// deadline: a socket gives up after its write timeout, and a relay's
    /// writer only queues.
    fn send(&self, bytes: &[u8]) -> DriverResult<()> {
        if let Some(reason) = self.gone() {
            return Err(DriverError::Disconnected(reason));
        }
        let mut writer = lock(&self.writer);
        let written = writer.write_all(bytes).and_then(|()| writer.flush());
        drop(writer);
        written.map_err(|error| {
            DriverError::Disconnected(
                self.end(format!("could not send to the VNC server: {error}")),
            )
        })
    }

    fn gone(&self) -> Option<String> {
        lock(&self.screen).gone.clone()
    }

    /// Ends the connection for `reason`, unless it already ended, and answers
    /// the reason it ended for.
    fn end(&self, reason: String) -> String {
        let recorded = lock(&self.screen).gone.get_or_insert(reason).clone();
        self.changed.notify_all();
        // Taken and let go first, so a read that just found the connection
        // alive is already waiting when this wakes it.
        drop(lock(&self.clipboard));
        self.clipboard_changed.notify_all();
        self.handle.close();
        recorded
    }

    fn size(&self) -> DriverResult<(u32, u32)> {
        let screen = lock(&self.screen);
        if let Some(reason) = &screen.gone {
            return Err(DriverError::Disconnected(reason.clone()));
        }
        Ok((screen.framebuffer.width(), screen.framebuffer.height()))
    }

    fn check_fits(&self, area: Area) -> WireResult<()> {
        let screen = lock(&self.screen);
        let (width, height) = (screen.framebuffer.width(), screen.framebuffer.height());
        if area.fits(width, height) {
            Ok(())
        } else {
            Err(WireError::Protocol(format!(
                "the VNC server sent a rectangle ({},{} {}x{}) outside its {width}x{height} screen",
                area.x, area.y, area.width, area.height
            )))
        }
    }

    /// Resizes the framebuffer, answering whether the size changed.
    fn resize(&self, width: u32, height: u32) -> WireResult<bool> {
        decode::check_size(width, height).map_err(WireError::Protocol)?;
        let mut screen = lock(&self.screen);
        if (screen.framebuffer.width(), screen.framebuffer.height()) == (width, height) {
            return Ok(false);
        }
        screen.framebuffer.resize(width, height);
        screen.snapshot = None;
        screen.dirty_whole = true;
        Ok(true)
    }

    fn emit(&self, event: DriverEvent) {
        (self.events)(event);
    }
}

/// Reads server messages until the connection ends, then says why, once.
fn read_loop(shared: Arc<Shared>, reader: Box<dyn Read + Send>) {
    let mut reader = BufReader::with_capacity(64 * 1024, reader);
    let mut inflate = Decompress::new(true);
    let error = loop {
        if let Err(error) = read_message(&shared, &mut reader, &mut inflate) {
            break error;
        }
    };
    let reason = match error {
        WireError::Protocol(message) => message,
        WireError::Io(error) => match error.kind() {
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                "the VNC server closed the connection".to_owned()
            }
            std::io::ErrorKind::ConnectionAborted => "the VNC connection was closed".to_owned(),
            _ => format!("the connection to the VNC server broke: {error}"),
        },
    };
    let reason = shared.end(reason);
    shared.emit(DriverEvent::Disconnected(reason));
}

fn read_message(
    shared: &Shared,
    reader: &mut impl Read,
    inflate: &mut Decompress,
) -> WireResult<()> {
    let mut kind = [0u8; 1];
    reader.read_exact(&mut kind)?;
    match kind[0] {
        0 => framebuffer_update(shared, reader, inflate),
        // SetColourMapEntries: only for colour-mapped formats, which this client
        // never asks for. Read past it.
        1 => {
            let mut header = [0u8; 5];
            reader.read_exact(&mut header)?;
            let count = u16::from_be_bytes([header[3], header[4]]);
            decode::skip(reader, u64::from(count) * 6)?;
            Ok(())
        }
        // Bell.
        2 => Ok(()),
        3 => server_cut_text(shared, reader),
        other => Err(WireError::Protocol(format!(
            "the VNC server sent message type {other}, which this client never asked for"
        ))),
    }
}

/// ServerCutText, after its type: three bytes of padding and a signed length.
/// Zero or more is the classic message, the server's clipboard in Latin-1,
/// kept as the last text it sent. Less is an Extended Clipboard message of
/// `abs(length)` bytes, flags first (`clipboard`).
fn server_cut_text(shared: &Shared, reader: &mut impl Read) -> WireResult<()> {
    let mut padding = [0u8; 3];
    reader.read_exact(&mut padding)?;
    let length = decode::read_u32(reader)? as i32;
    let size = length.unsigned_abs();
    if size > MAX_CUT_TEXT {
        return Err(WireError::Protocol(format!(
            "the VNC server sent a clipboard of {size} bytes, more than this client accepts"
        )));
    }
    if length >= 0 {
        let kept = (size as usize).min(MAX_KEPT_CLIPBOARD);
        let mut text = vec![0u8; kept];
        reader.read_exact(&mut text)?;
        decode::skip(reader, u64::from(size) - kept as u64)?;
        shared.server_text(clipboard::latin1_text(&text), false);
        return Ok(());
    }
    if size < 4 {
        // Too short to hold its flags: nothing to act on.
        decode::skip(reader, u64::from(size))?;
        return Ok(());
    }
    let flags = decode::read_u32(reader)?;
    let rest = u64::from(size - 4);
    if flags & clipboard::CAPS != 0 {
        // One maximum size per format follows; this client sends only what is
        // asked for, so it needs none of them.
        decode::skip(reader, rest)?;
        return shared.server_caps(flags);
    }
    match flags & clipboard::ACTIONS {
        clipboard::PROVIDE => {
            let mut compressed = vec![0u8; rest as usize];
            reader.read_exact(&mut compressed)?;
            shared.server_provides(flags, &compressed);
            Ok(())
        }
        action => {
            decode::skip(reader, rest)?;
            match action {
                clipboard::NOTIFY => shared.server_notifies(flags),
                clipboard::REQUEST => shared.server_requests(flags),
                clipboard::PEEK => shared.server_peeks(),
                // Several actions at once, or none: no message of the extension.
                _ => Ok(()),
            }
        }
    }
}

impl Shared {
    /// Sends from the reader thread, whose errors end the connection.
    fn answer(&self, message: &[u8]) -> WireResult<()> {
        self.send(message)
            .map_err(|error| WireError::Protocol(error.message().to_owned()))
    }

    /// The server's clipboard text arrived: as classic cut text, or
    /// `provided` through the Extended Clipboard.
    fn server_text(&self, text: String, provided: bool) {
        let mut clipboard = lock(&self.clipboard);
        clipboard.received = Some(ClipboardText {
            text,
            received: Instant::now(),
        });
        clipboard.ours = None;
        if provided {
            clipboard.provides += 1;
        }
        drop(clipboard);
        self.clipboard_changed.notify_all();
    }

    /// The server's caps, sent for each SetEncodings that lists the
    /// extension: kept, and answered with this client's, once each.
    fn server_caps(&self, flags: u32) -> WireResult<()> {
        let first = lock(&self.clipboard).caps.replace(flags).is_none();
        if first {
            self.emit(DriverEvent::Log(
                if flags & clipboard::TEXT != 0 {
                    "the server has the Extended Clipboard: clipboard text goes both ways in UTF-8"
                } else {
                    "the server has the Extended Clipboard, but not for text"
                }
                .to_owned(),
            ));
        }
        self.answer(&clipboard::client_caps())
    }

    fn server_provides(&self, flags: u32, compressed: &[u8]) {
        match clipboard::provided_text(flags, compressed, MAX_KEPT_CLIPBOARD) {
            Ok(Some(text)) => self.server_text(text, true),
            // Formats other than text, which nothing here reads.
            Ok(None) => {}
            Err(error) => self.emit(DriverEvent::Log(format!(
                "ignored the clipboard the server sent: {error}"
            ))),
        }
    }

    /// The server's clipboard changed. With text, a read waiting for it asks
    /// for it (the client's caps let nothing come unasked); without, the
    /// server holds no text, which ends the wait.
    fn server_notifies(&self, flags: u32) -> WireResult<()> {
        let mut clipboard = lock(&self.clipboard);
        clipboard.ours = None;
        if flags & clipboard::TEXT == 0 {
            clipboard.received = None;
            clipboard.emptied += 1;
            drop(clipboard);
            self.clipboard_changed.notify_all();
            return Ok(());
        }
        let ask = clipboard.waiting > 0 && clipboard.takes(clipboard::REQUEST);
        drop(clipboard);
        if ask {
            self.answer(&clipboard::client_action(clipboard::REQUEST))?;
        }
        Ok(())
    }

    /// The server asks for the text this client announced: the last it put
    /// there, if any.
    fn server_requests(&self, flags: u32) -> WireResult<()> {
        if flags & clipboard::TEXT == 0 {
            return Ok(());
        }
        let offered = lock(&self.clipboard).offered.clone();
        match offered {
            Some(text) => self.answer(&clipboard::client_provide(&text)),
            None => Ok(()),
        }
    }

    /// The server asks what this client's clipboard holds: answered only
    /// while it holds this client's text, so a peek never takes the clipboard
    /// back from whatever the desktop copied since.
    fn server_peeks(&self) -> WireResult<()> {
        let holds = lock(&self.clipboard).ours.is_some();
        if holds {
            self.answer(&clipboard::client_action(clipboard::NOTIFY))?;
        }
        Ok(())
    }

    /// Waits, at most `limit`, for the first update after connecting, by which
    /// the server has said whether it has the Extended Clipboard ([`CAPS_WAIT`]).
    fn first_update_within(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut screen = lock(&self.screen);
        while screen.completed == 0 && screen.gone.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            screen = match self.changed.wait_timeout(screen, deadline - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

fn framebuffer_update(
    shared: &Shared,
    reader: &mut impl Read,
    inflate: &mut Decompress,
) -> WireResult<()> {
    let mut header = [0u8; 3];
    reader.read_exact(&mut header)?;
    let count = u16::from_be_bytes([header[1], header[2]]);
    {
        let mut screen = lock(&shared.screen);
        screen.started += 1;
        screen.in_update = true;
    }
    let mut resized = false;
    let mut sink = |area: Area, pixels: &[u8]| {
        let mut screen = lock(&shared.screen);
        screen.framebuffer.blit(area, pixels);
        screen.snapshot = None;
        screen.mark_dirty(area);
    };
    for _ in 0..count {
        let mut rectangle = [0u8; 12];
        reader.read_exact(&mut rectangle)?;
        let field = |at: usize| u16::from_be_bytes([rectangle[at], rectangle[at + 1]]);
        let area = Area::new(field(0), field(2), field(4), field(6));
        let kind = i32::from_be_bytes([rectangle[8], rectangle[9], rectangle[10], rectangle[11]]);
        match kind {
            encoding::LAST_RECT => break,
            encoding::DESKTOP_SIZE => resized |= shared.resize(area.width, area.height)?,
            encoding::EXTENDED_DESKTOP_SIZE => {
                let mut screens = [0u8; 4];
                reader.read_exact(&mut screens)?;
                decode::skip(reader, u64::from(screens[0]) * 16)?;
                // x = 1 with a non-zero y answers a SetDesktopSize that failed; this client
                // sends none, so only the server's own resizes (and other clients') apply.
                if !(area.x == 1 && area.y != 0) {
                    resized |= shared.resize(area.width, area.height)?;
                }
            }
            encoding::RAW => {
                shared.check_fits(area)?;
                decode::raw(reader, area, &mut sink)?;
            }
            encoding::COPY_RECT => {
                let source_x = u32::from(decode::read_u16(reader)?);
                let source_y = u32::from(decode::read_u16(reader)?);
                shared.check_fits(area)?;
                shared.check_fits(Area {
                    x: source_x,
                    y: source_y,
                    ..area
                })?;
                let mut screen = lock(&shared.screen);
                screen.framebuffer.copy_within(source_x, source_y, area);
                screen.snapshot = None;
                screen.mark_dirty(area);
            }
            encoding::HEXTILE => {
                shared.check_fits(area)?;
                decode::hextile(reader, area, &mut sink)?;
            }
            encoding::ZRLE => {
                shared.check_fits(area)?;
                decode::zrle(reader, area, inflate, &mut sink)?;
            }
            other => {
                return Err(WireError::Protocol(format!(
                    "the VNC server sent a rectangle in encoding {other}, which this client did not offer"
                )))
            }
        }
    }
    let (width, height) = {
        let mut screen = lock(&shared.screen);
        screen.in_update = false;
        screen.completed += 1;
        screen.seq += 1;
        screen.last_update = Some(Instant::now());
        (screen.framebuffer.width(), screen.framebuffer.height())
    };
    shared.changed.notify_all();
    if resized {
        shared.emit(DriverEvent::Log(format!(
            "screen resized to {width}x{height}"
        )));
        shared.emit(DriverEvent::Resized { width, height });
    }
    shared.emit(DriverEvent::Frame);
    // After a resize the old picture means nothing, so the next request is for all of it.
    shared
        .send(&update_request(!resized, width, height))
        .map_err(|error| WireError::Protocol(error.message().to_owned()))
}

/// The driver the session holds. Dropping it closes the connection, so a
/// session that forgets to call [`Driver::close`] does not leave a reader
/// thread holding a socket open.
struct VncDriver {
    shared: Arc<Shared>,
}

impl Drop for VncDriver {
    fn drop(&mut self) {
        self.shared.end("the VNC connection was closed".to_owned());
    }
}

impl VncDriver {
    /// Asks for the screen as it is now and waits until it has arrived and
    /// stopped changing: an update that began after this call has completed and
    /// no other has followed for [`SETTLE`], or [`FRESH_LIMIT`] has passed.
    ///
    /// The incremental request covers any change; the one-pixel request that is
    /// not incremental is there because a server answers that one at once,
    /// changed or not, so a still screen settles in a round trip instead of
    /// waiting out the limit.
    fn settle(&self) -> DriverResult<()> {
        let (started_before, width, height) = {
            let screen = lock(&self.shared.screen);
            if let Some(reason) = &screen.gone {
                return Err(DriverError::Disconnected(reason.clone()));
            }
            (
                screen.started,
                screen.framebuffer.width(),
                screen.framebuffer.height(),
            )
        };
        let mut requests = update_request(true, width, height).to_vec();
        if width > 0 && height > 0 {
            requests.extend_from_slice(&area_request(false, 0, 0, 1, 1));
        }
        self.shared.send(&requests)?;
        let deadline = Instant::now() + FRESH_LIMIT;
        let mut screen = lock(&self.shared.screen);
        loop {
            if let Some(reason) = &screen.gone {
                return Err(DriverError::Disconnected(reason.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let wake = if screen.completed > started_before && !screen.in_update {
                let quiet_at = screen.last_update.map_or(now, |at| at + SETTLE);
                if now >= quiet_at {
                    return Ok(());
                }
                quiet_at.min(deadline)
            } else {
                deadline
            };
            screen = match self.shared.changed.wait_timeout(screen, wake - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn key_event(message: &mut Vec<u8>, symbol: u32, down: bool) {
        message.extend_from_slice(&[4, u8::from(down), 0, 0]);
        message.extend_from_slice(&symbol.to_be_bytes());
    }

    /// The keysym of a named key, `None` for a phone button. Apple's server
    /// maps modifiers by a table of its own: Meta_L/R are Option, while
    /// Alt_L/R — like Super_L/R and Hyper_L/R — are Command (research-rfb §4.4,
    /// from remotex's measurements on macOS 26.5-26.6, `docs/apple-vnc-889.md`).
    /// So `alt` goes to it as Meta_L, to arrive as the Option it means, and
    /// `cmd` stays Super_L. Every other server gets the keysyms as named.
    fn named_keysym(&self, key: Key) -> Option<u32> {
        match key {
            Key::Named(NamedKey::Alt) if self.shared.apple => Some(META_L),
            _ => keysym(key),
        }
    }

    /// The keysym for `character`, unless this server cannot take it: on a
    /// server that reads keysyms in sixteen bits, a character beyond Latin-1
    /// would at best be dropped and at worst press a key that deletes, submits
    /// or locks the case.
    fn char_keysym(&self, character: char) -> DriverResult<u32> {
        if self.shared.sixteen_bit_keysyms && !is_latin1_keysym(character) {
            let shown = if character.is_control() {
                format!("U+{:04X}", u32::from(character))
            } else {
                format!("\"{character}\" (U+{:04X})", u32::from(character))
            };
            let consequence = if misread_in_sixteen_bits(character) {
                ", and would take it for a control key (BackSpace, Return, Tab, Caps Lock...)"
            } else {
                ""
            };
            return Err(DriverError::Unsupported(format!(
                "this VNC server is QEMU's, which types only the characters of its keyboard layout (ASCII and Latin-1){consequence}; it cannot type {shown}. Nothing was typed. Type that text inside the guest some other way."
            )));
        }
        keysym(Key::Char(character)).ok_or_else(|| {
            DriverError::Unsupported(format!(
                "no VNC key types \"{character}\"; nothing was typed."
            ))
        })
    }

    /// The key presses that type `text`, each with whether Shift goes around
    /// it, and whether any is a Unicode keysym. Refuses the whole text, before
    /// anything is sent, when one character cannot be typed.
    fn presses(&self, text: &str) -> DriverResult<(Vec<(u32, bool)>, bool)> {
        let mut presses: Vec<(u32, bool)> = Vec::with_capacity(text.len());
        let mut unicode = false;
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            let symbol = match character {
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    keysym(Key::Named(NamedKey::Enter))
                }
                '\n' => keysym(Key::Named(NamedKey::Enter)),
                '\t' => keysym(Key::Named(NamedKey::Tab)),
                '\u{8}' => keysym(Key::Named(NamedKey::Backspace)),
                '\u{1b}' => keysym(Key::Named(NamedKey::Escape)),
                '\u{7f}' => keysym(Key::Named(NamedKey::Delete)),
                control if control.is_control() => {
                    return Err(DriverError::Unsupported(format!(
                        "the text contains the control character U+{:04X}, which no key types; nothing was typed",
                        u32::from(control)
                    )))
                }
                character => Some(self.char_keysym(character)?),
            };
            let Some(symbol) = symbol else { continue };
            unicode |= symbol >= 0x0100_0000;
            presses.push((symbol, shifted(character)));
        }
        Ok((presses, unicode))
    }
}

/// Whether a US keyboard types `character` with Shift held. Typing sends
/// Shift around these, as a person at a US keyboard would: QEMU turns keysyms
/// into scancodes and ignores their case, so without it "A" types "a" and "!"
/// types "1", while servers that map keysyms themselves (X, macOS, Windows)
/// accept the held Shift either way.
fn shifted(character: char) -> bool {
    character.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(character)
}

impl Driver for VncDriver {
    fn info(&self) -> DriverInfo {
        let screen = lock(&self.shared.screen);
        DriverInfo {
            platform: Platform::Desktop,
            label: self.shared.name.clone(),
            width: screen.framebuffer.width(),
            height: screen.framebuffer.height(),
            has_ui_tree: false,
        }
    }

    /// The framebuffer as it stands. `captured_at` is the moment of the call:
    /// the server pushes every change, so the picture is as current as the
    /// connection, however long ago the screen last changed.
    fn frame(&self, fresh: bool) -> DriverResult<Frame> {
        if fresh {
            self.settle()?;
        }
        let mut screen = lock(&self.shared.screen);
        if let Some(reason) = &screen.gone {
            return Err(DriverError::Disconnected(reason.clone()));
        }
        let (width, height) = (screen.framebuffer.width(), screen.framebuffer.height());
        if width == 0 || height == 0 {
            return Err(DriverError::Failed(
                "the VNC server has not given its screen a size yet".to_owned(),
            ));
        }
        let rgba = match &screen.snapshot {
            Some(rgba) => rgba.clone(),
            None => {
                let rgba: Arc<[u8]> = Arc::from(screen.framebuffer.rgba());
                screen.snapshot = Some(rgba.clone());
                rgba
            }
        };
        Ok(Frame {
            width,
            height,
            rgba,
            seq: screen.seq,
            captured_at: Instant::now(),
        })
    }

    /// Only what the update touched, so a cursor blinking on a 4K desktop moves a few hundred bytes
    /// and not the 33 MB a whole picture is.
    fn take_patch(&self) -> Option<ScreenUpdate> {
        let mut screen = lock(&self.shared.screen);
        if screen.dirty_whole {
            return screen.whole();
        }
        let area = screen.dirty.take()?;
        let stride = screen.framebuffer.width() as usize * 4;
        let row = area.width as usize * 4;
        let source = screen.framebuffer.rgba();
        let mut rgba = Vec::with_capacity(row * area.height as usize);
        for y in area.y as usize..(area.y + area.height) as usize {
            let start = y * stride + area.x as usize * 4;
            rgba.extend_from_slice(&source[start..start + row]);
        }
        Some(ScreenUpdate::Patch { x: area.x, y: area.y, width: area.width, height: area.height, rgba })
    }

    fn take_full(&self) -> Option<ScreenUpdate> {
        lock(&self.shared.screen).whole()
    }

    fn pointer(&self, x: u32, y: u32, buttons: u8) -> DriverResult<()> {
        let (width, height) = self.shared.size()?;
        let x = x.min(width.saturating_sub(1)).min(u32::from(u16::MAX)) as u16;
        let y = y.min(height.saturating_sub(1)).min(u32::from(u16::MAX)) as u16;
        let mut message = [5, buttons, 0, 0, 0, 0];
        message[2..4].copy_from_slice(&x.to_be_bytes());
        message[4..6].copy_from_slice(&y.to_be_bytes());
        self.shared.send(&message)
    }

    fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
        let symbol = match key {
            Key::Char(character) => self.char_keysym(*character)?,
            Key::Named(named) => self.named_keysym(*key).ok_or_else(|| {
                DriverError::Unsupported(format!(
                    "\"{named:?}\" is a phone button; a VNC desktop has no such key. Use a keyboard key or shortcut instead."
                ))
            })?,
        };
        let mut message = Vec::with_capacity(16);
        match down {
            None => {
                Self::key_event(&mut message, symbol, true);
                Self::key_event(&mut message, symbol, false);
            }
            Some(edge) => Self::key_event(&mut message, symbol, edge),
        }
        self.shared.send(&message)
    }

    /// Types `text` one key press per character. RFB has no text message, and
    /// no server confirms a key: a character the server's keyboard cannot
    /// produce (much of CJK, for one) is sent as a Unicode keysym and may
    /// simply not appear — except on QEMU, which is refused it
    /// ([`Self::char_keysym`]).
    fn text(&self, text: &str) -> DriverResult<()> {
        let (presses, unicode) = self.presses(text)?;
        if presses.is_empty() {
            return Ok(());
        }
        if unicode && !self.shared.unicode_noted.swap(true, Ordering::Relaxed) {
            self.shared.emit(DriverEvent::Log(
                "typed characters beyond Latin-1 as Unicode keysyms; the server does not confirm keys and may ignore them"
                    .to_owned(),
            ));
        }
        for chunk in presses.chunks(TEXT_EVENTS_PER_WRITE) {
            let mut message = Vec::with_capacity(chunk.len() * 32);
            for &(symbol, shift) in chunk {
                if shift {
                    Self::key_event(&mut message, SHIFT, true);
                }
                Self::key_event(&mut message, symbol, true);
                Self::key_event(&mut message, symbol, false);
                if shift {
                    Self::key_event(&mut message, SHIFT, false);
                }
            }
            self.shared.send(&message)?;
        }
        Ok(())
    }

    fn check_text(&self, text: &str) -> DriverResult<()> {
        self.presses(text).map(|_| ())
    }

    fn close(&self) {
        self.shared.end("the VNC connection was closed".to_owned());
    }

    fn desktop(&self) -> Option<&dyn DesktopControl> {
        Some(self)
    }
}

/// The clipboard (`clipboard`, and the module documentation). Every call
/// first lets the server say whether it has the Extended Clipboard
/// ([`CAPS_WAIT`]), which only takes time in the second after connecting.
impl DesktopControl for VncDriver {
    fn clipboard_channel(&self) -> ClipboardChannel {
        self.shared.first_update_within(CAPS_WAIT);
        if lock(&self.shared.clipboard).extended() {
            ClipboardChannel::Extended
        } else {
            ClipboardChannel::Latin1
        }
    }

    /// Extended: a notify and then the text itself (research-rfb §7.2), as
    /// far as the server's caps take each: the notify makes TigerVNC and QEMU
    /// take the clipboard for this client, the provide spares them asking for
    /// it. The text stays to answer the server's later requests. Latin-1: the
    /// classic cut text, refused before anything is sent when a character is
    /// beyond Latin-1. Either way, text above [`MAX_SENT_CLIPBOARD`] as it
    /// would go is refused before anything is sent.
    fn set_clipboard(&self, text: &str) -> DriverResult<()> {
        self.shared.first_update_within(CAPS_WAIT);
        if let Some(reason) = self.shared.gone() {
            return Err(DriverError::Disconnected(reason));
        }
        let mut clipboard = lock(&self.shared.clipboard);
        let message = if clipboard.extended() {
            if !clipboard.takes(clipboard::NOTIFY) && !clipboard.takes(clipboard::PROVIDE) {
                return Err(DriverError::Unsupported(
                    "this VNC server's Extended Clipboard takes no text from viewers; nothing was put on its clipboard".to_owned(),
                ));
            }
            // Also when only the notify goes: the server then asks for this text.
            fits_on_clipboard(
                clipboard::provided_size(text),
                "as UTF-8 with CRLF line ends",
            )?;
            let mut message = Vec::new();
            if clipboard.takes(clipboard::NOTIFY) {
                message.extend(clipboard::client_action(clipboard::NOTIFY));
            }
            if clipboard.takes(clipboard::PROVIDE) {
                message.extend(clipboard::client_provide(text));
            }
            message
        } else {
            match clipboard::latin1_cut_text(text) {
                Some(message) => {
                    // The text after the type, padding and length.
                    fits_on_clipboard(message.len() - 8, "in Latin-1")?;
                    message
                }
                None => {
                    let character = clipboard::beyond_latin1(text).unwrap_or('?');
                    return Err(DriverError::Unsupported(format!(
                        "this VNC server takes only Latin-1 clipboard text (it lacks the Extended Clipboard extension), and U+{:04X} is not Latin-1; nothing was put on its clipboard",
                        u32::from(character)
                    )));
                }
            }
        };
        // Kept before it goes, so a request the notify brings at once is
        // answered with this text.
        clipboard.offered = Some(text.to_owned());
        clipboard.ours = Some(ClipboardText {
            text: text.replace("\r\n", "\n"),
            received: Instant::now(),
        });
        drop(clipboard);
        self.shared.send(&message)
    }

    /// Extended: this client's own text while the clipboard still holds it,
    /// else the server's, asked for and awaited up to `timeout`. `None` only
    /// when the server answered that its clipboard is empty: a server that
    /// does not answer in time, or whose caps take no requests, leaves the
    /// last text it sent, and without one is `Unsupported`, whose text says
    /// so (`desktop` answers with it). Latin-1: the last text the server sent
    /// unasked, if any.
    fn clipboard(&self, timeout: Duration) -> DriverResult<Option<ClipboardText>> {
        let deadline = Instant::now() + timeout;
        self.shared.first_update_within(CAPS_WAIT.min(timeout));
        if let Some(reason) = self.shared.gone() {
            return Err(DriverError::Disconnected(reason));
        }
        let mut clipboard = lock(&self.shared.clipboard);
        if !clipboard.extended() {
            return Ok(clipboard.received.clone());
        }
        if let Some(ours) = &clipboard.ours {
            return Ok(Some(ours.clone()));
        }
        if !clipboard.takes(clipboard::REQUEST) {
            return unanswered(clipboard.received.clone(), || {
                "this VNC server's Extended Clipboard takes no requests, so its clipboard arrives only when the server sends it unasked, and it has sent none since the connection was made. Copy the text there again and read once more".to_owned()
            });
        }
        let (provides, emptied) = (clipboard.provides, clipboard.emptied);
        clipboard.waiting += 1;
        drop(clipboard);
        let sent = self
            .shared
            .send(&clipboard::client_action(clipboard::REQUEST));
        let mut clipboard = lock(&self.shared.clipboard);
        let result = loop {
            if let Err(error) = &sent {
                break Err(error.clone());
            }
            if clipboard.provides > provides || clipboard.emptied > emptied {
                break Ok(clipboard.received.clone());
            }
            if let Some(reason) = self.shared.gone() {
                break Err(DriverError::Disconnected(reason));
            }
            let now = Instant::now();
            if now >= deadline {
                break unanswered(clipboard.received.clone(), || {
                    format!(
                        "this VNC server did not answer within {} when asked for its clipboard. Its clipboard may be empty (some servers then send nothing), or out of its reach: QEMU sees the guest's clipboard only while spice-vdagent runs in the guest. Copy the text there again and read once more",
                        waited(timeout)
                    )
                });
            }
            clipboard = match self
                .shared
                .clipboard_changed
                .wait_timeout(clipboard, deadline - now)
            {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        };
        clipboard.waiting -= 1;
        result
    }

    fn is_macos(&self) -> bool {
        self.shared.apple
    }
}

/// Refuses clipboard text of `size` bytes `form` (how it would go) above
/// [`MAX_SENT_CLIPBOARD`], before anything is sent.
fn fits_on_clipboard(size: usize, form: &str) -> DriverResult<()> {
    if size <= MAX_SENT_CLIPBOARD {
        return Ok(());
    }
    Err(DriverError::Unsupported(format!(
        "the text is {size} bytes {form}, more than the {MAX_SENT_CLIPBOARD} a VNC server is sure to take (TigerVNC drops clipboard text above 256 KiB without a word); nothing was put on its clipboard. Split the text into smaller parts"
    )))
}

/// A clipboard read the server did not answer: the last text it sent, if
/// any (its age says how old), else `Unsupported` with `why` — never `None`,
/// which says the server answered that its clipboard is empty.
fn unanswered(
    received: Option<ClipboardText>,
    why: impl FnOnce() -> String,
) -> DriverResult<Option<ClipboardText>> {
    match received.filter(|text| !text.text.is_empty()) {
        Some(text) => Ok(Some(text)),
        None => Err(DriverError::Unsupported(why())),
    }
}

/// How long a read waited, as its answer says it: `2 s`, `1.5 s`, `300 ms`.
fn waited(timeout: Duration) -> String {
    let millis = timeout.as_millis();
    if millis < 1000 {
        format!("{millis} ms")
    } else if millis % 1000 == 0 {
        format!("{} s", millis / 1000)
    } else {
        format!("{:.1} s", timeout.as_secs_f64())
    }
}
