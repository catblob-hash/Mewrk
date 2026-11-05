//! The driver against fake RFB servers on this computer's loopback: each test's
//! server is a thread speaking the server's half of the protocol byte by byte,
//! so what goes over the wire is asserted exactly.

use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use aes::{
    cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit},
    Aes128,
};
use flate2::{write::ZlibEncoder, Compress, Compression, FlushCompress, FlushDecompress, Status};
use md5::{Digest, Md5};
use num_bigint::BigUint;

use super::*;
use crate::remote_control::{driver::buttons, keys::KeyChord, transport::LocalConnector};

type Seen = Arc<Mutex<Vec<DriverEvent>>>;

fn recorder() -> (DriverEvents, Seen) {
    let seen: Seen = Arc::default();
    let sink = seen.clone();
    (Arc::new(move |event| lock(&sink).push(event)), seen)
}

fn wait_for(seen: &Seen, what: &str, done: impl Fn(&[DriverEvent]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if done(&lock(seen)) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}; events: {:?}", lock(seen));
}

fn count(seen: &Seen, matches: impl Fn(&DriverEvent) -> bool) -> usize {
    lock(seen).iter().filter(|event| matches(event)).count()
}

fn frames(seen: &Seen) -> usize {
    count(seen, |event| *event == DriverEvent::Frame)
}

fn logs(seen: &Seen) -> Vec<String> {
    lock(seen)
        .iter()
        .filter_map(|event| match event {
            DriverEvent::Log(line) => Some(line.clone()),
            _ => None,
        })
        .collect()
}

/// A fake server: accepts one connection and runs `script` on it.
fn serve<T: Send + 'static>(
    script: impl FnOnce(TcpStream) -> T + Send + 'static,
) -> (u16, thread::JoinHandle<T>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        script(stream)
    });
    (port, server)
}

fn connect_to(port: u16, auth: VncAuth, events: DriverEvents) -> DriverResult<Arc<dyn Driver>> {
    connect(Arc::new(LocalConnector::new()), port, auth, events)
}

fn read_n(stream: &mut TcpStream, count: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; count];
    stream.read_exact(&mut bytes).unwrap();
    bytes
}

/// Whether the client closed its side without sending anything more.
fn client_hung_up(stream: &mut TcpStream) -> bool {
    let mut byte = [0u8; 1];
    matches!(stream.read(&mut byte), Ok(0) | Err(_))
}

fn greet(stream: &mut TcpStream, version: &[u8; 12], answer: &[u8; 12]) {
    stream.write_all(version).unwrap();
    assert_eq!(read_n(stream, 12), answer.to_vec());
}

/// Offers `types` (3.7 and later) and answers the client's choice.
fn offer(stream: &mut TcpStream, types: &[u8]) -> u8 {
    let mut message = vec![types.len() as u8];
    message.extend_from_slice(types);
    stream.write_all(&message).unwrap();
    read_n(stream, 1)[0]
}

fn security_ok(stream: &mut TcpStream) {
    stream.write_all(&0u32.to_be_bytes()).unwrap();
}

fn security_failed(stream: &mut TcpStream, reason: &str) {
    stream.write_all(&1u32.to_be_bytes()).unwrap();
    stream
        .write_all(&(reason.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(reason.as_bytes()).unwrap();
}

/// Expects ClientInit (shared) and sends ServerInit.
fn server_init(stream: &mut TcpStream, width: u16, height: u16, name: &str) {
    assert_eq!(
        read_n(stream, 1),
        vec![1],
        "ClientInit must ask to share the desktop"
    );
    let mut message = Vec::new();
    message.extend_from_slice(&width.to_be_bytes());
    message.extend_from_slice(&height.to_be_bytes());
    // The server's own format, 16 bits, which the client must replace.
    message.extend_from_slice(&[16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0]);
    message.extend_from_slice(&(name.len() as u32).to_be_bytes());
    message.extend_from_slice(name.as_bytes());
    stream.write_all(&message).unwrap();
}

/// Expects SetPixelFormat, SetEncodings and the first, full update request.
fn expect_setup(stream: &mut TcpStream, width: u16, height: u16) {
    let pixel_format = read_n(stream, 20);
    assert_eq!(pixel_format[0], 0);
    assert_eq!(&pixel_format[4..], &PIXEL_FORMAT);
    assert_eq!(
        &PIXEL_FORMAT[..],
        &[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0],
        "32 bpp, depth 24, little-endian, true colour, R0 G8 B16"
    );
    let header = read_n(stream, 4);
    assert_eq!(header[0], 2);
    let count = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let encodings: Vec<i32> = read_n(stream, count * 4)
        .chunks(4)
        .map(|bytes| i32::from_be_bytes(bytes.try_into().unwrap()))
        .collect();
    assert_eq!(encodings, vec![16, 5, 1, 0, -223, -308, -224, -1063131698]);
    assert_eq!(encodings[7] as u32, 0xC0A1_E5CE, "the Extended Clipboard");
    assert!(
        !encodings.contains(&-239),
        "the cursor stays in the picture"
    );
    assert_eq!(read_n(stream, 10), request(false, width, height));
}

fn request(incremental: bool, width: u16, height: u16) -> Vec<u8> {
    let mut message = vec![3, u8::from(incremental), 0, 0, 0, 0];
    message.extend_from_slice(&width.to_be_bytes());
    message.extend_from_slice(&height.to_be_bytes());
    message
}

/// A connected desktop without authentication; the test plays the server
/// through the returned stream.
fn desktop(width: u16, height: u16) -> (Arc<dyn Driver>, Seen, TcpStream) {
    desktop_named(width, height, "fake desktop")
}

/// A desktop whose ServerInit calls it `name`.
fn desktop_named(
    width: u16,
    height: u16,
    name: &'static str,
) -> (Arc<dyn Driver>, Seen, TcpStream) {
    let (events, seen) = recorder();
    let (port, server) = serve(move |mut stream| {
        greet(&mut stream, b"RFB 003.008\n", b"RFB 003.008\n");
        assert_eq!(offer(&mut stream, &[1]), 1);
        security_ok(&mut stream);
        server_init(&mut stream, width, height, name);
        expect_setup(&mut stream, width, height);
        stream
    });
    let driver = connect_to(port, VncAuth::default(), events).unwrap();
    (driver, seen, server.join().unwrap())
}

fn rectangle(x: u16, y: u16, width: u16, height: u16, encoding: i32) -> Vec<u8> {
    let mut header = Vec::new();
    for value in [x, y, width, height] {
        header.extend_from_slice(&value.to_be_bytes());
    }
    header.extend_from_slice(&encoding.to_be_bytes());
    header
}

fn update(rectangles: &[Vec<u8>]) -> Vec<u8> {
    let mut message = vec![0, 0];
    message.extend_from_slice(&(rectangles.len() as u16).to_be_bytes());
    for rectangle in rectangles {
        message.extend_from_slice(rectangle);
    }
    message
}

fn raw(x: u16, y: u16, width: u16, height: u16, colour: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut bytes = rectangle(x, y, width, height, 0);
    for row in 0..u32::from(height) {
        for column in 0..u32::from(width) {
            let pixel = colour(u32::from(x) + column, u32::from(y) + row);
            bytes.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0]);
        }
    }
    bytes
}

/// Sends a message and waits until the driver has applied a whole update.
fn apply(stream: &mut TcpStream, seen: &Seen, message: &[u8]) {
    let before = frames(seen);
    stream.write_all(message).unwrap();
    wait_for(seen, "a frame", |events| {
        events
            .iter()
            .filter(|event| **event == DriverEvent::Frame)
            .count()
            > before
    });
}

/// The next message the client sent, whole.
fn client_message(stream: &mut TcpStream) -> Vec<u8> {
    let mut message = read_n(stream, 1);
    let rest = match message[0] {
        0 => 19,
        3 => 9,
        4 => 7,
        5 => 5,
        // ClientCutText: padding and a signed length, negative for the Extended Clipboard.
        6 => {
            message.extend(read_n(stream, 7));
            let length = i32::from_be_bytes(message[4..8].try_into().unwrap());
            length.unsigned_abs() as usize
        }
        other => panic!("unexpected client message type {other}"),
    };
    message.extend(read_n(stream, rest));
    message
}

/// The next pointer or key message, past any update requests.
fn next_input(stream: &mut TcpStream) -> Vec<u8> {
    loop {
        let message = client_message(stream);
        if message[0] != 3 {
            return message;
        }
    }
}

fn key_message(symbol: u32, down: bool) -> Vec<u8> {
    let mut message = vec![4, u8::from(down), 0, 0];
    message.extend_from_slice(&symbol.to_be_bytes());
    message
}

fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * frame.width + x) * 4) as usize;
    frame.rgba[at..at + 4].try_into().unwrap()
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

#[test]
fn a_3_3_server_chooses_no_authentication_and_its_desktop_comes_up() {
    let (events, seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.003\n", b"RFB 003.003\n");
        stream.write_all(&1u32.to_be_bytes()).unwrap();
        server_init(&mut stream, 64, 48, "QEMU (debian)");
        expect_setup(&mut stream, 64, 48);
        stream
    });
    let driver = connect_to(port, VncAuth::default(), events).unwrap();
    let mut stream = server.join().unwrap();
    assert_eq!(
        driver.info(),
        DriverInfo {
            platform: Platform::Desktop,
            label: "QEMU (debian)".to_owned(),
            width: 64,
            height: 48,
            has_ui_tree: false,
        }
    );
    let lines = logs(&seen);
    assert!(
        lines.iter().any(|line| line.contains("003.003")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("chosen by the server")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"QEMU (debian)\", 64x48")),
        "{lines:?}"
    );

    // Before any update the picture is black, and opaque.
    let black = driver.frame(false).unwrap();
    assert_eq!((black.width, black.height, black.seq), (64, 48, 0));
    assert!(black.rgba.chunks(4).all(|pixel| pixel == [0, 0, 0, 255]));

    apply(
        &mut stream,
        &seen,
        &update(&[raw(0, 0, 64, 48, |x, y| [x as u8, y as u8, 7, 255])]),
    );
    let frame = driver.frame(false).unwrap();
    assert_eq!(frame.seq, 1);
    assert_eq!(frame.rgba.len(), 64 * 48 * 4);
    assert_eq!(pixel(&frame, 0, 0), [0, 0, 7, 255]);
    assert_eq!(pixel(&frame, 63, 47), [63, 47, 7, 255]);
    // The incremental request for what comes next.
    assert_eq!(client_message(&mut stream), request(true, 64, 48));
}

#[test]
fn a_3_7_server_sends_no_security_result_after_no_authentication() {
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.007\n", b"RFB 003.007\n");
        assert_eq!(offer(&mut stream, &[1]), 1);
        server_init(&mut stream, 8, 8, "seven");
        expect_setup(&mut stream, 8, 8);
    });
    let driver = connect_to(port, VncAuth::default(), events).unwrap();
    server.join().unwrap();
    assert_eq!(driver.info().label, "seven");
}

#[test]
fn vnc_authentication_is_preferred_over_none_and_answers_the_challenge() {
    let (events, seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.008\n", b"RFB 003.008\n");
        assert_eq!(offer(&mut stream, &[1, 2]), 2);
        stream
            .write_all(&[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
                0x32, 0x10,
            ])
            .unwrap();
        // DES-ECB under "password" with each key byte's bits reversed (computed with OpenSSL).
        assert_eq!(
            read_n(&mut stream, 16),
            vec![
                0x2b, 0x23, 0x72, 0x57, 0xe9, 0xf9, 0x0c, 0x81, 0x65, 0xdc, 0x8e, 0x23, 0x10, 0x7e,
                0xdf, 0x71
            ]
        );
        security_ok(&mut stream);
        server_init(&mut stream, 8, 8, "tiger");
        expect_setup(&mut stream, 8, 8);
    });
    let auth = VncAuth {
        username: None,
        password: Some(zeroize::Zeroizing::new("password".to_owned())),
    };
    connect_to(port, auth, events).unwrap();
    server.join().unwrap();
    let lines = logs(&seen);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("offered: 1 (no authentication), 2 (VNC password); using 2")),
        "{lines:?}"
    );
}

const MODP_GROUP_2: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381FFFFFFFFFFFFFFFF";

fn from_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn padded(value: &BigUint, length: usize) -> Vec<u8> {
    let mut bytes = value.to_bytes_be();
    while bytes.len() < length {
        bytes.insert(0, 0);
    }
    bytes
}

#[test]
fn apple_screen_sharing_signs_in_with_ard_and_real_diffie_hellman() {
    let (events, seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.889\n", b"RFB 003.008\n");
        assert_eq!(offer(&mut stream, &[30, 33, 36, 35]), 30);
        let prime = from_hex(MODP_GROUP_2);
        let modulus = BigUint::from_bytes_be(&prime);
        let secret = BigUint::from_bytes_be(&[0x5c; 40]);
        let server_public = padded(&BigUint::from(2u32).modpow(&secret, &modulus), 128);
        let mut message = vec![0, 2];
        message.extend_from_slice(&128u16.to_be_bytes());
        message.extend_from_slice(&prime);
        message.extend_from_slice(&server_public);
        stream.write_all(&message).unwrap();

        let mut credentials = read_n(&mut stream, 128);
        let client_public = read_n(&mut stream, 128);
        let shared = padded(
            &BigUint::from_bytes_be(&client_public).modpow(&secret, &modulus),
            128,
        );
        let key = Md5::digest(&shared);
        let cipher = Aes128::new(GenericArray::from_slice(&key));
        for block in credentials.chunks_exact_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
        }
        assert_eq!(&credentials[..8], b"holycat\0");
        assert_eq!(&credentials[64..76], b"s3cret-Pa55\0");
        security_ok(&mut stream);
        server_init(&mut stream, 16, 10, "holycat's MacBook");
        expect_setup(&mut stream, 16, 10);
    });
    let auth = VncAuth {
        username: Some("holycat".to_owned()),
        password: Some(zeroize::Zeroizing::new("s3cret-Pa55".to_owned())),
    };
    let driver = connect_to(port, auth, events).unwrap();
    server.join().unwrap();
    assert_eq!(driver.info().label, "holycat's MacBook");
    let lines = logs(&seen);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("003.889; answered 003.008")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("using 30 (Apple Remote Desktop)")),
        "{lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.contains("s3cret")),
        "{lines:?}"
    );
}

#[test]
fn missing_credentials_are_asked_for_before_anything_is_chosen() {
    // VNC password, none given.
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.008\n", b"RFB 003.008\n");
        stream.write_all(&[1, 2]).unwrap();
        client_hung_up(&mut stream)
    });
    let error = connect_to(port, VncAuth::default(), events).err().unwrap();
    assert!(
        server.join().unwrap(),
        "the client must not pick a type it cannot use"
    );
    assert!(
        matches!(
            &error,
            DriverError::Credentials {
                retry: false,
                needs_username: false,
                ..
            }
        ),
        "{error:?}"
    );

    // Apple Remote Desktop with a password but no account name.
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.889\n", b"RFB 003.008\n");
        stream.write_all(&[2, 30, 35]).unwrap();
        client_hung_up(&mut stream)
    });
    let auth = VncAuth {
        username: None,
        password: Some(zeroize::Zeroizing::new("pw".to_owned())),
    };
    let error = connect_to(port, auth, events).err().unwrap();
    assert!(server.join().unwrap());
    assert!(
        matches!(
            &error,
            DriverError::Credentials {
                retry: false,
                needs_username: true,
                ..
            }
        ),
        "{error:?}"
    );

    // A 3.3 server that decided on VNC authentication.
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.003\n", b"RFB 003.003\n");
        stream.write_all(&2u32.to_be_bytes()).unwrap();
        client_hung_up(&mut stream)
    });
    let error = connect_to(port, VncAuth::default(), events).err().unwrap();
    assert!(server.join().unwrap());
    assert!(
        matches!(&error, DriverError::Credentials { retry: false, .. }),
        "{error:?}"
    );
}

/// TigerVNC with `SecurityTypes=None,VncAuth` and no password stored: asking for one would ask
/// forever (an empty one counts as none), so the server is reached without.
#[test]
fn a_server_that_also_allows_no_authentication_is_reached_without_a_password() {
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.008\n", b"RFB 003.008\n");
        assert_eq!(offer(&mut stream, &[2, 1]), 1);
        security_ok(&mut stream);
        server_init(&mut stream, 8, 8, "tiger");
        expect_setup(&mut stream, 8, 8);
    });
    let auth = VncAuth {
        username: None,
        password: Some(zeroize::Zeroizing::new(String::new())),
    };
    let driver = connect_to(port, auth, events).unwrap();
    server.join().unwrap();
    assert_eq!(driver.info().label, "tiger");
}

#[test]
fn a_refused_password_is_a_retry_carrying_the_servers_reason() {
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.008\n", b"RFB 003.008\n");
        assert_eq!(offer(&mut stream, &[2]), 2);
        stream.write_all(&[9u8; 16]).unwrap();
        read_n(&mut stream, 16);
        security_failed(&mut stream, "Authentication failed");
    });
    let auth = VncAuth {
        username: None,
        password: Some(zeroize::Zeroizing::new("wrong".to_owned())),
    };
    let error = connect_to(port, auth, events).err().unwrap();
    server.join().unwrap();
    match error {
        DriverError::Credentials {
            retry: true,
            needs_username: false,
            message,
        } => assert!(message.contains("Authentication failed"), "{message}"),
        other => panic!("expected a credentials retry, got {other:?}"),
    }
}

#[test]
fn unusable_security_types_are_listed_by_name() {
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        greet(&mut stream, b"RFB 003.008\n", b"RFB 003.008\n");
        stream.write_all(&[2, 19, 16]).unwrap();
        client_hung_up(&mut stream)
    });
    let error = connect_to(port, VncAuth::default(), events).err().unwrap();
    assert!(server.join().unwrap());
    let DriverError::Failed(message) = error else {
        panic!("expected a failure, got {error:?}");
    };
    assert!(message.contains("19 (VeNCrypt), 16 (Tight)"), "{message}");
}

#[test]
fn a_port_that_does_not_speak_rfb_is_named_as_such() {
    let (events, _seen) = recorder();
    let (port, server) = serve(|mut stream| {
        stream.write_all(b"HTTP/1.1 400").unwrap();
        client_hung_up(&mut stream)
    });
    let error = connect_to(port, VncAuth::default(), events).err().unwrap();
    server.join().unwrap();
    assert!(
        matches!(&error, DriverError::Failed(message) if message.contains("not with an RFB")),
        "{error:?}"
    );
}

#[test]
fn a_silent_server_runs_out_the_handshake_deadline() {
    let (events, _seen) = recorder();
    let (port, server) = serve(|stream| {
        thread::sleep(Duration::from_secs(2));
        drop(stream);
    });
    let started = Instant::now();
    let error = connect_within(
        &LocalConnector::new(),
        port,
        &VncAuth::default(),
        events,
        Duration::from_millis(300),
    )
    .err()
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert!(
        matches!(&error, DriverError::Disconnected(message) if message.contains("did not finish the handshake")),
        "{error:?}"
    );
    server.join().unwrap();
}

/// What the screen should look like, painted alongside the server.
struct Picture {
    width: u32,
    pixels: Vec<[u8; 4]>,
}

impl Picture {
    fn paint(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        colour: impl Fn(u32, u32) -> [u8; 4],
    ) {
        for row in y..y + height {
            for column in x..x + width {
                self.pixels[(row * self.width + column) as usize] = colour(column, row);
            }
        }
    }

    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.pixels[(y * self.width + x) as usize]
    }

    fn matches(&self, frame: &Frame) {
        assert_eq!(frame.width, self.width);
        for (index, expected) in self.pixels.iter().enumerate() {
            let (x, y) = (index as u32 % self.width, index as u32 / self.width);
            assert_eq!(pixel(frame, x, y), *expected, "pixel {x},{y}");
        }
    }
}

fn gradient(x: u32, y: u32) -> [u8; 4] {
    [(x * 2) as u8, (y * 3) as u8, (x + y) as u8, 255]
}

#[test]
fn raw_copyrect_hextile_and_zrle_rectangles_reach_the_frame() {
    let (driver, seen, mut stream) = desktop(96, 80);
    let mut picture = Picture {
        width: 96,
        pixels: vec![[0, 0, 0, 255]; 96 * 80],
    };

    apply(&mut stream, &seen, &update(&[raw(0, 0, 96, 80, gradient)]));
    picture.paint(0, 0, 96, 80, gradient);
    picture.matches(&driver.frame(false).unwrap());

    // CopyRect: the top-left 16x16 block to 40,40.
    let mut copy = rectangle(40, 40, 16, 16, 1);
    copy.extend_from_slice(&[0, 0, 0, 0]);
    apply(&mut stream, &seen, &update(&[copy]));
    let source = picture.pixels.clone();
    picture.paint(40, 40, 16, 16, |x, y| {
        source[((y - 40) * 96 + (x - 40)) as usize]
    });
    picture.matches(&driver.frame(false).unwrap());

    // Hextile at 64,0, 32x16: a red tile with a green subrect, then a tile that keeps
    // the red background and adds a blue coloured subrect.
    let mut hextile = rectangle(64, 0, 32, 16, 5);
    hextile.extend_from_slice(&[
        2 | 4 | 8,
        255,
        0,
        0,
        0,
        0,
        255,
        0,
        0,
        1,
        (2 << 4) | 3,
        (3 << 4) | 4,
    ]);
    hextile.extend_from_slice(&[8 | 16, 1, 0, 0, 255, 0, 0, 15 << 4]);
    apply(&mut stream, &seen, &update(&[hextile]));
    picture.paint(64, 0, 32, 16, |_, _| RED);
    picture.paint(66, 3, 4, 5, |_, _| GREEN);
    picture.paint(80, 0, 16, 1, |_, _| BLUE);
    picture.matches(&driver.frame(false).unwrap());

    // ZRLE, twice through one zlib stream: a solid tile and a raw tile at 0,64, then
    // the same bytes at 0,48, which deflate sends as references into the first.
    let mut tiles = vec![1, 0, 0, 255, 0];
    for row in 0..16u32 {
        for column in 0..32u32 {
            tiles.extend_from_slice(&[column as u8 * 8, row as u8 * 16, 99]);
        }
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let mut zrle_rect = |y: u16| {
        encoder.write_all(&tiles).unwrap();
        encoder.flush().unwrap();
        let data = std::mem::take(encoder.get_mut());
        let mut bytes = rectangle(0, y, 96, 16, 16);
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&data);
        bytes
    };
    let first = zrle_rect(64);
    let second = zrle_rect(48);
    apply(&mut stream, &seen, &update(&[first]));
    apply(&mut stream, &seen, &update(&[second]));
    for top in [48, 64] {
        picture.paint(0, top, 64, 16, |_, _| BLUE);
        picture.paint(64, top, 32, 16, |x, y| {
            [((x - 64) * 8) as u8, ((y - top) * 16) as u8, 99, 255]
        });
    }
    let frame = driver.frame(false).unwrap();
    picture.matches(&frame);
    assert_eq!(picture.at(70, 50), pixel(&frame, 70, 50));
    assert_eq!(frame.seq, 5);
    // A frame asked for again without a change is the same buffer, not a copy.
    assert!(Arc::ptr_eq(&frame.rgba, &driver.frame(false).unwrap().rgba));
}

#[test]
fn desktop_size_and_extended_desktop_size_resize_the_screen() {
    let (driver, seen, mut stream) = desktop(64, 48);
    apply(
        &mut stream,
        &seen,
        &update(&[raw(0, 0, 64, 48, |_, _| RED)]),
    );
    assert_eq!(client_message(&mut stream), request(true, 64, 48));

    apply(
        &mut stream,
        &seen,
        &update(&[rectangle(0, 0, 32, 16, -223)]),
    );
    assert!(lock(&seen).contains(&DriverEvent::Resized {
        width: 32,
        height: 16
    }));
    assert_eq!((driver.info().width, driver.info().height), (32, 16));
    // After a resize the next request asks for the whole new screen.
    assert_eq!(client_message(&mut stream), request(false, 32, 16));
    let frame = driver.frame(false).unwrap();
    assert_eq!((frame.width, frame.height), (32, 16));
    assert_eq!(pixel(&frame, 31, 15), RED);

    let mut extended = rectangle(0, 0, 40, 20, -308);
    extended.extend_from_slice(&[1, 0, 0, 0]);
    extended.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 40, 0, 20, 0, 0, 0, 0]);
    apply(
        &mut stream,
        &seen,
        &update(&[extended, raw(32, 0, 8, 20, |_, _| BLUE)]),
    );
    assert!(lock(&seen).contains(&DriverEvent::Resized {
        width: 40,
        height: 20
    }));
    assert_eq!(client_message(&mut stream), request(false, 40, 20));
    let frame = driver.frame(false).unwrap();
    assert_eq!(pixel(&frame, 0, 0), RED);
    assert_eq!(pixel(&frame, 0, 19), [0, 0, 0, 255]);
    assert_eq!(pixel(&frame, 39, 19), BLUE);

    // A refused SetDesktopSize (reason 1, status 3) changes nothing.
    let mut refused = rectangle(1, 3, 10, 10, -308);
    refused.extend_from_slice(&[0, 0, 0, 0]);
    apply(&mut stream, &seen, &update(&[refused]));
    assert_eq!((driver.info().width, driver.info().height), (40, 20));
    assert_eq!(client_message(&mut stream), request(true, 40, 20));
    assert_eq!(
        count(&seen, |event| matches!(event, DriverEvent::Resized { .. })),
        2
    );
    assert!(logs(&seen)
        .iter()
        .any(|line| line == "screen resized to 40x20"));
}

#[test]
fn a_fresh_frame_waits_for_an_update_begun_after_the_call_and_for_quiet() {
    let (driver, seen, mut stream) = desktop(8, 8);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    assert_eq!(client_message(&mut stream), request(true, 8, 8));

    let asking = driver.clone();
    let fresh = thread::spawn(move || {
        let started = Instant::now();
        (asking.frame(true).unwrap(), started.elapsed())
    });
    // The incremental request for any change, and the one-pixel request answered at once.
    assert_eq!(client_message(&mut stream), request(true, 8, 8));
    assert_eq!(client_message(&mut stream), [3, 0, 0, 0, 0, 0, 0, 1, 0, 1]);
    thread::sleep(Duration::from_millis(100));
    stream
        .write_all(&update(&[raw(0, 0, 8, 8, |_, _| BLUE)]))
        .unwrap();
    // A second change inside the quiet window moves the wait on.
    thread::sleep(Duration::from_millis(120));
    stream
        .write_all(&update(&[raw(0, 0, 8, 8, |_, _| GREEN)]))
        .unwrap();
    let (frame, elapsed) = fresh.join().unwrap();
    assert_eq!(pixel(&frame, 4, 4), GREEN);
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    assert!(elapsed < FRESH_LIMIT, "{elapsed:?}");
}

#[test]
fn a_fresh_frame_of_a_server_that_never_answers_returns_by_the_limit() {
    let (driver, seen, mut stream) = desktop(8, 8);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    let started = Instant::now();
    let frame = driver.frame(true).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(pixel(&frame, 0, 0), RED);
    assert!(
        elapsed >= FRESH_LIMIT - Duration::from_millis(50),
        "{elapsed:?}"
    );
    assert!(
        elapsed < FRESH_LIMIT + Duration::from_millis(500),
        "{elapsed:?}"
    );
}

#[test]
fn pointer_keys_and_text_go_out_as_rfb_messages() {
    let (driver, seen, mut stream) = desktop(100, 50);

    driver.pointer(10, 20, buttons::LEFT).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 1, 0, 10, 0, 20]);
    // Off the screen is the screen's edge.
    driver.pointer(500, 70, buttons::WHEEL_DOWN).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 16, 0, 99, 0, 49]);

    driver.key(&Key::Named(NamedKey::Enter), None).unwrap();
    assert_eq!(next_input(&mut stream), key_message(0xff0d, true));
    assert_eq!(next_input(&mut stream), key_message(0xff0d, false));
    driver.key(&Key::Char('a'), Some(true)).unwrap();
    assert_eq!(next_input(&mut stream), key_message(0x61, true));
    driver.key(&Key::Char('a'), Some(false)).unwrap();
    assert_eq!(next_input(&mut stream), key_message(0x61, false));

    let error = driver.key(&Key::Named(NamedKey::Back), None).unwrap_err();
    assert!(
        matches!(&error, DriverError::Unsupported(message) if message.contains("Back")),
        "{error:?}"
    );

    driver
        .chord(&KeyChord {
            modifiers: vec![Key::Named(NamedKey::Control)],
            key: Key::Char('c'),
        })
        .unwrap();
    for (symbol, down) in [(0xffe3, true), (0x63, true), (0x63, false), (0xffe3, false)] {
        assert_eq!(next_input(&mut stream), key_message(symbol, down));
    }

    driver.text("Hi!\r\n中\t").unwrap();
    let expected = [
        (0xffe1, true),
        (0x48, true),
        (0x48, false),
        (0xffe1, false),
        (0x69, true),
        (0x69, false),
        (0xffe1, true),
        (0x21, true),
        (0x21, false),
        (0xffe1, false),
        (0xff0d, true),
        (0xff0d, false),
        (0x0100_4e2d, true),
        (0x0100_4e2d, false),
        (0xff09, true),
        (0xff09, false),
    ];
    for (symbol, down) in expected {
        assert_eq!(next_input(&mut stream), key_message(symbol, down));
    }
    driver.text("文").unwrap();
    for down in [true, false] {
        assert_eq!(next_input(&mut stream), key_message(0x0100_6587, down));
    }
    let notes = logs(&seen)
        .into_iter()
        .filter(|line| line.contains("Unicode keysyms"))
        .count();
    assert_eq!(notes, 1);

    // A control character with no key types nothing at all.
    let error = driver.text("ab\u{7}").unwrap_err();
    assert!(error.message().contains("U+0007"), "{error:?}");
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);
}

/// QEMU reads keysyms in sixteen bits: `（` would arrive as BackSpace and `中` as nothing at all.
/// Text with any character beyond Latin-1 is refused whole, before a key is sent.
#[test]
fn qemu_is_never_sent_a_character_beyond_latin_1() {
    let (driver, _seen, mut stream) = desktop_named(100, 50, "QEMU (debian)");
    let error = driver.text("ab（c").unwrap_err();
    assert!(
        matches!(&error, DriverError::Unsupported(message)
            if message.contains("QEMU") && message.contains("U+FF08") && message.contains("control key")),
        "{error:?}"
    );
    let error = driver.check_text("中文").unwrap_err();
    assert!(error.message().contains("U+4E2D"), "{error:?}");
    assert!(driver.key(&Key::Char('中'), None).is_err());
    assert!(driver.check_text("Grüße, é!").is_ok());
    // Nothing went out for any of that: the next message is this pointer event.
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);
    driver.text("é").unwrap();
    for down in [true, false] {
        assert_eq!(next_input(&mut stream), key_message(0xe9, down));
    }
}

#[test]
fn a_rectangle_outside_the_screen_ends_the_connection_with_the_reason() {
    let (driver, seen, mut stream) = desktop(16, 16);
    stream
        .write_all(&update(&[rectangle(10, 10, 8, 8, 0)]))
        .unwrap();
    wait_for(&seen, "the disconnect", |events| {
        events
            .iter()
            .any(|event| matches!(event, DriverEvent::Disconnected(_)))
    });
    let reason = match lock(&seen)
        .iter()
        .find(|event| matches!(event, DriverEvent::Disconnected(_)))
    {
        Some(DriverEvent::Disconnected(reason)) => reason.clone(),
        _ => unreachable!(),
    };
    assert!(
        reason.contains("(10,10 8x8) outside its 16x16 screen"),
        "{reason}"
    );
    assert_eq!(
        driver.frame(false).unwrap_err(),
        DriverError::Disconnected(reason.clone())
    );
    assert_eq!(
        driver.pointer(0, 0, 0).unwrap_err(),
        DriverError::Disconnected(reason)
    );
    assert!(client_hung_up(&mut stream));
}

#[test]
fn a_server_hang_up_and_a_close_each_end_the_connection_once() {
    let (driver, seen, stream) = desktop(8, 8);
    drop(stream);
    wait_for(&seen, "the disconnect", |events| {
        events
            .iter()
            .any(|event| matches!(event, DriverEvent::Disconnected(_)))
    });
    driver.close();
    thread::sleep(Duration::from_millis(300));
    let ends: Vec<DriverEvent> = lock(&seen)
        .iter()
        .filter(|event| matches!(event, DriverEvent::Disconnected(_)))
        .cloned()
        .collect();
    assert_eq!(
        ends,
        vec![DriverEvent::Disconnected(
            "the VNC server closed the connection".to_owned()
        )]
    );

    let (driver, seen, mut stream) = desktop(8, 8);
    driver.close();
    wait_for(&seen, "the disconnect", |events| {
        events.contains(&DriverEvent::Disconnected(
            "the VNC connection was closed".to_owned(),
        ))
    });
    assert!(client_hung_up(&mut stream));
    assert!(matches!(
        driver.key(&Key::Char('x'), None),
        Err(DriverError::Disconnected(_))
    ));
    assert!(matches!(
        driver.frame(true),
        Err(DriverError::Disconnected(_))
    ));
}

#[test]
fn dropping_the_driver_closes_the_connection() {
    let (driver, seen, mut stream) = desktop(8, 8);
    drop(driver);
    assert!(client_hung_up(&mut stream));
    wait_for(&seen, "the disconnect", |events| {
        events
            .iter()
            .any(|event| matches!(event, DriverEvent::Disconnected(_)))
    });
}

#[test]
fn bells_clipboards_and_colour_maps_are_read_past() {
    let (driver, seen, mut stream) = desktop(4, 4);
    let mut messages = vec![2];
    messages.extend_from_slice(&[3, 0, 0, 0, 0, 0, 0, 5]);
    messages.extend_from_slice(b"hello");
    messages.extend_from_slice(&[1, 0, 0, 0, 0, 2]);
    messages.extend_from_slice(&[0; 12]);
    messages.extend(update(&[raw(0, 0, 4, 4, |_, _| GREEN)]));
    apply(&mut stream, &seen, &messages);
    assert_eq!(pixel(&driver.frame(false).unwrap(), 3, 3), GREEN);
    // The clipboard is kept, not just read past.
    let desktop = driver.desktop().unwrap();
    assert_eq!(
        desktop.clipboard(Duration::ZERO).unwrap().unwrap().text,
        "hello"
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A ServerCutText of the Extended Clipboard: its flags and what follows them.
fn extended(flags: u32, rest: &[u8]) -> Vec<u8> {
    let mut message = vec![3, 0, 0, 0];
    message.extend_from_slice(&(-((4 + rest.len()) as i32)).to_be_bytes());
    message.extend_from_slice(&flags.to_be_bytes());
    message.extend_from_slice(rest);
    message
}

/// TigerVNC's caps (and neatvnc's): text, with caps, request, peek, notify and provide,
/// and no text sent unasked.
fn tiger_caps() -> Vec<u8> {
    extended(0x1f00_0001, &0u32.to_be_bytes())
}

const CLIENT_CAPS: &str = "06 00 00 00 FF FF FF F8 1F 00 00 01 00 00 00 00";
const CLIENT_NOTIFY: &str = "06 00 00 00 FF FF FF FC 08 00 00 01";
const CLIENT_REQUEST: &str = "06 00 00 00 FF FF FF FC 02 00 00 01";

/// A size-prefixed text payload of a provide.
fn text_payload(bytes: &[u8]) -> Vec<u8> {
    let mut payload = (bytes.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(bytes);
    payload
}

/// A provide of `payload`, compressed as TigerVNC does (a sync flush, no end of stream) or,
/// `finished`, as QEMU does.
fn provide(payload: &[u8], finished: bool) -> Vec<u8> {
    let mut compress = Compress::new(Compression::default(), true);
    let mut compressed = Vec::with_capacity(payload.len() * 2 + 1024);
    let flush = if finished {
        FlushCompress::Finish
    } else {
        FlushCompress::Sync
    };
    compress
        .compress_vec(payload, &mut compressed, flush)
        .unwrap();
    extended(0x1000_0001, &compressed)
}

/// A desktop whose server announced its Extended Clipboard with `caps`, got the client's caps
/// back, and sent its first update.
fn extended_desktop(caps: Vec<u8>) -> (Arc<dyn Driver>, Seen, TcpStream) {
    let (driver, seen, mut stream) = desktop(8, 8);
    stream.write_all(&caps).unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_CAPS);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    (driver, seen, stream)
}

/// Reads the clipboard on a thread of its own, as the server answers it on this one.
fn read_clipboard(
    driver: &Arc<dyn Driver>,
    timeout: Duration,
) -> thread::JoinHandle<DriverResult<Option<ClipboardText>>> {
    let reading = driver.clone();
    thread::spawn(move || reading.desktop().unwrap().clipboard(timeout))
}

/// The client answers each caps message with its own, once: TigerVNC takes a client that never
/// does for one that takes no clipboard actions.
#[test]
fn the_servers_caps_are_answered_with_the_clients_once_each() {
    let (driver, seen, mut stream) = desktop(8, 8);
    for _ in 0..2 {
        stream.write_all(&tiger_caps()).unwrap();
        assert_eq!(hex(&next_input(&mut stream)), CLIENT_CAPS);
    }
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    assert_eq!(
        driver.desktop().unwrap().clipboard_channel(),
        ClipboardChannel::Extended
    );
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);
    let lines = logs(&seen);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("Extended Clipboard"))
            .count(),
        1,
        "{lines:?}"
    );

    // Caps without the text format are answered too, and leave the classic channel.
    let (driver, seen, mut stream) = desktop(8, 8);
    stream
        .write_all(&extended(0x1f00_0002, &0u32.to_be_bytes()))
        .unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_CAPS);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    assert_eq!(
        driver.desktop().unwrap().clipboard_channel(),
        ClipboardChannel::Latin1
    );
    // And a server that never sends caps is a classic one.
    let (driver, seen, mut stream) = desktop(8, 8);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    assert_eq!(
        driver.desktop().unwrap().clipboard_channel(),
        ClipboardChannel::Latin1
    );
}

/// research-rfb §2.2 and §7.2: a notify, then the text in one finished zlib stream as UTF-8 with
/// CRLF line ends and a counted NUL; the server's later request is answered with the same text,
/// its peek with a notify, and a read is that text without asking a server that holds it.
#[test]
fn an_extended_write_notifies_then_provides_and_answers_the_servers_requests() {
    let (driver, seen, mut stream) = extended_desktop(tiger_caps());
    let desktop = driver.desktop().unwrap();
    // Before anything was written, a request and a peek go unanswered (the update after them
    // shows they were read).
    let mut messages = extended(0x0200_0001, &[]);
    messages.extend(extended(0x0400_0000, &[]));
    messages.extend(update(&[raw(0, 0, 8, 8, |_, _| GREEN)]));
    apply(&mut stream, &seen, &messages);

    desktop.set_clipboard("héllo\nwörld\r\n中").unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_NOTIFY);
    let provided = next_input(&mut stream);
    assert_eq!(hex(&provided[..4]), "06 00 00 00");
    let length = i32::from_be_bytes(provided[4..8].try_into().unwrap());
    assert_eq!(length, -((provided.len() - 8) as i32));
    assert_eq!(hex(&provided[8..12]), "10 00 00 01");
    let mut inflate = Decompress::new(true);
    let mut payload = Vec::with_capacity(256);
    assert_eq!(
        inflate
            .decompress_vec(&provided[12..], &mut payload, FlushDecompress::Finish)
            .unwrap(),
        Status::StreamEnd,
        "one finished zlib stream"
    );
    assert_eq!(
        payload,
        text_payload("héllo\r\nwörld\r\n中\0".as_bytes()),
        "UTF-8, CRLF, the NUL counted in the size"
    );

    // QEMU asks when the guest pastes: the same text again.
    stream.write_all(&extended(0x0200_0001, &[])).unwrap();
    assert_eq!(next_input(&mut stream), provided);
    stream.write_all(&extended(0x0400_0000, &[])).unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_NOTIFY);
    let read = desktop.clipboard(Duration::from_secs(2)).unwrap().unwrap();
    assert_eq!(read.text, "héllo\nwörld\n中");
    // Nothing else went: no request for the read, no answer to the first request and peek.
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);
}

/// libvncserver's caps take no notify: only the provide goes. QEMU's take no peek, which this
/// client never sends.
#[test]
fn an_extended_write_sends_only_what_the_servers_caps_take() {
    let (driver, _seen, mut stream) =
        extended_desktop(extended(0x1700_0001, &0x0010_0000u32.to_be_bytes()));
    driver.desktop().unwrap().set_clipboard("x").unwrap();
    let message = next_input(&mut stream);
    assert_eq!(
        hex(&message[8..12]),
        "10 00 00 01",
        "a provide, no notify first"
    );

    let (driver, _seen, mut stream) = extended_desktop(extended(0x1b00_0001, &0u32.to_be_bytes()));
    driver.desktop().unwrap().set_clipboard("x").unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_NOTIFY);
    assert_eq!(hex(&next_input(&mut stream)[8..12]), "10 00 00 01");
}

/// A read asks for the server's text and waits for the provide, decoded however the server
/// compressed and ended it.
#[test]
fn a_read_asks_for_the_servers_text_and_waits_for_it() {
    let (driver, _seen, mut stream) = extended_desktop(tiger_caps());

    // TigerVNC: a sync-flushed stream that never ends; CRLF and the NUL.
    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream
        .write_all(&provide(
            &text_payload("tiger\r\nvnc 中\0".as_bytes()),
            false,
        ))
        .unwrap();
    let text = read.join().unwrap().unwrap().unwrap();
    assert_eq!(text.text, "tiger\nvnc 中");
    assert!(text.received.elapsed() < Duration::from_secs(5));

    // QEMU: a finished stream, and no NUL.
    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream
        .write_all(&provide(&text_payload(b"qemu\r\nguest"), true))
        .unwrap();
    assert_eq!(read.join().unwrap().unwrap().unwrap().text, "qemu\nguest");

    // A change the server announces while a read waits is asked for again.
    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream.write_all(&extended(0x0800_0001, &[])).unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream
        .write_all(&provide(&text_payload(b"newer\0"), false))
        .unwrap();
    assert_eq!(read.join().unwrap().unwrap().unwrap().text, "newer");

    // A server that says its clipboard holds no text ends the wait: empty.
    let started = Instant::now();
    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream.write_all(&extended(0x0800_0000, &[])).unwrap();
    assert_eq!(read.join().unwrap().unwrap(), None);
    assert!(started.elapsed() < Duration::from_secs(4));
}

/// A server that does not answer (TigerVNC with no selection owner, QEMU without spice-vdagent in
/// the guest) leaves the last text it sent, if any, once the read's time is up; without one the
/// read says the server did not answer, never that the clipboard is empty (`None`).
#[test]
fn a_read_the_server_does_not_answer_ends_at_its_timeout() {
    let (driver, _seen, mut stream) = extended_desktop(tiger_caps());
    let started = Instant::now();
    let read = read_clipboard(&driver, Duration::from_millis(300));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    let error = read.join().unwrap().unwrap_err();
    assert!(
        matches!(&error, DriverError::Unsupported(message)
            if message.starts_with("this VNC server did not answer within 300 ms when asked for its clipboard.")
                && message.contains("spice-vdagent")),
        "{error:?}"
    );
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");

    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream
        .write_all(&provide(&text_payload(b"kept\0"), false))
        .unwrap();
    let kept = read.join().unwrap().unwrap().unwrap();
    let read = read_clipboard(&driver, Duration::from_millis(200));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    assert_eq!(read.join().unwrap().unwrap(), Some(kept));

    // An empty text it provided before is no answer to this request either.
    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    stream
        .write_all(&provide(&text_payload(b"\0"), false))
        .unwrap();
    assert_eq!(read.join().unwrap().unwrap().unwrap().text, "");
    let read = read_clipboard(&driver, Duration::from_secs(1));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    let error = read.join().unwrap().unwrap_err();
    assert!(
        matches!(&error, DriverError::Unsupported(message) if message.contains("did not answer within 1 s")),
        "{error:?}"
    );
}

/// A server whose caps take no requests is never asked: a read gives what it sent unasked, and
/// before anything says so at once, never that the clipboard is empty.
#[test]
fn a_read_of_a_server_that_takes_no_requests_says_so() {
    // Caps, peek, notify and provide; no request.
    let (driver, seen, mut stream) = extended_desktop(extended(0x1d00_0001, &0u32.to_be_bytes()));
    let started = Instant::now();
    let error = read_clipboard(&driver, Duration::from_secs(5))
        .join()
        .unwrap()
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        matches!(&error, DriverError::Unsupported(message)
            if message.starts_with("this VNC server's Extended Clipboard takes no requests")),
        "{error:?}"
    );
    let mut messages = provide(&text_payload(b"unasked\0"), true);
    messages.extend(update(&[raw(0, 0, 8, 8, |_, _| GREEN)]));
    apply(&mut stream, &seen, &messages);
    let read = read_clipboard(&driver, Duration::from_secs(5));
    assert_eq!(read.join().unwrap().unwrap().unwrap().text, "unasked");
    // Nothing was asked: the next message is the pointer's.
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);
}

/// Text above what servers are sure to take, counted as it would go (CRLF line ends and the NUL
/// for the Extended Clipboard), is refused before anything is sent and is not kept to answer the
/// server's requests; up to it, it goes. The tools' longest text, 60,000 characters of 4 bytes,
/// goes.
#[test]
fn an_extended_write_refuses_more_than_servers_take_before_sending() {
    let (driver, _seen, mut stream) = extended_desktop(tiger_caps());
    let desktop = driver.desktop().unwrap();
    let refused = |text: String| {
        let error = desktop.set_clipboard(&text).unwrap_err();
        match error {
            DriverError::Unsupported(message) => message,
            other => panic!("{other:?}"),
        }
    };
    let message = refused("😀".repeat(70_000));
    assert_eq!(
        message,
        format!("the text is 280001 bytes as UTF-8 with CRLF line ends, more than the {MAX_SENT_CLIPBOARD} a VNC server is sure to take (TigerVNC drops clipboard text above 256 KiB without a word); nothing was put on its clipboard. Split the text into smaller parts")
    );
    // The NUL counts, and so does each line end that goes as CRLF.
    assert!(refused("a".repeat(MAX_SENT_CLIPBOARD))
        .contains(&format!("is {} bytes", MAX_SENT_CLIPBOARD + 1)));
    assert!(refused("\n".repeat(MAX_SENT_CLIPBOARD / 2))
        .contains(&format!("is {} bytes", MAX_SENT_CLIPBOARD + 1)));
    // Nothing went, and nothing is kept: a request is answered with nothing, a read asks.
    stream.write_all(&extended(0x0200_0001, &[])).unwrap();
    let read = read_clipboard(&driver, Duration::from_millis(200));
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_REQUEST);
    assert!(read.join().unwrap().is_err());

    let edge = "a".repeat(MAX_SENT_CLIPBOARD - 1);
    desktop.set_clipboard(&edge).unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_NOTIFY);
    let provided = next_input(&mut stream);
    assert_eq!(hex(&provided[8..12]), "10 00 00 01");
    let mut inflate = Decompress::new(true);
    let mut payload = Vec::with_capacity(MAX_SENT_CLIPBOARD + 64);
    inflate
        .decompress_vec(&provided[12..], &mut payload, FlushDecompress::Finish)
        .unwrap();
    assert_eq!(payload.len(), 4 + MAX_SENT_CLIPBOARD);
    assert_eq!(payload[..4], (MAX_SENT_CLIPBOARD as u32).to_be_bytes());

    desktop.set_clipboard(&"😀".repeat(60_000)).unwrap();
    assert_eq!(hex(&next_input(&mut stream)), CLIENT_NOTIFY);
    assert_eq!(hex(&next_input(&mut stream)[8..12]), "10 00 00 01");
}

/// Classic cut text counts its Latin-1 bytes after CRLF becomes LF: one a byte, whatever UTF-8
/// would make of it.
#[test]
fn a_latin1_write_refuses_more_than_servers_take_before_sending() {
    let (driver, seen, mut stream) = desktop(8, 8);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    let error = driver
        .desktop()
        .unwrap()
        .set_clipboard(&"é".repeat(MAX_SENT_CLIPBOARD + 1))
        .unwrap_err();
    assert!(
        matches!(&error, DriverError::Unsupported(message)
            if message.starts_with(&format!("the text is {} bytes in Latin-1,", MAX_SENT_CLIPBOARD + 1))
                && message.contains("Split the text")),
        "{error:?}"
    );
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);

    // Sent from a thread of its own: the message is larger than a socket's buffer.
    let writing = driver.clone();
    let wrote = thread::spawn(move || {
        let desktop = writing.desktop().unwrap();
        desktop.set_clipboard(&"é".repeat(MAX_SENT_CLIPBOARD))?;
        desktop.set_clipboard(&"\r\n".repeat(MAX_SENT_CLIPBOARD / 2 + 1))
    });
    let message = next_input(&mut stream);
    assert_eq!(message.len(), 8 + MAX_SENT_CLIPBOARD);
    assert!(message[8..].iter().all(|byte| *byte == 0xe9));
    let message = next_input(&mut stream);
    assert_eq!(message.len(), 8 + MAX_SENT_CLIPBOARD / 2 + 1);
    wrote.join().unwrap().unwrap();
}

/// Classic cut text is kept as the last text the server sent, with when it came, cut at 1 MiB;
/// on that channel a read asks nothing.
#[test]
fn classic_cut_text_is_kept_with_its_time() {
    let (driver, seen, mut stream) = desktop(8, 8);
    let desktop = driver.desktop().unwrap();
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    assert_eq!(desktop.clipboard(Duration::from_secs(2)).unwrap(), None);

    let mut messages = vec![3, 0, 0, 0, 0, 0, 0, 7];
    messages.extend_from_slice(&[b'c', b'a', b'f', 0xe9, b'\r', b'\n', b'!']);
    messages.extend(update(&[raw(0, 0, 8, 8, |_, _| GREEN)]));
    apply(&mut stream, &seen, &messages);
    assert_eq!(desktop.clipboard_channel(), ClipboardChannel::Latin1);
    let text = desktop.clipboard(Duration::from_secs(2)).unwrap().unwrap();
    assert_eq!(text.text, "café\n!");
    assert!(text.received.elapsed() < Duration::from_secs(5));

    let size = MAX_KEPT_CLIPBOARD + 10;
    let mut messages = vec![3, 0, 0, 0];
    messages.extend_from_slice(&(size as u32).to_be_bytes());
    messages.extend(std::iter::repeat(b'a').take(size));
    messages.extend(update(&[raw(0, 0, 8, 8, |_, _| BLUE)]));
    apply(&mut stream, &seen, &messages);
    let text = desktop.clipboard(Duration::from_secs(2)).unwrap().unwrap();
    assert_eq!(text.text.len(), MAX_KEPT_CLIPBOARD);
    // The rest was read past: the update after it arrived whole.
    assert_eq!(pixel(&driver.frame(false).unwrap(), 7, 7), BLUE);
    driver.pointer(1, 2, 0).unwrap();
    assert_eq!(next_input(&mut stream), vec![5, 0, 0, 1, 0, 2]);
}

/// An absurd length still ends the connection, extended as classic.
#[test]
fn an_absurd_clipboard_length_ends_the_connection() {
    let (_driver, seen, mut stream) = desktop(8, 8);
    stream.write_all(&[3, 0, 0, 0, 0x80, 0, 0, 0]).unwrap();
    wait_for(&seen, "the disconnect", |events| {
        events.iter().any(|event| {
            matches!(event, DriverEvent::Disconnected(reason) if reason.contains("more than this client accepts"))
        })
    });
}

/// Text Latin-1 cannot carry is refused before anything is sent; the rest goes as classic cut
/// text with LF line ends.
#[test]
fn a_latin1_write_refuses_other_characters_before_sending() {
    let (driver, seen, mut stream) = desktop(8, 8);
    apply(&mut stream, &seen, &update(&[raw(0, 0, 8, 8, |_, _| RED)]));
    let desktop = driver.desktop().unwrap();
    let error = desktop.set_clipboard("ok 中").unwrap_err();
    assert!(
        matches!(&error, DriverError::Unsupported(message) if message.contains("U+4E2D") && message.contains("Latin-1")),
        "{error:?}"
    );
    desktop.set_clipboard("café\r\nx").unwrap();
    assert_eq!(
        next_input(&mut stream),
        vec![6, 0, 0, 0, 0, 0, 0, 6, b'c', b'a', b'f', 0xe9, b'\n', b'x']
    );
}

/// A server that offered nothing for the first update: the channel waits about a second for
/// it, then is the classic one.
#[test]
fn a_clipboard_call_waits_only_briefly_for_a_server_that_never_updates() {
    let (driver, _seen, _stream) = desktop(8, 8);
    let started = Instant::now();
    assert_eq!(
        driver.desktop().unwrap().clipboard_channel(),
        ClipboardChannel::Latin1
    );
    let waited = started.elapsed();
    assert!(
        waited >= CAPS_WAIT - Duration::from_millis(50),
        "{waited:?}"
    );
    assert!(waited < CAPS_WAIT + Duration::from_secs(1), "{waited:?}");
}

/// A desktop whose server greets with `version` and offers `offered`, signed into without
/// authentication.
fn desktop_greeted(
    version: &'static [u8; 12],
    offered: &'static [u8],
) -> (Arc<dyn Driver>, Seen, TcpStream) {
    let (events, seen) = recorder();
    let (port, server) = serve(move |mut stream| {
        greet(&mut stream, version, b"RFB 003.008\n");
        assert_eq!(offer(&mut stream, offered), 1);
        security_ok(&mut stream);
        server_init(&mut stream, 8, 8, "Mac");
        expect_setup(&mut stream, 8, 8);
        stream
    });
    let driver = connect_to(port, VncAuth::default(), events).unwrap();
    (driver, seen, server.join().unwrap())
}

/// Apple's server is told by Apple Remote Desktop authentication offered, or by its own version
/// 003.889; there `alt` goes as Meta_L, which it maps to Option, and `cmd` stays Super_L
/// (research-rfb §4.4, unverified on a real Mac here).
#[test]
fn apples_server_is_told_apart_and_gets_alt_as_meta() {
    const ARD_OFFERED: &[u8] = &[30, 1];
    const NONE_ONLY: &[u8] = &[1];
    for (version, offered) in [
        (b"RFB 003.008\n", ARD_OFFERED),
        (b"RFB 003.889\n", NONE_ONLY),
    ] {
        let (driver, seen, mut stream) = desktop_greeted(version, offered);
        assert!(driver.desktop().unwrap().is_macos());
        assert!(
            logs(&seen)
                .iter()
                .any(|line| line.contains("alt is sent as Meta_L")),
            "{:?}",
            logs(&seen)
        );
        driver.key(&Key::Named(NamedKey::Alt), Some(true)).unwrap();
        assert_eq!(next_input(&mut stream), key_message(0xffe7, true));
        driver
            .chord(&KeyChord {
                modifiers: vec![Key::Named(NamedKey::Meta), Key::Named(NamedKey::Alt)],
                key: Key::Char('v'),
            })
            .unwrap();
        for (symbol, down) in [
            (0xffeb, true),
            (0xffe7, true),
            (0x76, true),
            (0x76, false),
            (0xffe7, false),
            (0xffeb, false),
        ] {
            assert_eq!(next_input(&mut stream), key_message(symbol, down));
        }
    }

    // Everyone else gets Alt_L.
    let (driver, seen, mut stream) = desktop(8, 8);
    assert!(!driver.desktop().unwrap().is_macos());
    assert!(logs(&seen).iter().all(|line| !line.contains("Meta_L")));
    driver.key(&Key::Named(NamedKey::Alt), None).unwrap();
    assert_eq!(next_input(&mut stream), key_message(0xffe9, true));
    assert_eq!(next_input(&mut stream), key_message(0xffe9, false));
}

/// Against a real server: QEMU's built-in VNC. Run with a QEMU started as
/// `qemu-system-x86_64 -nodefaults -vga std -display none -vnc 127.0.0.1:47`
/// and `MEWRK_E2E_QEMU_VNC_PORT=5947`; set `MEWRK_E2E_QEMU_VNC_PNG` to a path
/// to keep the frame as a picture.
#[test]
#[ignore = "needs a running QEMU with -vnc"]
fn a_real_qemu_server_paints_a_frame() {
    let Some(port) = std::env::var("MEWRK_E2E_QEMU_VNC_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
    else {
        eprintln!("MEWRK_E2E_QEMU_VNC_PORT is not set");
        return;
    };
    let (events, seen) = recorder();
    let driver = connect_to(port, VncAuth::default(), events).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let frame = loop {
        let frame = driver.frame(true).unwrap();
        let lit = frame
            .rgba
            .chunks(4)
            .filter(|pixel| pixel[..3] != [0, 0, 0])
            .count();
        if lit > 100 || Instant::now() > deadline {
            break frame;
        }
    };
    let lit = frame
        .rgba
        .chunks(4)
        .filter(|pixel| pixel[..3] != [0, 0, 0])
        .count();
    eprintln!(
        "{}x{} seq {} with {lit} lit pixels",
        frame.width, frame.height, frame.seq
    );
    for line in logs(&seen) {
        eprintln!("log: {line}");
    }
    if let Ok(path) = std::env::var("MEWRK_E2E_QEMU_VNC_PNG") {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder =
            png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&frame.rgba)
            .unwrap();
    }
    assert!(lit > 100, "the frame stayed blank");
    driver
        .pointer(frame.width / 2, frame.height / 2, 0)
        .unwrap();
    driver.key(&Key::Named(NamedKey::Shift), None).unwrap();
    driver.close();
}
