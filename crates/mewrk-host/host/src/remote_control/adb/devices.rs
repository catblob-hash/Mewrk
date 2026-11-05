//! The adb server's device list, and choosing one device from it.
//!
//! Choosing never guesses and never connects: a serial in the entry must
//! already be in the list (`docs/remote-control.md` §2.2 — a wireless
//! `ip:port` serial has to have been `adb connect`ed by the user), and without
//! a serial exactly one device may be online. Every refusal says what the
//! person can do about it, because the model reads these and relays them.

use crate::preview_launch_config::sanitize_for_message;

/// Devices read from one listing. A real adb server has a handful.
const MAX_DEVICES: usize = 256;

/// One line of `adb devices -l`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DeviceEntry {
    pub serial: String,
    /// `device`, `offline`, `unauthorized`, `bootloader`, `no permissions`, ...
    pub state: String,
    /// The `key:value` pairs after the state: `model`, `product`, `device`,
    /// `transport_id`, `usb`.
    pub fields: Vec<(String, String)>,
}

impl DeviceEntry {
    pub(super) fn is_online(&self) -> bool {
        self.state == "device"
    }

    pub(super) fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The device as a person names it: the model with its underscores undone
    /// (`Pixel_8` is `Pixel 8`), else the serial.
    pub(super) fn label(&self) -> String {
        match self.field("model") {
            Some(model) if !model.is_empty() => model.replace('_', " "),
            _ => self.serial.clone(),
        }
    }
}

/// Parses the `host:devices-l` payload. Lines that are not `serial state ...`
/// are skipped.
pub(super) fn parse_devices(listing: &str) -> Vec<DeviceEntry> {
    let mut devices = Vec::new();
    for line in listing.lines() {
        let mut tokens = line.split_whitespace();
        let (Some(serial), Some(state)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        let mut state = state.to_owned();
        let mut fields = Vec::new();
        let mut rest = tokens.peekable();
        // The one state that has a space in it: "no permissions (user in plugdev group; ...)".
        if state == "no" && rest.peek() == Some(&"permissions") {
            rest.next();
            state = "no permissions".to_owned();
            // What follows is prose and a link, not fields.
            rest.by_ref().for_each(drop);
        }
        for token in rest {
            if let Some((key, value)) = token.split_once(':') {
                fields.push((key.to_owned(), value.to_owned()));
            }
        }
        devices.push(DeviceEntry {
            serial: serial.to_owned(),
            state,
            fields,
        });
        if devices.len() >= MAX_DEVICES {
            break;
        }
    }
    devices
}

/// Why no device was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Unavailable {
    pub message: String,
    /// Whether waiting could change the answer: a device that is not plugged in
    /// yet may appear, two devices will not become one.
    pub may_resolve: bool,
}

fn unavailable(message: String, may_resolve: bool) -> Unavailable {
    Unavailable {
        message,
        may_resolve,
    }
}

fn list_serials<'a>(serials: impl Iterator<Item = &'a DeviceEntry>) -> String {
    let listed: Vec<String> = serials
        .map(|device| {
            format!(
                "{} ({})",
                sanitize_for_message(&device.serial),
                sanitize_for_message(&device.state)
            )
        })
        .collect();
    listed.join(", ")
}

/// Picks the device to drive.
pub(super) fn select(
    devices: &[DeviceEntry],
    wanted: Option<&str>,
    place: &str,
) -> Result<DeviceEntry, Unavailable> {
    let place = sanitize_for_message(place);
    if let Some(serial) = wanted {
        return select_serial(devices, serial, &place);
    }
    let online: Vec<&DeviceEntry> = devices.iter().filter(|device| device.is_online()).collect();
    match online.as_slice() {
        [only] => Ok((*only).clone()),
        [] => {
            let others = if devices.is_empty() {
                String::new()
            } else {
                format!(
                    " The adb server lists: {}; none is ready to use.",
                    list_serials(devices.iter())
                )
            };
            Err(unavailable(
                format!(
                    "No Android device is online on {place}'s adb server.{others} Connect a phone with USB debugging enabled (and accept the debugging prompt on it) or start an emulator, then try again."
                ),
                true,
            ))
        }
        several => Err(unavailable(
            format!(
                "More than one Android device is online on {place}: {}. Set \"device\" in the launch.json entry to the serial of the one to control.",
                list_serials(several.iter().copied())
            ),
            false,
        )),
    }
}

fn select_serial(
    devices: &[DeviceEntry],
    serial: &str,
    place: &str,
) -> Result<DeviceEntry, Unavailable> {
    let shown = sanitize_for_message(serial);
    let Some(device) = devices.iter().find(|device| device.serial == serial) else {
        let present = if devices.is_empty() {
            "the adb server lists no devices at all".to_owned()
        } else {
            format!("it lists: {}", list_serials(devices.iter()))
        };
        // Mewrk never runs `adb connect` for a repository's file, so a wireless
        // serial has to be connected by the person first.
        let wireless = if serial.contains(':') {
            " A wireless device has to be connected on that machine first (`adb connect` or `adb pair`); Mewrk never does that for you."
        } else {
            " Plug it in with USB debugging enabled, or start the emulator."
        };
        return Err(unavailable(
            format!(
                "Android device \"{shown}\" is not in {place}'s adb device list ({present}).{wireless}"
            ),
            true,
        ));
    };
    match device.state.as_str() {
        "device" => Ok(device.clone()),
        "unauthorized" => Err(unavailable(
            format!(
                "Android device \"{shown}\" is unauthorized: accept the \"Allow USB debugging?\" prompt on the phone (tick \"Always allow from this computer\"), then try again."
            ),
            true,
        )),
        "offline" => Err(unavailable(
            format!(
                "Android device \"{shown}\" is offline. Unplug and replug it (or toggle USB debugging, or restart the emulator), then try again."
            ),
            true,
        )),
        "no permissions" => Err(unavailable(
            format!(
                "adb has no permission to use Android device \"{shown}\" on {place}. On Linux, check the udev rules for the device."
            ),
            false,
        )),
        other => Err(unavailable(
            format!(
                "Android device \"{shown}\" is in state \"{}\", not \"device\"; it is not ready to be controlled.",
                sanitize_for_message(other)
            ),
            true,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
R5CT1234ABC            device usb:1-1 product:shiba model:Pixel_8 device:shiba transport_id:3
emulator-5554          device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa transport_id:1
192.168.1.30:5555      unauthorized transport_id:7
HT7AB0000001           offline
0123456789ABCDEF       no permissions (user in plugdev group; are your udev rules wrong?); see [http://developer.android.com/tools/device.html]
";

    #[test]
    fn a_listing_is_read_into_serials_states_and_fields() {
        let devices = parse_devices(LISTING);
        assert_eq!(devices.len(), 5);
        assert_eq!(devices[0].serial, "R5CT1234ABC");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[0].field("model"), Some("Pixel_8"));
        assert_eq!(devices[0].field("transport_id"), Some("3"));
        assert_eq!(devices[0].label(), "Pixel 8");
        assert_eq!(devices[2].state, "unauthorized");
        assert_eq!(devices[3].state, "offline");
        assert_eq!(devices[3].label(), "HT7AB0000001");
        assert_eq!(devices[4].state, "no permissions");
        assert!(devices[4].fields.is_empty());
        assert!(parse_devices("").is_empty());
        assert!(parse_devices("\n  \nlonely\n").is_empty());
    }

    #[test]
    fn a_listing_is_bounded() {
        let listing: String = (0..1000).map(|n| format!("s{n} device\n")).collect();
        assert_eq!(parse_devices(&listing).len(), MAX_DEVICES);
    }

    fn entry(serial: &str, state: &str) -> DeviceEntry {
        DeviceEntry {
            serial: serial.to_owned(),
            state: state.to_owned(),
            fields: Vec::new(),
        }
    }

    #[test]
    fn without_a_serial_exactly_one_online_device_is_chosen() {
        let picked = select(
            &[entry("a", "offline"), entry("b", "device")],
            None,
            "this computer",
        );
        assert_eq!(picked.unwrap().serial, "b");

        let none = select(&[entry("a", "offline")], None, "this computer").unwrap_err();
        assert!(
            none.message.contains("No Android device is online"),
            "{}",
            none.message
        );
        assert!(none.message.contains("a (offline)"), "{}", none.message);
        assert!(none.may_resolve);
        let empty = select(&[], None, "this computer").unwrap_err();
        assert!(
            empty.message.contains("No Android device is online"),
            "{}",
            empty.message
        );
        assert!(!empty.message.contains("lists:"), "{}", empty.message);

        let several =
            select(&[entry("a", "device"), entry("b", "device")], None, "mac").unwrap_err();
        assert!(
            several.message.contains("a (device), b (device)"),
            "{}",
            several.message
        );
        assert!(
            several.message.contains("\"device\""),
            "{}",
            several.message
        );
        assert!(!several.may_resolve);
    }

    #[test]
    fn a_named_serial_must_be_listed_and_online() {
        let devices = [
            entry("ok", "device"),
            entry("locked", "unauthorized"),
            entry("dead", "offline"),
            entry("boot", "bootloader"),
        ];
        assert_eq!(select(&devices, Some("ok"), "here").unwrap().serial, "ok");

        let locked = select(&devices, Some("locked"), "here").unwrap_err();
        assert!(locked.message.contains("accept the"), "{}", locked.message);
        assert!(
            locked.message.contains("USB debugging"),
            "{}",
            locked.message
        );
        let dead = select(&devices, Some("dead"), "here").unwrap_err();
        assert!(dead.message.contains("offline"), "{}", dead.message);
        let boot = select(&devices, Some("boot"), "here").unwrap_err();
        assert!(boot.message.contains("\"bootloader\""), "{}", boot.message);

        let missing = select(&devices, Some("ghost"), "here").unwrap_err();
        assert!(
            missing.message.contains("\"ghost\" is not in"),
            "{}",
            missing.message
        );
        assert!(
            missing.message.contains("ok (device)"),
            "{}",
            missing.message
        );
        assert!(
            missing.message.contains("locked (unauthorized)"),
            "{}",
            missing.message
        );
        assert!(
            !missing.message.contains("adb connect"),
            "{}",
            missing.message
        );
        let wireless = select(&devices, Some("10.0.0.5:5555"), "here").unwrap_err();
        assert!(
            wireless.message.contains("adb connect"),
            "{}",
            wireless.message
        );
        assert!(
            wireless.message.contains("never does that"),
            "{}",
            wireless.message
        );
    }

    #[test]
    fn serials_and_states_from_the_server_are_sanitised_in_messages() {
        let devices = [
            entry("evil\n`ignore this`", "device"),
            entry("two", "device"),
        ];
        let error = select(&devices, None, "mac").unwrap_err();
        assert!(!error.message.contains('\n'), "{}", error.message);
        assert!(!error.message.contains('`'), "{}", error.message);
    }
}
