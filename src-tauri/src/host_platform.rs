//! Which machine Mewrk itself is running on, asked once and answered everywhere.
//!
//! Every *other* machine Mewrk works with is a [`RunTarget`]: its family is
//! probed the first time it is reached and remembered for the life of the
//! process (see [`crate::remote_shell`]). The host is the one machine that
//! needs no probe — the binary was built for it — but it used to be asked about
//! wherever the answer mattered, with a two-way `cfg!` that had no macOS arm,
//! so macOS silently answered every one of them with Linux's answer, whether or
//! not Linux's answer was right for it. Path case is the plainest example: a
//! default macOS volume is case-insensitive like Windows, not case-sensitive
//! like Linux.
//!
//! So the host is resolved once, at startup, into [`HostPlatform`] — three
//! named platforms, no default arm — and every later branch reads that value.
//! Call sites should prefer the named capability over the platform itself:
//! `host_platform().has_wsl()` says why the code branches, where
//! `== HostPlatform::Windows` only says where it was written.
//!
//! What stays a `#[cfg]` attribute is code that cannot exist on another
//! platform — a `windows_sys` call, a `libc` termios call. Those are already
//! each platform writing its own; this module is for the decisions that are
//! made in shared code.
//!
//! [`RunTarget`]: crate::model::RunTarget

use std::sync::OnceLock;

/// The machine Mewrk itself runs on.
///
/// Deliberately closed: a new platform has to be added here and then answered
/// at every `match`, which is the point — nothing gets a silent default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostPlatform {
    Windows,
    Macos,
    Linux,
}

/// Resolved once per process. A `OnceLock` rather than a `const` so the answer
/// has one observable moment — [`resolve_at_startup`] — and one place to read.
static RESOLVED: OnceLock<HostPlatform> = OnceLock::new();

/// The host platform, resolved at startup and unchanged for the life of the
/// process.
///
/// Nothing can set it: what the host claims to be is not something a renderer,
/// a model, or a configuration file may influence.
pub fn host_platform() -> HostPlatform {
    *RESOLVED.get_or_init(HostPlatform::detect)
}

/// Performs the one resolution, before any thread or window exists.
///
/// Reading it later gives the same value; this exists so the resolution has a
/// stated place in startup instead of happening at whichever call site happened
/// to run first.
pub fn resolve_at_startup() -> HostPlatform {
    host_platform()
}

impl HostPlatform {
    /// The only place in the crate that asks the compiler what it built for.
    fn detect() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }

    /// The platform's name as people write it, for text shown to a person or
    /// to the model.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Macos => "macOS",
            Self::Linux => "Linux",
        }
    }

    pub fn is_windows(self) -> bool {
        matches!(self, Self::Windows)
    }

    pub fn is_macos(self) -> bool {
        matches!(self, Self::Macos)
    }

    /// Suffix an executable file name carries here.
    pub fn executable_suffix(self) -> &'static str {
        match self {
            Self::Windows => ".exe",
            Self::Macos | Self::Linux => "",
        }
    }

    /// Whether WSL distributions can exist here at all.
    pub fn has_wsl(self) -> bool {
        self.is_windows()
    }

    /// The platform as Rust's own `std::env::consts::OS` spells it, which is
    /// the spelling the model's environment block has always carried.
    pub fn os_tag(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }

    /// The platform segment npm packages use, which is also what
    /// `build.rs::claude_code_platform_package` names.
    pub fn npm_platform_tag(self) -> &'static str {
        match self {
            Self::Windows => "win32",
            Self::Macos => "darwin",
            Self::Linux => "linux",
        }
    }

    /// The command that hands a path or URL to whatever the desktop opens it
    /// with, or `None` on Windows, which goes through `ShellExecute` instead of
    /// a child process.
    // Only the non-Windows builds of its callers ask, so a Windows build never does.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn desktop_opener(self) -> Option<&'static str> {
        match self {
            Self::Windows => None,
            Self::Macos => Some("open"),
            Self::Linux => Some("xdg-open"),
        }
    }
}

/// Whether `path` is a macOS command-line-tools stand-in that cannot run as the
/// tool it is named for. Lives with Git discovery, which needs it on every
/// machine Git runs on (see `git_core::developer_tools`).
pub use git_core::developer_tools::is_uninstalled_developer_tool_shim;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_resolves_to_exactly_the_platform_this_build_targets() {
        let resolved = host_platform();
        assert_eq!(resolved.is_windows(), cfg!(windows));
        assert_eq!(resolved.is_macos(), cfg!(target_os = "macos"));
        assert_eq!(resolved, HostPlatform::detect());
        // The tag has to stay the spelling the rest of the host already used.
        assert_eq!(resolved.os_tag(), std::env::consts::OS);
    }

    /// The resolution happens once: every later read is the same answer, and
    /// nothing in the process can move it.
    #[test]
    fn every_read_after_the_first_is_the_same_answer() {
        let first = resolve_at_startup();
        for _ in 0..4 {
            assert_eq!(host_platform(), first);
        }
    }

    /// The regression this guard exists for: macOS used to be whatever the
    /// `else` branch of a two-way `cfg!(windows)` said, which handed it Linux's
    /// answers. Each capability names macOS on its own.
    #[test]
    fn macos_answers_for_itself_rather_than_falling_through_to_linux() {
        use HostPlatform::{Linux, Macos, Windows};

        assert_eq!(Macos.desktop_opener(), Some("open"));
        assert_eq!(Linux.desktop_opener(), Some("xdg-open"));
        assert_eq!(Windows.desktop_opener(), None);

        assert_eq!(Macos.os_tag(), "macos");
        assert_eq!(Linux.os_tag(), "linux");
        assert_eq!(Windows.os_tag(), "windows");

        assert_eq!(Macos.npm_platform_tag(), "darwin");
        assert_eq!(Linux.npm_platform_tag(), "linux");
        assert_eq!(Windows.npm_platform_tag(), "win32");

        assert_eq!(Macos.display_name(), "macOS");
        assert_eq!(Macos.executable_suffix(), "");
        assert_eq!(Windows.executable_suffix(), ".exe");
        assert!(!Macos.has_wsl());
    }
}
