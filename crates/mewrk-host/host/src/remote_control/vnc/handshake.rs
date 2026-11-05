//! Opening an RFB connection: version, security, sign-in, and the server's
//! description of its screen.
//!
//! Servers in the wild speak several dialects of the same handshake. The
//! version decides three things — whether the server picks the security type
//! or offers a list, whether a failure comes with a reason, and whether a
//! connection without authentication still gets a SecurityResult — and every
//! other version a server announces (Apple's 003.889, RealVNC's 004.x and
//! 005.000) behaves as 3.8.
//!
//! The security type is chosen by this client's preference, not the server's
//! order: TigerVNC lists VeNCrypt first and macOS lists undocumented types
//! (33, 35, 36) after its own Apple Remote Desktop type, and taking the first
//! type both sides know would sign in differently depending on how a server
//! happens to be configured. Credentials are checked before the choice is
//! sent, so a missing password never costs the user a failed attempt on the
//! server's lockout counter.

use std::{
    io::{self, Read, Write},
    time::{Duration, Instant},
};

use aes::{
    cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit},
    Aes128,
};
use des::Des;
use md5::{Digest, Md5};
use num_bigint::BigUint;
use ring::rand::{SecureRandom, SystemRandom};
use zeroize::Zeroizing;

use super::{
    decode::{self, check_size},
    VncAuth,
};
use crate::remote_control::driver::{DriverError, DriverResult};

const NONE: u8 = 1;
const VNC_AUTH: u8 = 2;
const APPLE_REMOTE_DESKTOP: u8 = 30;

/// The longest failure reason read from a server; longer ones are cut.
const MAX_REASON: u32 = 64 * 1024;
/// The longest desktop name accepted at all, and the part of it kept.
const MAX_NAME: u32 = 64 * 1024;
const KEPT_NAME: usize = 4096;
/// The largest Diffie-Hellman modulus accepted for Apple Remote Desktop, in
/// bytes. macOS uses 128 (1024 bits); a larger one only makes the modular
/// exponentiation slower.
const MAX_ARD_KEY: usize = 512;

/// The handshake dialect a server speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V3_3,
    V3_7,
    V3_8,
}

impl Version {
    fn reply(self) -> &'static [u8; 12] {
        match self {
            Self::V3_3 => b"RFB 003.003\n",
            Self::V3_7 => b"RFB 003.007\n",
            Self::V3_8 => b"RFB 003.008\n",
        }
    }
}

/// What the server said about its screen once the client was in, and what
/// the handshake showed of the server itself.
#[derive(Debug)]
pub struct ServerInit {
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// Apple's server (macOS Screen Sharing): it offered Apple Remote Desktop
    /// authentication (type 30), or announced Apple's own RFB 003.889.
    pub apple: bool,
}

/// The handshake's own error, before it is phrased for the session: I/O is
/// told apart from a refusal so that a deadline reads as a deadline.
enum Failure {
    Io(io::Error),
    Driver(DriverError),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<DriverError> for Failure {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

/// Runs the whole handshake on `stream`, whose deadline the caller has set to
/// `timeout`. `log` gets one line per negotiated step, never a credential.
pub fn run<S: Read + Write>(
    stream: &mut S,
    auth: &VncAuth,
    port: u16,
    timeout: Duration,
    log: &dyn Fn(String),
) -> DriverResult<ServerInit> {
    let started = Instant::now();
    handshake(stream, auth, port, log).map_err(|failure| match failure {
        Failure::Driver(error) => error,
        Failure::Io(error) => {
            if error.kind() == io::ErrorKind::TimedOut || started.elapsed() >= timeout {
                DriverError::Disconnected(format!(
                    "the VNC server on port {port} did not finish the handshake within {} s",
                    timeout.as_secs()
                ))
            } else if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            ) {
                DriverError::Disconnected(format!(
                    "the VNC server on port {port} closed the connection during the handshake"
                ))
            } else {
                DriverError::Disconnected(format!(
                    "the connection to the VNC server on port {port} failed during the handshake: {error}"
                ))
            }
        }
    })
}

fn handshake<S: Read + Write>(
    stream: &mut S,
    auth: &VncAuth,
    port: u16,
    log: &dyn Fn(String),
) -> Result<ServerInit, Failure> {
    let mut greeting = [0u8; 12];
    stream.read_exact(&mut greeting)?;
    let version = parse_version(&greeting, port)?;
    let apple_version = &greeting[4..11] == b"003.889";
    stream.write_all(version.reply())?;
    stream.flush()?;
    log(format!(
        "RFB server version {}; answered {}",
        String::from_utf8_lossy(&greeting[4..11]),
        String::from_utf8_lossy(&version.reply()[4..11])
    ));

    let (security, offered_ard) = negotiate_security(stream, version, auth, log)?;
    match security {
        NONE => {
            if version == Version::V3_8 {
                security_result(stream, version, security)?;
            }
        }
        VNC_AUTH => {
            let password = password(auth).ok_or_else(|| missing(security))?;
            let mut challenge = [0u8; 16];
            stream.read_exact(&mut challenge)?;
            let response = vnc_auth_response(password.as_bytes(), &challenge);
            stream.write_all(&response)?;
            stream.flush()?;
            security_result(stream, version, security)?;
        }
        APPLE_REMOTE_DESKTOP => {
            let (Some(username), Some(password)) = (username(auth), password(auth)) else {
                return Err(missing(security).into());
            };
            ard(stream, username, password)?;
            security_result(stream, version, security)?;
        }
        other => {
            return Err(DriverError::Failed(format!(
                "the VNC server chose security type {other}, which this client does not support"
            ))
            .into())
        }
    }

    // ClientInit: shared. A non-shared client makes the server disconnect every other
    // viewer, which would throw the user's own VNC window off the desktop.
    stream.write_all(&[1])?;
    stream.flush()?;
    let mut init = server_init(stream, log)?;
    init.apple = apple_version || offered_ard;
    if init.apple {
        log("Apple's VNC server (macOS Screen Sharing): alt is sent as Meta_L, which it takes for Option".to_owned());
    }
    Ok(init)
}

/// The account name to sign in with, if there is one worth sending.
fn username(auth: &VncAuth) -> Option<&str> {
    auth.username.as_deref().filter(|text| !text.is_empty())
}

/// The password to sign in with. An empty one is treated as none: no server
/// that asks for a password accepts an empty one, and asking is better than
/// spending one of the server's allowed attempts.
fn password(auth: &VncAuth) -> Option<&str> {
    auth.password
        .as_deref()
        .map(String::as_str)
        .filter(|text| !text.is_empty())
}

fn parse_version(greeting: &[u8; 12], port: u16) -> Result<Version, Failure> {
    let well_formed = greeting.starts_with(b"RFB ")
        && greeting[11] == b'\n'
        && greeting[7] == b'.'
        && greeting[4..7]
            .iter()
            .chain(&greeting[8..11])
            .all(u8::is_ascii_digit);
    if !well_formed {
        return Err(DriverError::Failed(format!(
            "port {port} answered, but not with an RFB (VNC) greeting; check that the entry's port is the VNC server's"
        ))
        .into());
    }
    let number = &greeting[4..11];
    Ok(match number {
        // 003.006 is how some UltraVNC builds announce 3.3.
        b"003.003" | b"003.006" => Version::V3_3,
        b"003.007" => Version::V3_7,
        b"003.008" | b"003.889" | b"004.000" | b"004.001" | b"005.000" => Version::V3_8,
        _ => {
            return Err(DriverError::Failed(format!(
                "the VNC server speaks RFB {}, which this client does not support",
                String::from_utf8_lossy(number)
            ))
            .into())
        }
    })
}

fn type_name(security: u8) -> String {
    match security {
        NONE => "1 (no authentication)".to_owned(),
        VNC_AUTH => "2 (VNC password)".to_owned(),
        APPLE_REMOTE_DESKTOP => "30 (Apple Remote Desktop)".to_owned(),
        16 => "16 (Tight)".to_owned(),
        18 => "18 (TLS)".to_owned(),
        19 => "19 (VeNCrypt)".to_owned(),
        other => other.to_string(),
    }
}

/// The security type to use among those offered, given what there is to
/// sign in with.
///
/// First whatever the credentials at hand complete: Apple Remote Desktop with
/// an account name and a password, the VNC password with a password. Then no
/// authentication, when the server allows it — a server offering both "none"
/// and a password is reached without asking for a password nobody has (an
/// empty one counts as none, so asking would only ask again). Only when
/// nothing works without a sign-in is a type chosen to ask for one: Apple
/// Remote Desktop when an account name is known, else the VNC password, else
/// Apple Remote Desktop, which asks for both.
pub fn choose_security(offered: &[u8], has_username: bool, has_password: bool) -> Option<u8> {
    let offers = |security| offered.contains(&security);
    if has_username && has_password && offers(APPLE_REMOTE_DESKTOP) {
        Some(APPLE_REMOTE_DESKTOP)
    } else if has_password && offers(VNC_AUTH) {
        Some(VNC_AUTH)
    } else if offers(NONE) {
        Some(NONE)
    } else if has_username && offers(APPLE_REMOTE_DESKTOP) {
        Some(APPLE_REMOTE_DESKTOP)
    } else if offers(VNC_AUTH) {
        Some(VNC_AUTH)
    } else if offers(APPLE_REMOTE_DESKTOP) {
        Some(APPLE_REMOTE_DESKTOP)
    } else {
        None
    }
}

fn missing(security: u8) -> DriverError {
    if security == APPLE_REMOTE_DESKTOP {
        DriverError::Credentials {
            retry: false,
            needs_username: true,
            message: "The VNC server asks for an account name and password (Apple Remote Desktop sign-in)."
                .to_owned(),
        }
    } else {
        DriverError::Credentials {
            retry: false,
            needs_username: false,
            message: "The VNC server asks for a password.".to_owned(),
        }
    }
}

/// The security type to use, and whether Apple Remote Desktop (30) was among
/// those offered — or, on 3.3, was the server's choice.
fn negotiate_security<S: Read + Write>(
    stream: &mut S,
    version: Version,
    auth: &VncAuth,
    log: &dyn Fn(String),
) -> Result<(u8, bool), Failure> {
    if version == Version::V3_3 {
        let chosen = decode::read_u32(stream)?;
        if chosen == 0 {
            let reason = read_reason(stream)?;
            return Err(DriverError::Failed(format!(
                "the VNC server refused the connection: {reason}"
            ))
            .into());
        }
        let Ok(chosen) = u8::try_from(chosen) else {
            return Err(DriverError::Failed(format!(
                "the VNC server chose security type {chosen}, which this client does not support"
            ))
            .into());
        };
        log(format!(
            "security type {}, chosen by the server",
            type_name(chosen)
        ));
        // The server decided; a missing credential is still the user's to supply.
        if chosen == VNC_AUTH && password(auth).is_none() {
            return Err(missing(chosen).into());
        }
        return Ok((chosen, chosen == APPLE_REMOTE_DESKTOP));
    }

    let mut count = [0u8; 1];
    stream.read_exact(&mut count)?;
    if count[0] == 0 {
        let reason = read_reason(stream)?;
        return Err(DriverError::Failed(format!(
            "the VNC server refused the connection: {reason}"
        ))
        .into());
    }
    let mut offered = vec![0u8; usize::from(count[0])];
    stream.read_exact(&mut offered)?;
    let listed = offered
        .iter()
        .map(|security| type_name(*security))
        .collect::<Vec<_>>()
        .join(", ");
    let Some(chosen) = choose_security(
        &offered,
        username(auth).is_some(),
        password(auth).is_some(),
    ) else {
        log(format!("security types offered: {listed}; none usable"));
        return Err(DriverError::Failed(format!(
            "the VNC server offers only security types {listed}, which this client does not support; \
             allow VNC password (2), Apple Remote Desktop (30) or no authentication (1) on the server"
        ))
        .into());
    };
    log(format!(
        "security types offered: {listed}; using {}",
        type_name(chosen)
    ));
    match chosen {
        VNC_AUTH if password(auth).is_none() => return Err(missing(chosen).into()),
        APPLE_REMOTE_DESKTOP if username(auth).is_none() || password(auth).is_none() => {
            return Err(missing(chosen).into())
        }
        _ => {}
    }
    stream.write_all(&[chosen])?;
    stream.flush()?;
    Ok((chosen, offered.contains(&APPLE_REMOTE_DESKTOP)))
}

/// A server's reason for a failure: a length, then text. Cut to a line a log
/// and a model can carry.
fn read_reason(stream: &mut impl Read) -> Result<String, Failure> {
    let length = decode::read_u32(stream)?;
    let kept = length.min(MAX_REASON);
    let mut reason = vec![0u8; kept as usize];
    stream.read_exact(&mut reason)?;
    if length > kept {
        decode::skip(stream, u64::from(length - kept))?;
    }
    let reason = printable(&String::from_utf8_lossy(&reason), 300);
    Ok(if reason.is_empty() {
        "no reason given".to_owned()
    } else {
        reason
    })
}

/// `text` without control characters, at most `limit` characters long.
pub fn printable(text: &str, limit: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(limit)
        .collect();
    cleaned.trim().to_owned()
}

fn security_result<S: Read + Write>(
    stream: &mut S,
    version: Version,
    security: u8,
) -> Result<(), Failure> {
    let status = decode::read_u32(stream)?;
    if status == 0 {
        return Ok(());
    }
    let reason = if version == Version::V3_8 {
        Some(read_reason(stream)?)
    } else {
        None
    };
    let because = reason
        .map(|reason| format!(": {reason}"))
        .unwrap_or_default();
    Err(match security {
        VNC_AUTH => DriverError::Credentials {
            retry: true,
            needs_username: false,
            message: format!("The VNC server did not accept the password{because}."),
        },
        APPLE_REMOTE_DESKTOP => DriverError::Credentials {
            retry: true,
            needs_username: true,
            message: format!(
                "The VNC server did not accept the account name and password{because}."
            ),
        },
        _ => DriverError::Failed(format!("the VNC server refused the connection{because}")),
    }
    .into())
}

/// VNC authentication: the challenge encrypted with DES, keyed by the first
/// eight bytes of the password (zero-padded) with each byte's bits reversed —
/// a quirk of the original implementation every server has kept.
pub fn vnc_auth_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = Zeroizing::new([0u8; 8]);
    for (slot, byte) in key.iter_mut().zip(password) {
        *slot = byte.reverse_bits();
    }
    let cipher = Des::new(GenericArray::from_slice(&key[..]));
    let mut response = *challenge;
    for block in response.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    response
}

fn random(bytes: &mut [u8]) -> Result<(), Failure> {
    SystemRandom::new().fill(bytes).map_err(|_| {
        Failure::Driver(DriverError::Failed(
            "this computer could not produce random bytes for the sign-in".to_owned(),
        ))
    })
}

/// Apple Remote Desktop authentication, as noVNC's `_negotiateARDAuth` does
/// it: anonymous Diffie-Hellman, then the account name and password in a
/// 128-byte block encrypted with AES-128-ECB under the MD5 of the shared
/// secret. The DH does not authenticate the server, which is why connections
/// only ever go to a place's loopback.
fn ard<S: Read + Write>(stream: &mut S, username: &str, password: &str) -> Result<(), Failure> {
    let mut generator = [0u8; 2];
    stream.read_exact(&mut generator)?;
    let key_length = usize::from(decode::read_u16(stream)?);
    if key_length == 0 || key_length > MAX_ARD_KEY {
        return Err(DriverError::Failed(format!(
            "the VNC server proposed a {key_length}-byte Apple Remote Desktop key, which this client does not accept"
        ))
        .into());
    }
    let mut prime = vec![0u8; key_length];
    stream.read_exact(&mut prime)?;
    let mut server_public = vec![0u8; key_length];
    stream.read_exact(&mut server_public)?;

    let mut private = Zeroizing::new(vec![0u8; key_length]);
    random(&mut private)?;
    let mut plaintext = Zeroizing::new([0u8; 128]);
    random(&mut plaintext[..])?;
    place_credentials(&mut plaintext, username, password);
    let (ciphertext, client_public) =
        ard_response(&generator, &prime, &server_public, &private, &plaintext)
            .map_err(|message| Failure::Driver(DriverError::Failed(message)))?;
    let mut answer = Vec::with_capacity(128 + client_public.len());
    answer.extend_from_slice(&ciphertext);
    answer.extend_from_slice(&client_public);
    stream.write_all(&answer)?;
    stream.flush()?;
    Ok(())
}

/// Writes the account name at offset 0 and the password at offset 64 of the
/// block, each cut to 63 bytes and ended with a NUL; the rest stays random.
pub fn place_credentials(block: &mut [u8; 128], username: &str, password: &str) {
    for (offset, text) in [(0, username), (64, password)] {
        let bytes = &text.as_bytes()[..text.len().min(63)];
        block[offset..offset + bytes.len()].copy_from_slice(bytes);
        block[offset + bytes.len()] = 0;
    }
}

/// `value` as exactly `length` big-endian bytes. `BigUint::to_bytes_be` drops
/// leading zeros, and the server hashes the secret at its full length: without
/// the padding about one sign-in in 256 would fail.
pub fn left_pad(value: &BigUint, length: usize) -> Zeroizing<Vec<u8>> {
    let bytes = Zeroizing::new(value.to_bytes_be());
    let mut padded = Zeroizing::new(vec![0u8; length.max(bytes.len())]);
    let start = padded.len() - bytes.len();
    padded[start..].copy_from_slice(&bytes);
    padded
}

/// The encrypted credential block and the client's public key, for the
/// server's generator, prime and public key and the client's private key.
pub fn ard_response(
    generator: &[u8],
    prime: &[u8],
    server_public: &[u8],
    private: &[u8],
    plaintext: &[u8; 128],
) -> Result<([u8; 128], Vec<u8>), String> {
    let key_length = prime.len();
    let modulus = BigUint::from_bytes_be(prime);
    if modulus <= BigUint::from(1u32) {
        return Err("the VNC server sent an invalid Apple Remote Desktop prime".to_owned());
    }
    let exponent = BigUint::from_bytes_be(private);
    let client_public = left_pad(
        &BigUint::from_bytes_be(generator).modpow(&exponent, &modulus),
        key_length,
    );
    let shared = left_pad(
        &BigUint::from_bytes_be(server_public).modpow(&exponent, &modulus),
        key_length,
    );
    let mut key = Zeroizing::new([0u8; 16]);
    key.copy_from_slice(&Md5::digest(&shared[..]));
    let cipher = Aes128::new(GenericArray::from_slice(&key[..]));
    let mut ciphertext = *plaintext;
    for block in ciphertext.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok((ciphertext, client_public.to_vec()))
}

fn server_init(stream: &mut impl Read, log: &dyn Fn(String)) -> Result<ServerInit, Failure> {
    let width = u32::from(decode::read_u16(stream)?);
    let height = u32::from(decode::read_u16(stream)?);
    // The server's own pixel format; the client replaces it with its own.
    let mut pixel_format = [0u8; 16];
    stream.read_exact(&mut pixel_format)?;
    let length = decode::read_u32(stream)?;
    if length > MAX_NAME {
        return Err(DriverError::Failed(format!(
            "the VNC server sent a desktop name of {length} bytes, more than this client accepts"
        ))
        .into());
    }
    let mut name = vec![0u8; length as usize];
    stream.read_exact(&mut name)?;
    let name = printable(&String::from_utf8_lossy(&name), KEPT_NAME);
    check_size(width, height).map_err(DriverError::Failed)?;
    log(format!(
        "desktop \"{}\", {width}x{height}",
        printable(&name, 200)
    ));
    Ok(ServerInit {
        width,
        height,
        name,
        apple: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vnc_auth_response_matches_an_independent_des_vector() {
        // Computed with OpenSSL: key 0e86ceceeef64e26 ("password", each byte's bits
        // reversed), DES-ECB, no padding.
        let challenge = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        assert_eq!(
            vnc_auth_response(b"password", &challenge),
            [
                0x2b, 0x23, 0x72, 0x57, 0xe9, 0xf9, 0x0c, 0x81, 0x65, 0xdc, 0x8e, 0x23, 0x10, 0x7e,
                0xdf, 0x71
            ]
        );
        // Only the first eight bytes count.
        assert_eq!(
            vnc_auth_response(b"password-and-more", &challenge),
            vnc_auth_response(b"password", &challenge)
        );
    }

    #[test]
    fn security_is_chosen_by_preference_not_by_server_order() {
        // With a password at hand.
        assert_eq!(choose_security(&[1, 2], false, true), Some(2));
        assert_eq!(choose_security(&[19, 16, 1], false, true), Some(1));
        assert_eq!(choose_security(&[30, 33, 36, 35], true, true), Some(30));
        assert_eq!(choose_security(&[2, 30], true, true), Some(30));
        assert_eq!(choose_security(&[30, 2], false, true), Some(2));
        assert_eq!(choose_security(&[30, 33, 36, 35], false, true), Some(30));
        assert_eq!(choose_security(&[33, 35, 36], true, true), None);
        assert_eq!(choose_security(&[19, 18], false, true), None);
        assert_eq!(choose_security(&[30, 1], false, true), Some(1));
    }

    /// A server that also allows no authentication is reached without a password nobody has;
    /// one that allows none asks for the sign-in it can take.
    #[test]
    fn without_a_password_no_authentication_wins_and_otherwise_one_is_asked_for() {
        assert_eq!(choose_security(&[1, 2], false, false), Some(1));
        assert_eq!(choose_security(&[2, 1], true, false), Some(1));
        assert_eq!(choose_security(&[30, 2, 1], true, false), Some(1));
        assert_eq!(choose_security(&[2], false, false), Some(2));
        assert_eq!(choose_security(&[2, 30], true, false), Some(30));
        assert_eq!(choose_security(&[2, 30], false, false), Some(2));
        assert_eq!(choose_security(&[30, 33], false, false), Some(30));
        assert_eq!(choose_security(&[19], false, false), None);
    }

    #[test]
    fn versions_map_to_their_dialects() {
        let parse = |text: &[u8; 12]| parse_version(text, 5900).ok();
        assert_eq!(parse(b"RFB 003.003\n"), Some(Version::V3_3));
        assert_eq!(parse(b"RFB 003.007\n"), Some(Version::V3_7));
        for version in [
            b"RFB 003.008\n",
            b"RFB 003.889\n",
            b"RFB 004.000\n",
            b"RFB 004.001\n",
            b"RFB 005.000\n",
        ] {
            assert_eq!(parse(version), Some(Version::V3_8));
        }
        assert_eq!(parse(b"RFB 002.000\n"), None);
        assert_eq!(parse(b"HTTP/1.1 400"), None);
    }

    #[test]
    fn credentials_sit_at_their_offsets_cut_to_63_bytes() {
        let mut block = [0xaa; 128];
        place_credentials(&mut block, "holycat", &"p".repeat(80));
        assert_eq!(&block[..7], b"holycat");
        assert_eq!(block[7], 0);
        assert_eq!(block[8], 0xaa);
        assert_eq!(&block[64..127], "p".repeat(63).as_bytes());
        assert_eq!(block[127], 0);
    }

    #[test]
    fn a_shared_secret_with_a_leading_zero_keeps_it_before_hashing() {
        // p = 65521 (two bytes), g = 3. With private key 1 the secret is the server's
        // public key itself, so a public key below 256 gives a secret of 0x00XX.
        let prime = 65521u32.to_be_bytes()[2..].to_vec();
        let server_public = [0x00, 0x2a];
        let plaintext = [7u8; 128];
        let (ciphertext, client_public) =
            ard_response(&[0, 3], &prime, &server_public, &[0, 1], &plaintext).unwrap();
        assert_eq!(client_public, vec![0x00, 0x03]);
        let key = Md5::digest([0x00, 0x2a]);
        let cipher = Aes128::new(GenericArray::from_slice(&key[..]));
        let mut expected = plaintext;
        for block in expected.chunks_exact_mut(16) {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        assert_eq!(ciphertext, expected);
        assert_eq!(left_pad(&BigUint::from(5u32), 3).to_vec(), vec![0, 0, 5]);
        assert!(ard_response(&[0, 2], &[0, 1], &[0, 1], &[1], &plaintext).is_err());
    }
}
