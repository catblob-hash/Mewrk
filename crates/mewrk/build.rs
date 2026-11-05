//! Embeds the icon, the version block and the application manifest of `mewrk.exe` (assets/mewrk.rc).
//!
//! Only the Windows binary gets a resource. On every other target this script does nothing, and the `embed-resource` build dependency
//! is compiled only for Windows hosts. The resource goes into the `mewrk` binary alone: the library and the test binaries stay untouched.
//! `RC` names the resource compiler (rc.exe). Without it, embed-resource searches the Windows SDK itself.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/mewrk.rc");
    println!("cargo:rerun-if-changed=assets/mewrk.exe.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-env-changed=RC");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    #[cfg(windows)]
    embed_windows_resource();
    #[cfg(not(windows))]
    println!("cargo:warning=mewrk.exe resources are only embedded by a Windows host (cross builds get none)");
}

#[cfg(windows)]
fn embed_windows_resource() {
    let major = std::env::var("CARGO_PKG_VERSION_MAJOR").expect("cargo sets CARGO_PKG_VERSION_MAJOR");
    let minor = std::env::var("CARGO_PKG_VERSION_MINOR").expect("cargo sets CARGO_PKG_VERSION_MINOR");
    let patch = std::env::var("CARGO_PKG_VERSION_PATCH").expect("cargo sets CARGO_PKG_VERSION_PATCH");
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let macros = vec![
        format!("MEWRK_FILEVERSION={major},{minor},{patch},0"),
        // The quotes are part of the macro value: the resource compiler needs a string literal here.
        format!("MEWRK_VERSION_STR=\"{version}\""),
    ];
    embed_resource::compile_for("assets/mewrk.rc", ["mewrk"], embed_resource::ParamsMacros(macros)).manifest_required().unwrap();
}
