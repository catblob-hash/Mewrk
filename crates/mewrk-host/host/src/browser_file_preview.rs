//! The built-in browser's "open file" entry.
//!
//! The page WebView admits only `http`/`https`, so a picked file is served from a virtual host
//! name mapped onto the folder that holds it. That folder and the folders below it load the way a
//! web server would serve them — an HTML page gets its stylesheets, scripts, images and fonts by
//! their relative links, under their own names — and nothing outside it is reachable: the CEF
//! handler re-checks every request after canonicalisation, and WebView2 serves only the mapped
//! folder. The folder is served in place, never copied, so the mapping is all there is to release.

use std::{
    fs,
    path::{Path, PathBuf},
};

use url::Url;

use crate::ui_text;

/// RFC 6761 reserves `.invalid`, so this name can never belong to a real site. The dotted form also
/// avoids the single-label host names Chromium resolves as search terms instead of as a mapping.
pub(crate) const PREVIEW_VIRTUAL_HOST: &str = "mewrk-file-preview.invalid";

pub(crate) const PREVIEWABLE_EXTENSIONS: [&str; 10] = [
    "html", "htm", "svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "pdf",
];

/// Where builds that copied the picked file kept their copies.
const LEGACY_STAGING_ROOT: &str = "file-previews";
const MAX_PREVIEW_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// `SetVirtualHostNameToFolderMapping` documents MAX_PATH as the limit for the mapped folder.
const MAX_MAPPED_FOLDER_CHARS: usize = 260;
const SWEEP_MAX_DIRECTORIES: usize = 64;

/// A picked file, ready to serve: the folder the virtual host maps and the file's address there.
#[derive(Debug)]
pub(crate) struct FilePreviewTarget {
    pub(crate) folder: PathBuf,
    pub(crate) url: Url,
}

/// Validates a user-picked path and names the folder to serve it from.
pub(crate) fn target(picked: &Path) -> Result<FilePreviewTarget, String> {
    let file = validate_picked_file(picked)?;
    let folder = file.parent().ok_or_else(|| {
        ui_text::pick("所选文件没有所在文件夹", "The chosen file has no folder").to_owned()
    })?;
    let folder = mappable_folder(folder)?;
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ui_text::pick(
                "所选文件的名称不是有效的 Unicode",
                "The chosen file's name is not valid Unicode",
            )
            .to_owned()
        })?;
    let url = preview_url(name)?;
    Ok(FilePreviewTarget { folder, url })
}

/// Removes the copies builds before folder serving left under app data.
///
/// Bounded the way the browser-profile startup sweep is: a pathological directory count must not
/// stall startup, and the remainder is collected by the next launch.
pub(crate) fn sweep_orphans(app_data: &Path) {
    let root = app_data.join(LEGACY_STAGING_ROOT);
    let Ok(metadata) = fs::symlink_metadata(&root) else {
        return;
    };
    if is_link_like(&metadata) || !metadata.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    for entry in entries.take(SWEEP_MAX_DIRECTORIES) {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if is_link_like(&metadata) {
            let _ = remove_link_entry(&path, &metadata);
        } else if metadata.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    // Only succeeds once the root is empty, which is when nothing is left to sweep.
    let _ = fs::remove_dir(&root);
}

/// The picked file's canonical path, once it has passed every check.
fn validate_picked_file(picked: &Path) -> Result<PathBuf, String> {
    let unreadable = |error: std::io::Error| {
        ui_text::ui_text!(
            "无法读取所选文件：{error}",
            "Could not read the chosen file: {error}"
        )
    };
    let metadata = fs::symlink_metadata(picked).map_err(unreadable)?;
    if is_link_like(&metadata) {
        return Err(ui_text::pick(
            "不能预览链接形式的文件",
            "A link cannot be previewed; choose the file it points to",
        )
        .to_owned());
    }
    let canonical = fs::canonicalize(picked).map_err(|error| {
        ui_text::ui_text!(
            "无法解析所选文件的路径：{error}",
            "Could not resolve the chosen file's path: {error}"
        )
    })?;
    let canonical = local_volume_path(&canonical)?;
    let metadata = fs::metadata(&canonical).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(ui_text::pick("只能预览普通文件", "Only a regular file can be previewed").to_owned());
    }
    let limit = MAX_PREVIEW_FILE_BYTES / (1024 * 1024);
    if metadata.len() > MAX_PREVIEW_FILE_BYTES {
        return Err(ui_text::ui_text!(
            "所选文件超过 {limit} MiB 的本地预览上限",
            "The chosen file is larger than the {limit} MiB preview limit"
        ));
    }
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    // The dialog filter is advisory — the Windows name box accepts a typed `*.*` — so this is the
    // gate that actually decides what the built-in browser will display.
    if !PREVIEWABLE_EXTENSIONS.contains(&extension.as_str()) {
        return Err(ui_text::ui_text!(
            "内置浏览器只能预览这些类型的文件：{}",
            "The built-in browser can only preview these file types: {}",
            PREVIEWABLE_EXTENSIONS.join(ui_text::pick("、", ", "))
        ));
    }
    Ok(canonical)
}

/// The folder as both platforms can map it: Unicode, on a local volume, within MAX_PATH.
fn mappable_folder(folder: &Path) -> Result<PathBuf, String> {
    let folder = local_volume_path(folder)?;
    let text = folder.to_str().ok_or_else(|| {
        ui_text::pick(
            "所选文件所在文件夹的路径不是有效的 Unicode",
            "The path of the chosen file's folder is not valid Unicode",
        )
        .to_owned()
    })?;
    if text.chars().count() > MAX_MAPPED_FOLDER_CHARS {
        return Err(ui_text::ui_text!(
            "所选文件所在文件夹的路径超过 {MAX_MAPPED_FOLDER_CHARS} 个字符，内置浏览器无法提供",
            "The built-in browser cannot serve a folder whose path is longer than {MAX_MAPPED_FOLDER_CHARS} characters"
        ));
    }
    Ok(folder)
}

fn preview_url(name: &str) -> Result<Url, String> {
    let mut url = Url::parse(&format!("https://{PREVIEW_VIRTUAL_HOST}/"))
        .expect("the preview origin is a valid URL");
    // One segment, percent-encoded: a `#`, `?` or `%` in the name is part of the name.
    url.path_segments_mut()
        .expect("an https URL has path segments")
        .clear()
        .push(name);
    // Nothing in this module may widen what the page WebView admits; the minted address has to
    // clear the same gate the address bar does.
    if !crate::browser::is_navigation_allowed(&url) {
        return Err(ui_text::pick(
            "本地文件预览地址未通过内置浏览器的导航许可",
            "The local file's preview address was refused by the built-in browser's navigation rules",
        )
        .to_owned());
    }
    Ok(url)
}

/// Drops Windows' `\\?\` verbatim prefix and refuses network locations.
///
/// WebView2 rejects a UNC folder mapping outright, and canonicalization is what turns an ordinary
/// drive path into the verbatim form that the mapping API does not understand either.
#[cfg(windows)]
fn local_volume_path(path: &Path) -> Result<PathBuf, String> {
    let network = || {
        ui_text::pick(
            "不能预览网络位置上的文件",
            "A file in a network location cannot be previewed",
        )
        .to_owned()
    };
    let text = path.to_str().ok_or_else(|| {
        ui_text::pick(
            "所选文件的路径不是有效的 Unicode",
            "The chosen file's path is not valid Unicode",
        )
        .to_owned()
    })?;
    if text.starts_with(r"\\?\UNC\") {
        return Err(network());
    }
    let plain = text.strip_prefix(r"\\?\").unwrap_or(text);
    if plain.starts_with(r"\\") {
        return Err(network());
    }
    Ok(PathBuf::from(plain))
}

#[cfg(not(windows))]
fn local_volume_path(path: &Path) -> Result<PathBuf, String> {
    Ok(path.to_path_buf())
}

#[cfg(not(windows))]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn remove_link_entry(path: &Path, _metadata: &fs::Metadata) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link_entry(path: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResolvedLanguage;

    fn write_picked(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, b"preview fixture").unwrap();
        path
    }

    #[test]
    fn every_filtered_extension_is_served_from_its_own_folder() {
        let source = tempfile::tempdir().unwrap();
        let folder = fs::canonicalize(source.path()).unwrap();

        for extension in PREVIEWABLE_EXTENSIONS {
            let picked = write_picked(source.path(), &format!("report.{extension}"));
            let target = target(&picked).unwrap();
            assert_eq!(
                target.url.as_str(),
                format!("https://{PREVIEW_VIRTUAL_HOST}/report.{extension}")
            );
            assert_eq!(target.folder, folder);
        }
        // Nothing is copied anywhere: the folder holds exactly what the test wrote.
        assert_eq!(
            fs::read_dir(source.path()).unwrap().count(),
            PREVIEWABLE_EXTENSIONS.len()
        );
    }

    #[test]
    fn a_file_keeps_its_own_name_so_links_back_to_it_resolve() {
        let source = tempfile::tempdir().unwrap();
        let picked = write_picked(source.path(), "我的 报告 #2.html");

        let target = target(&picked).unwrap();
        assert_eq!(
            target.url.path(),
            "/%E6%88%91%E7%9A%84%20%E6%8A%A5%E5%91%8A%20%232.html"
        );
        assert_eq!(target.url.fragment(), None);
    }

    #[test]
    fn extensions_outside_the_filter_are_refused() {
        let source = tempfile::tempdir().unwrap();

        for name in ["payload.exe", "notes.txt", "readme.md", "noextension"] {
            let picked = write_picked(source.path(), name);
            assert!(target(&picked).is_err(), "unexpectedly served {name}");
        }
    }

    #[test]
    fn an_oversize_file_is_refused() {
        let source = tempfile::tempdir().unwrap();
        let picked = source.path().join("huge.png");
        let file = fs::File::create(&picked).unwrap();
        file.set_len(MAX_PREVIEW_FILE_BYTES + 1).unwrap();
        drop(file);

        assert_eq!(
            target(&picked).unwrap_err(),
            "所选文件超过 64 MiB 的本地预览上限"
        );
    }

    #[test]
    fn a_directory_is_not_a_previewable_file() {
        let source = tempfile::tempdir().unwrap();
        let picked = source.path().join("bundle.html");
        fs::create_dir(&picked).unwrap();

        assert_eq!(target(&picked).err(), Some("只能预览普通文件".to_owned()));
        ui_text::with_language(ResolvedLanguage::EnUs, || {
            assert_eq!(
                target(&picked).err(),
                Some("Only a regular file can be previewed".to_owned())
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_refused_in_the_app_language() {
        let source = tempfile::tempdir().unwrap();
        let real = write_picked(source.path(), "page.html");
        let link = source.path().join("link.html");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        ui_text::with_language(ResolvedLanguage::EnUs, || {
            assert_eq!(
                target(&link).unwrap_err(),
                "A link cannot be previewed; choose the file it points to"
            );
        });
    }

    #[test]
    fn the_minted_url_clears_the_page_navigation_gate() {
        let url = preview_url("report.pdf").unwrap();
        assert!(crate::browser::is_navigation_allowed(&url));
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some(PREVIEW_VIRTUAL_HOST));
    }

    #[test]
    fn the_legacy_sweep_is_bounded_and_leaves_the_remainder_for_the_next_launch() {
        let app_data = tempfile::tempdir().unwrap();
        let root = app_data.path().join(LEGACY_STAGING_ROOT);
        let remainder = 3;
        fs::create_dir_all(&root).unwrap();
        for index in 0..SWEEP_MAX_DIRECTORIES + remainder {
            let directory = root.join(format!("{index:032x}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("staged.png"), b"leftover").unwrap();
        }

        sweep_orphans(app_data.path());
        assert_eq!(fs::read_dir(&root).unwrap().count(), remainder);

        sweep_orphans(app_data.path());
        assert!(!root.exists());
    }

    #[test]
    fn sweeping_a_missing_root_does_nothing() {
        let app_data = tempfile::tempdir().unwrap();
        sweep_orphans(app_data.path());
        assert!(!app_data.path().join(LEGACY_STAGING_ROOT).exists());
    }
}
