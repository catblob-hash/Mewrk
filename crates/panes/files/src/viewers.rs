//! Which viewer an open file gets (upstream's `lib/fileViewers.ts`). The decision is made from the name alone: the host
//! answers with text or with a `binary` flag, neither of which tells a PNG apart from a ZIP.

use crate::icons::file_extension;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileViewerKind {
    Markdown,
    Html,
    Image,
    Pdf,
    Audio,
    Video,
    Font,
    Csv,
    Notebook,
    Code,
    Text,
}

const MARKDOWN: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "mdx", "rmd", "qmd"];
const HTML: &[&str] = &["html", "htm", "xhtml"];
const CSV: &[&str] = &["csv", "tsv", "tab", "psv"];
const VIDEO: &[&str] = &["mp4", "m4v", "mov", "webm", "mkv", "avi", "ogv", "wmv", "flv", "mpg", "mpeg", "3gp"];

fn image_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" | "jpe" | "jfif" | "pjpeg" | "pjp" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" | "dib" => "image/bmp",
        "ico" | "cur" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

fn audio_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp3" => "audio/mpeg",
        "wav" | "wave" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        "aif" | "aiff" => "audio/aiff",
        "caf" => "audio/x-caf",
        _ => return None,
    })
}

fn font_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttc" => "font/collection",
        _ => return None,
    })
}

/// Extension to grammar. A file whose grammar cannot be found falls back to `text` and is shown uncoloured.
fn code_language_of(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" | "pyw" | "pyx" | "pxd" | "mojo" | "gyp" | "bzl" | "star" => "python",
        "rb" | "rake" | "gemspec" | "ru" | "podspec" | "cr" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" | "sbt" => "scala",
        "groovy" | "gradle" | "gvy" => "groovy",
        "dart" => "dart",
        "cs" | "csx" => "csharp",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "tpp" | "inl" | "ino" => "cpp",
        "m" | "mm" => "objectivec",
        "glsl" | "vert" | "frag" | "geom" | "comp" | "tesc" | "tese" | "hlsl" | "fx" | "wgsl" | "metal" | "cu" | "cuh" => "shader",
        "sol" => "solidity",
        "v" | "sv" | "svh" | "vh" => "verilog",
        "vhd" | "vhdl" => "vhdl",
        "zig" | "zon" => "zig",
        "nim" | "nims" | "nimble" => "nim",
        "php" | "phtml" => "php",
        "pl" | "pm" | "pod" => "perl",
        "lua" | "luau" => "lua",
        "r" => "r",
        "jl" => "julia",
        "hs" | "lhs" | "purs" => "haskell",
        "elm" => "elm",
        "ml" | "mli" | "fs" | "fsi" | "fsx" | "re" => "ocaml",
        "ex" | "exs" | "heex" => "elixir",
        "erl" | "hrl" => "erlang",
        "clj" | "cljs" | "cljc" | "edn" | "scm" | "ss" | "rkt" | "lisp" | "lsp" | "el" | "fnl" => "lisp",
        "gleam" => "gleam",
        "tcl" => "tcl",
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "command" | "bats" | "awk" | "env" => "shell",
        "ps1" | "psm1" | "psd1" => "powershell",
        "bat" | "cmd" => "batch",
        "vb" | "vbs" | "bas" | "vba" => "vb",
        "f" | "for" | "f77" | "f90" | "f95" | "f03" | "f08" => "fortran",
        "sql" | "psql" | "pgsql" | "mysql" | "ddl" | "dml" => "sql",
        "css" => "css",
        "scss" | "sass" | "less" | "styl" | "pcss" => "scss",
        "json" | "json5" | "jsonc" | "jsonl" | "ndjson" | "webmanifest" | "har" | "geojson" | "topojson" | "ipynb" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" | "cfg" | "conf" | "properties" | "prefs" | "desktop" | "service" | "editorconfig" | "gitconfig" | "reg" => "ini",
        "html" | "htm" | "xhtml" | "xml" | "svg" | "xsd" | "xsl" | "xslt" | "wsdl" | "rss" | "atom" | "plist" | "xaml" | "csproj" | "fsproj" | "vbproj" | "props" | "targets" | "resx" | "storyboard" | "xib" | "kml" | "gpx" | "vue" | "svelte" | "astro" | "jsp" | "asp" | "aspx" | "cshtml" | "razor" | "erb" | "ejs" | "hbs" | "handlebars" | "mustache" | "njk" | "jinja" | "jinja2" | "j2" | "liquid" | "twig" => "xml",
        "graphql" | "gql" | "proto" | "thrift" | "prisma" => "graphql",
        "tf" | "tfvars" | "hcl" | "nomad" => "hcl",
        "nix" => "nix",
        "tex" | "sty" | "cls" | "ltx" | "bib" => "latex",
        "asm" | "s" | "nasm" => "assembly",
        "cmake" => "cmake",
        "mk" | "mak" => "makefile",
        "dockerfile" => "dockerfile",
        "diff" | "patch" | "rej" => "diff",
        "log" => "log",
        _ => return None,
    })
}

/// Files whose whole name is their type. `file_extension` reports nothing for these.
fn code_filename(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "dockerfile" | "containerfile" => "dockerfile",
        "makefile" | "gnumakefile" | "bsdmakefile" | "justfile" => "makefile",
        "cmakelists.txt" => "cmake",
        "rakefile" | "gemfile" | "brewfile" | "vagrantfile" | "podfile" | "fastfile" | "guardfile" | "capfile" => "ruby",
        "jenkinsfile" => "groovy",
        "cargo.lock" | "poetry.lock" | "pipfile" => "toml",
        "yarn.lock" | "procfile" | ".clang-format" => "yaml",
        "go.mod" => "go",
        "go.sum" => "text",
        ".bashrc" | ".bash_profile" | ".bash_logout" | ".zshrc" | ".zshenv" | ".zprofile" | ".profile" | ".gitignore" | ".gitattributes" | ".dockerignore" | ".npmignore" | ".prettierignore" | ".eslintignore" | ".htaccess" | "codeowners" | ".env" => "shell",
        ".gitmodules" | ".npmrc" | ".yarnrc" | ".editorconfig" | ".gitconfig" => "ini",
        ".babelrc" | ".eslintrc" | ".prettierrc" => "json",
        _ => return None,
    })
}

/// Names a fenced code block may carry that are not also file extensions.
fn fence_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "typescript" => "typescript",
        "javascript" | "node" => "javascript",
        "python" | "python3" => "python",
        "rust" => "rust",
        "golang" => "go",
        "ruby" => "ruby",
        "kotlin" => "kotlin",
        "csharp" | "c#" => "csharp",
        "objective-c" | "objc" => "objectivec",
        "f#" | "fsharp" | "ocaml" => "ocaml",
        "shell" | "console" | "terminal" | "sh" | "shellsession" | "env" | "dotenv" => "shell",
        "powershell" | "pwsh" => "powershell",
        "batch" | "dos" => "batch",
        "yml" => "yaml",
        "html" | "xml" | "vue" | "svelte" => "xml",
        "markdown" | "md" => "markdown",
        "latex" | "tex" => "latex",
        "haskell" => "haskell",
        "elixir" => "elixir",
        "erlang" => "erlang",
        "clojure" | "scheme" | "racket" | "elisp" | "emacs" => "lisp",
        "julia" => "julia",
        "perl" => "perl",
        "fortran" => "fortran",
        "vbnet" | "vba" => "vb",
        "terraform" => "hcl",
        "docker" | "dockerfile" => "dockerfile",
        "make" | "makefile" => "makefile",
        "cmake" => "cmake",
        "protobuf" => "graphql",
        "solidity" => "solidity",
        "verilog" | "systemverilog" => "verilog",
        "glsl" | "hlsl" | "wgsl" | "cuda" => "shader",
        "assembly" | "nasm" => "assembly",
        "diff" | "patch" | "udiff" => "diff",
        "jsonc" | "json5" => "json",
        "ini" => "ini",
        "toml" => "toml",
        "log" => "log",
        _ => return None,
    })
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The grammar id for `path`, or `None` when nothing here claims it.
pub fn code_language(path: &str) -> Option<&'static str> {
    let name = file_name(path);
    let lower = name.to_lowercase();
    if let Some(by_name) = code_filename(&lower) {
        return (by_name != "text").then_some(by_name);
    }
    // `.env.local`, `.env.production`: the name, not the suffix, says what it is.
    if lower.starts_with(".env.") {
        return Some("shell");
    }
    if lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile") {
        return Some("dockerfile");
    }
    let extension = file_extension(name);
    if MARKDOWN.contains(&extension.as_str()) {
        return Some("markdown");
    }
    if CSV.contains(&extension.as_str()) || extension.is_empty() {
        return None;
    }
    code_language_of(&extension)
}

/// The grammar a fenced code block names in its info string, or `None` when it names none known.
pub fn fence_language(info: Option<&str>) -> Option<&'static str> {
    let info = info?;
    let first = info.trim().split(|c: char| c.is_whitespace() || c == '{' || c == ',').next().unwrap_or("").to_lowercase();
    let name = first.strip_prefix('.').unwrap_or(&first);
    if name.is_empty() {
        return None;
    }
    fence_alias(name).or_else(|| code_language_of(name)).or_else(|| code_filename(name))
}

/// The media type an image of this name is served as, or `None` when it is not one.
pub fn image_media_type(path: &str) -> Option<&'static str> {
    image_type(&file_extension(file_name(path)))
}

/// The media type of a file the pane reads as bytes rather than as text, or `None`.
pub fn binary_media_type(path: &str) -> Option<&'static str> {
    let extension = file_extension(file_name(path));
    if extension == "pdf" {
        return Some("application/pdf");
    }
    image_type(&extension).or_else(|| audio_type(&extension)).or_else(|| font_type(&extension))
}

pub fn file_viewer_kind(path: &str) -> FileViewerKind {
    let extension = file_extension(file_name(path));
    let e = extension.as_str();
    if MARKDOWN.contains(&e) {
        FileViewerKind::Markdown
    } else if HTML.contains(&e) {
        FileViewerKind::Html
    } else if image_type(e).is_some() {
        // SVG is both a picture and a document. It is shown as a picture, and the source toggle shows its markup.
        FileViewerKind::Image
    } else if e == "pdf" {
        FileViewerKind::Pdf
    } else if audio_type(e).is_some() {
        FileViewerKind::Audio
    } else if VIDEO.contains(&e) {
        FileViewerKind::Video
    } else if font_type(e).is_some() {
        FileViewerKind::Font
    } else if CSV.contains(&e) {
        FileViewerKind::Csv
    } else if e == "ipynb" {
        FileViewerKind::Notebook
    } else if code_language(path).is_some() {
        FileViewerKind::Code
    } else {
        FileViewerKind::Text
    }
}

/// Whether the file has a rendered form and a source form, so the viewer offers the switch between them.
pub fn has_source_form(path: &str) -> bool {
    match file_viewer_kind(path) {
        FileViewerKind::Image => file_extension(file_name(path)) == "svg",
        FileViewerKind::Markdown | FileViewerKind::Html | FileViewerKind::Csv | FileViewerKind::Notebook => true,
        _ => false,
    }
}

/// Whether the viewer needs the file's bytes rather than its text to show it at all.
pub fn reads_bytes(path: &str) -> bool {
    matches!(file_viewer_kind(path), FileViewerKind::Image | FileViewerKind::Pdf | FileViewerKind::Audio | FileViewerKind::Font)
}

/// Whether a file's text has to be read to show it: everything but the files the pane reads as bytes, and a video, which
/// it cannot show at all. An SVG is both.
pub fn needs_text(path: &str) -> bool {
    if reads_bytes(path) {
        return has_source_form(path);
    }
    file_viewer_kind(path) != FileViewerKind::Video
}

/// Resolves a reference written inside `document_path` against the file it was written in, or returns `None` when it
/// climbs out of the workspace. A rooted reference is taken as workspace-rooted.
pub fn resolve_document_reference(document_path: &str, reference: &str) -> Option<String> {
    let raw = reference.trim();
    if raw.is_empty() {
        return None;
    }
    let rooted = raw.starts_with('/');
    let directory = if rooted { "" } else { &document_path[..document_path.rfind('/').map_or(0, |i| i + 1)] };
    let tail = if rooted { &raw[1..] } else { raw };
    let joined = format!("{directory}{tail}");
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            s => segments.push(s),
        }
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use FileViewerKind as K;

    #[test]
    fn renders_the_markdown_family_and_nothing_that_merely_looks_like_it() {
        for path in ["README.md", "docs/guide.markdown", "a/b.mdx", "SKILL.md"] {
            assert_eq!(file_viewer_kind(path), K::Markdown, "{path}");
        }
        assert_ne!(file_viewer_kind("notes.mdb"), K::Markdown);
    }

    #[test]
    fn shows_the_image_types_a_webview_can_decode_and_no_others() {
        for path in ["a.png", "a.JPG", "logo.svg", "icon.ico", "shot.webp", "photo.heic", "scan.tif"] {
            assert_eq!(file_viewer_kind(path), K::Image, "{path}");
        }
        for path in ["app.icns", "layers.psd"] {
            assert_ne!(file_viewer_kind(path), K::Image, "{path}");
        }
    }

    #[test]
    fn gives_every_kind_of_file_a_viewer_of_its_own() {
        assert_eq!(file_viewer_kind("site/index.html"), K::Html);
        assert_eq!(file_viewer_kind("paper.pdf"), K::Pdf);
        assert_eq!(file_viewer_kind("voice.mp3"), K::Audio);
        assert_eq!(file_viewer_kind("clip.mp4"), K::Video);
        assert_eq!(file_viewer_kind("Inter.woff2"), K::Font);
        assert_eq!(file_viewer_kind("data/table.tsv"), K::Csv);
        assert_eq!(file_viewer_kind("analysis.ipynb"), K::Notebook);
    }

    #[test]
    fn knows_which_files_have_a_source_to_switch_to_and_which_are_read_as_bytes() {
        for path in ["a.md", "a.html", "a.csv", "a.ipynb", "a.svg"] {
            assert!(has_source_form(path), "{path}");
        }
        for path in ["a.png", "a.pdf", "a.ts", "a.txt"] {
            assert!(!has_source_form(path), "{path}");
        }
        for path in ["a.png", "a.pdf", "a.mp3", "a.ttf"] {
            assert!(reads_bytes(path), "{path}");
        }
        for path in ["a.md", "a.mp4", "a.html"] {
            assert!(!reads_bytes(path), "{path}");
        }
        assert_eq!(binary_media_type("a.pdf"), Some("application/pdf"));
        assert_eq!(binary_media_type("a.flac"), Some("audio/flac"));
        assert_eq!(binary_media_type("a.otf"), Some("font/otf"));
    }

    #[test]
    fn claims_a_language_only_where_it_has_a_grammar_for_one() {
        assert_eq!(file_viewer_kind("src/App.tsx"), K::Code);
        assert_eq!(code_language("src/App.tsx"), Some("tsx"));
        assert_eq!(code_language("src/main.ts"), Some("typescript"));
        for (path, language) in [("lib/a.dart", "dart"), ("a.kt", "kotlin"), ("a.scala", "scala"), ("build.gradle", "groovy"), ("a.hs", "haskell"), ("a.ex", "elixir"), ("a.clj", "lisp"), ("a.zig", "zig"), ("a.ps1", "powershell"), ("run.bat", "batch"), ("main.tf", "hcl"), ("flake.nix", "nix"), ("paper.tex", "latex"), ("fix.patch", "diff"), ("shader.frag", "shader"), ("a.sol", "solidity"), ("a.jl", "julia"), ("setup.cfg", "ini"), ("a.scss", "scss"), ("server.log", "log"), ("a.m", "objectivec")] {
            assert_eq!(code_language(path), Some(language), "{path}");
            assert_eq!(file_viewer_kind(path), K::Code, "{path}");
        }
        assert_eq!(file_viewer_kind("Cargo.toml"), K::Code);
        assert_eq!(file_viewer_kind("notes.txt"), K::Text);
        assert_eq!(code_language("notes.txt"), None);
    }

    #[test]
    fn recognizes_the_files_that_are_named_rather_than_suffixed() {
        assert_eq!(code_language("Dockerfile"), Some("dockerfile"));
        assert_eq!(code_language("Dockerfile.dev"), Some("dockerfile"));
        assert_eq!(code_language("build/Makefile"), Some("makefile"));
        assert_eq!(code_language("CMakeLists.txt"), Some("cmake"));
        assert_eq!(code_language("Cargo.lock"), Some("toml"));
        assert_eq!(code_language(".gitignore"), Some("shell"));
        assert_eq!(code_language(".env.local"), Some("shell"));
        assert_eq!(code_language("LICENSE"), None);
    }

    #[test]
    fn resolves_against_the_document_not_the_workspace_root() {
        assert_eq!(resolve_document_reference("docs/guide.md", "./images/a.png").as_deref(), Some("docs/images/a.png"));
        assert_eq!(resolve_document_reference("docs/guide.md", "../assets/a.png").as_deref(), Some("assets/a.png"));
        assert_eq!(resolve_document_reference("docs/guide.md", "other.md").as_deref(), Some("docs/other.md"));
        assert_eq!(resolve_document_reference("readme.md", "docs/guide.md").as_deref(), Some("docs/guide.md"));
        assert_eq!(resolve_document_reference("docs/deep/guide.md", "/src/App.tsx").as_deref(), Some("src/App.tsx"));
    }

    #[test]
    fn refuses_a_reference_that_climbs_out_of_the_workspace() {
        assert_eq!(resolve_document_reference("readme.md", "../outside.md"), None);
        assert_eq!(resolve_document_reference("docs/guide.md", "../../outside.md"), None);
        assert_eq!(resolve_document_reference("readme.md", "  "), None);
    }

    #[test]
    fn names_the_type_a_data_url_has_to_carry() {
        assert_eq!(image_media_type("a.png"), Some("image/png"));
        assert_eq!(image_media_type("a.JPEG"), Some("image/jpeg"));
        assert_eq!(image_media_type("a.svg"), Some("image/svg+xml"));
        assert_eq!(image_media_type("a.txt"), None);
    }

    #[test]
    fn reads_the_name_a_fence_gives_as_an_alias_or_as_an_extension() {
        assert_eq!(fence_language(Some("ts")), Some("typescript"));
        assert_eq!(fence_language(Some("tsx title=\"a.tsx\"")), Some("tsx"));
        assert_eq!(fence_language(Some("Python")), Some("python"));
        assert_eq!(fence_language(Some("console")), Some("shell"));
        assert_eq!(fence_language(Some("c#")), Some("csharp"));
        assert_eq!(fence_language(Some("diff")), Some("diff"));
        assert_eq!(fence_language(Some("klingon")), None);
        assert_eq!(fence_language(Some("")), None);
        assert_eq!(fence_language(None), None);
    }
}
