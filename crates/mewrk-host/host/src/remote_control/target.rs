//! A remote-control entry of `.mewrk/launch.json`, once parsed.
//!
//! The file says which protocol, which place, which port and which device —
//! nothing that could carry an address or a secret (`docs/remote-control.md`
//! §2). The parser ([`crate::preview_launch_config`]) builds this; the session
//! layer resolves the place and connects.

use serde::Serialize;

use super::driver::Platform;

/// The protocol a remote-control entry speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Protocol {
    /// RFB: a desktop or a virtual machine.
    Vnc,
    /// The adb server's protocol: Android devices and emulators.
    Adb,
    /// WebDriverAgent's HTTP API: iOS devices and simulators.
    Wda,
}

impl Protocol {
    /// The wire name, as `"protocol"` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vnc => "vnc",
            Self::Adb => "adb",
            Self::Wda => "wda",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "vnc" => Some(Self::Vnc),
            "adb" => Some(Self::Adb),
            "wda" => Some(Self::Wda),
            _ => None,
        }
    }

    /// The port its service listens on unless the entry says otherwise: the RFB
    /// default, the adb **server**'s port, WebDriverAgent's.
    pub fn default_port(self) -> u16 {
        match self {
            Self::Vnc => 5900,
            Self::Adb => 5037,
            Self::Wda => 8100,
        }
    }

    /// The platform its screens are, where the protocol decides it. RFB serves
    /// any desktop.
    pub fn platform(self) -> Platform {
        match self {
            Self::Vnc => Platform::Desktop,
            Self::Adb => Platform::Android,
            Self::Wda => Platform::Ios,
        }
    }

    /// How a person names it.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Vnc => "VNC",
            Self::Adb => "Android",
            Self::Wda => "iOS",
        }
    }
}

/// Where the service is (`docs/remote-control.md` §2.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "name")]
pub enum TargetPlace {
    /// Where the workspace is: this computer, or the workspace's SSH machine.
    Workspace,
    /// This computer, whatever the workspace is (`"thisComputer": true`).
    ThisComputer,
    /// An SSH machine the user registered, by its display name as the file
    /// writes it (`"machine": "mac"`). Resolved case-insensitively; a name two
    /// machines share is an error, not a guess.
    Machine(String),
}

/// One remote-control entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlTarget {
    pub protocol: Protocol,
    pub place: TargetPlace,
    /// The service's port on the place's loopback; the protocol's default when
    /// the file names none.
    pub port: u16,
    /// adb: the device serial. `None` asks for the only device online.
    pub device: Option<String>,
    /// vnc: the account name for Apple Remote Desktop authentication.
    pub username: Option<String>,
    /// Another entry of the same file to start first — the virtual machine, the
    /// emulator, the WebDriverAgent bridge — by its name.
    pub launch: Option<String>,
    /// Look, never touch: every input is refused.
    pub read_only: bool,
}

/// Whether `serial` is something adb could have printed as a device serial.
/// Serials are opaque, but they are short and plain: USB serials, `emulator-5554`,
/// `192.168.1.30:5555`, `adb-XXXX._adb-tls-connect._tcp`.
pub fn is_plausible_serial(serial: &str) -> bool {
    !serial.is_empty()
        && serial.len() <= 64
        && serial
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-[]".contains(&byte))
}

/// Whether `name` could be an account name for Apple Remote Desktop
/// authentication: printable, no control characters, at most 63 bytes (the
/// protocol's field).
pub fn is_plausible_username(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocols_round_trip_their_wire_names_and_know_their_ports() {
        for protocol in [Protocol::Vnc, Protocol::Adb, Protocol::Wda] {
            assert_eq!(Protocol::parse(protocol.as_str()), Some(protocol));
        }
        assert_eq!(Protocol::parse("rdp"), None);
        assert_eq!(Protocol::parse("VNC"), None);
        assert_eq!(Protocol::Vnc.default_port(), 5900);
        assert_eq!(Protocol::Adb.default_port(), 5037);
        assert_eq!(Protocol::Wda.default_port(), 8100);
    }

    #[test]
    fn serials_are_short_and_plain() {
        assert!(is_plausible_serial("emulator-5554"));
        assert!(is_plausible_serial("192.168.1.30:5555"));
        assert!(is_plausible_serial("adb-R5CT1234ABC-xyZ12._adb-tls-connect._tcp"));
        assert!(!is_plausible_serial(""));
        assert!(!is_plausible_serial("a b"));
        assert!(!is_plausible_serial("x;rm -rf /"));
        assert!(!is_plausible_serial(&"a".repeat(65)));
    }

    #[test]
    fn usernames_fit_the_ard_field() {
        assert!(is_plausible_username("holycat"));
        assert!(is_plausible_username("Jane Doe"));
        assert!(!is_plausible_username(""));
        assert!(!is_plausible_username("a\nb"));
        assert!(!is_plausible_username(&"a".repeat(64)));
    }
}
