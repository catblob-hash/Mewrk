//! The remote file tools', language servers', instruction files', project
//! memory's and Git status probe's scripts, in PowerShell.
//!
//! A Windows machine whose agent shell is PowerShell runs these where a POSIX
//! machine runs the `sh` scripts in [`crate::remote_files`],
//! [`crate::remote_lsp`], [`crate::remote_instructions`] and
//! [`crate::remote_memory`]. They keep exactly the same contract, so everything
//! the host does with an answer is shared: the same two header lines (the
//! canonical root, then the canonical target), the same payloads byte for
//! byte, and the same reserved exit codes (64 root missing, 65 outside the
//! root, 66 not found, 67 wrong kind, 68 too large, 69 changed since the probe,
//! 70 write failed, 2 bad pattern, 127 language server not found).
//!
//! What differs is only how a Windows machine is asked:
//!
//! * **Paths** come back as `C:/Users/…` — forward slashes, drive letter
//!   first — which is what the rest of the host already reads a Windows path
//!   as (`lsp_servers::path_to_uri` turns it into `file:///C:/…`). A path the
//!   model or an older workspace record spells the Git Bash way (`/c/…`,
//!   `/cygdrive/c/…`) or with `~` is read as the Windows path it names.
//! * **Canonical** means every symbolic link and junction resolved, walking the
//!   path one component at a time, because confinement has to be decided on
//!   the real location, as it is on POSIX. Case is compared the way NTFS
//!   compares it: insensitively.
//! * **Output** is written as bytes to the standard output stream, never
//!   through PowerShell's formatter, so a file's bytes arrive as they are on
//!   disk and text lines are UTF-8 whatever the console code page.
//! * **Input** (the bytes a write puts in place) is read from the raw input
//!   handle: Windows PowerShell's `[Console]::OpenStandardInput()` never
//!   returns when the input is already waiting as it starts to read, the same
//!   trap the agent upload avoids.
//! * **Regular expressions** are .NET's, which, like the host leg's Rust
//!   `regex`, are Perl-shaped.
//!
//! PowerShell names are case-insensitive: `$c` and `$C` are one variable, so
//! a loop variable at a script's top level must not share a name with the
//! prologue's `$ROOT`, `$REQ`, `$T` and `$C` in any case.
//!
//! Every host-supplied operand is a single-quoted PowerShell literal
//! ([`crate::remote_shell::ps_single_quote`]), refused first when it holds a
//! control character, so nothing a model sends can become code.

use crate::remote_shell::ps_single_quote;

/// Where a script acts: the workspace root as recorded, and whether a path
/// outside it is refused — all but the files in `also`, the canonical paths of
/// the files the conversation's instruction files import on the machine.
pub(crate) struct Target<'a> {
    pub root: &'a str,
    pub confine: bool,
    pub also: &'a [String],
}

/// Whether a script's target must already exist.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Existing,
    ForWrite,
}

/// Helpers every script starts with: strict errors, UTF-8 byte output, the
/// path spellings a Windows machine may be named with, and canonicalization.
const HELPERS: &str = r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}
$MewrkOut = [Console]::OpenStandardOutput()
function Out-Line([string]$Text) { $b = [System.Text.Encoding]::UTF8.GetBytes($Text + "`n"); $MewrkOut.Write($b, 0, $b.Length) }
function Out-Bytes([byte[]]$Bytes) { if ($Bytes.Length -gt 0) { $MewrkOut.Write($Bytes, 0, $Bytes.Length) } }
function Out-Err([string]$Text) { [Console]::Error.WriteLine($Text) }
function Quit([int]$Code) { $MewrkOut.Flush(); exit $Code }
function Slash([string]$P) { $P.Replace('\', '/') }
function Native-Path([string]$P) {
  if ($P -eq '~') { return $HOME }
  if ($P.StartsWith('~/') -or $P.StartsWith('~\')) { return [System.IO.Path]::Combine($HOME, $P.Substring(2)) }
  if ($P -match '^/cygdrive/([A-Za-z])(/.*)?$' -or $P -match '^/([A-Za-z])(/.*)?$') {
    $rest = if ($Matches[2]) { $Matches[2] } else { '/' }
    return $Matches[1].ToUpper() + ':' + $rest
  }
  return $P
}
function Canon([string]$P) {
  $full = [System.IO.Path]::GetFullPath($P)
  for ($hop = 0; $hop -lt 40; $hop++) {
    $root = [System.IO.Path]::GetPathRoot($full)
    $parts = @($full.Substring($root.Length).Split([char[]]@('\', '/'), [System.StringSplitOptions]::RemoveEmptyEntries))
    $cur = $root
    $restart = $null
    for ($i = 0; $i -lt $parts.Count; $i++) {
      $next = [System.IO.Path]::Combine($cur, $parts[$i])
      $item = Get-Item -LiteralPath $next -Force -ErrorAction SilentlyContinue
      if ($null -eq $item) {
        $cur = $next
        for ($j = $i + 1; $j -lt $parts.Count; $j++) { $cur = [System.IO.Path]::Combine($cur, $parts[$j]) }
        break
      }
      if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -and $item.Target) {
        $link = [string](@($item.Target)[0])
        if (-not [System.IO.Path]::IsPathRooted($link)) { $link = [System.IO.Path]::Combine($cur, $link) }
        for ($j = $i + 1; $j -lt $parts.Count; $j++) { $link = [System.IO.Path]::Combine($link, $parts[$j]) }
        $restart = [System.IO.Path]::GetFullPath($link)
        break
      }
      $cur = $next
    }
    if ($null -eq $restart) {
      $out = (Slash $cur).TrimEnd('/')
      if ($out -match '^[A-Za-z]:$') { $out += '/' }
      return $out
    }
    $full = $restart
  }
  return $null
}
function Inside([string]$Path, [string]$Root) {
  if ($Path -ieq $Root) { return $true }
  return $Path.StartsWith($Root.TrimEnd('/') + '/', [System.StringComparison]::OrdinalIgnoreCase)
}
function Unix-Seconds([System.IO.FileInfo]$File) { return ([DateTimeOffset]$File.LastWriteTimeUtc).ToUnixTimeSeconds() }
function Sha256-Hex([byte[]]$Bytes) {
  $sha = [System.Security.Cryptography.SHA256]::Create()
  try { return ([System.BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
}
"#;

/// Enters the root, or exits 64. `~` and the Git Bash spellings are read as
/// the Windows paths they name.
fn enter_root(root: &str) -> String {
    format!(
        "try {{ Set-Location -LiteralPath (Native-Path {}) }} catch {{ Out-Err $_.Exception.Message; Quit 64 }}\n",
        ps_single_quote(root.trim())
    )
}

/// The shared prologue: enter the root, resolve and canonicalize the requested
/// path, test confinement, then announce both — the POSIX prologue's contract.
pub(crate) fn prologue(target: &Target<'_>, path: &str, mode: Mode) -> String {
    let mut script = String::with_capacity(HELPERS.len() + 1024);
    script.push_str(HELPERS);
    script.push_str(&enter_root(target.root));
    script.push_str("$ROOT = Canon (Get-Location).ProviderPath\nif (-not $ROOT) { Quit 64 }\n");
    script.push_str(&format!("$REQ = {}\n", ps_single_quote(path)));
    script.push_str(
        "$T = Native-Path $REQ\n\
         if (-not [System.IO.Path]::IsPathRooted($T)) { $T = [System.IO.Path]::Combine($ROOT, $T) }\n",
    );
    if mode == Mode::Existing {
        script.push_str(
            "try { $there = [System.IO.File]::Exists($T) -or [System.IO.Directory]::Exists($T) } catch { $there = $false }\n\
             if (-not $there) { Quit 66 }\n",
        );
    }
    script.push_str("try { $C = Canon $T } catch { Out-Err $_.Exception.Message; Quit 66 }\nif (-not $C) { Quit 66 }\n");
    if target.confine {
        let mut inside = String::from("(Inside $C $ROOT)");
        for path in target.also {
            inside.push_str(&format!(" -or (Inside $C {})", ps_single_quote(path)));
        }
        script.push_str(&format!("if (-not ({inside})) {{ Out-Err $C; Quit 65 }}\n"));
    }
    script.push_str("Out-Line $ROOT\nOut-Line $C\n");
    script
}

/// What Git says about the target, the PowerShell form of the POSIX
/// `IGNORE_PROBE`: `$Ign` becomes `git`, `root` or `names`
/// ([`crate::search_scope::take_remote_rules`]). With `list`, `$Ignored`
/// holds what `git ls-files` lists as ignored below the target.
///
/// Windows PowerShell turns a native command's redirected stderr into error
/// records, which `$ErrorActionPreference = 'Stop'` would make fatal, so the
/// preference is relaxed around the calls. Every argument is a fixed token:
/// the target is the current location, never an argument, because 5.1
/// re-splits what it hands a native program on spaces and quotes.
fn ignore_probe(list: bool) -> String {
    let listing = if list {
        "\n      $MewrkLines = & git -c core.fsmonitor=false -c core.quotepath=false ls-files --others --ignored --exclude-standard --directory -- . 2>$null\n      if ($LASTEXITCODE -eq 0) { $Ignored = @($MewrkLines | Where-Object { $_ }) } else { $Ign = 'names'; $Ignored = @() }"
    } else {
        ""
    };
    format!(
        r#"$Ign = 'names'
$Ignored = @()
if ([System.IO.Directory]::Exists($C) -and (Get-Command git -CommandType Application -ErrorAction SilentlyContinue)) {{
  $MewrkEap = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  foreach ($MewrkVar in @('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_COMMON_DIR', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_NAMESPACE', 'GIT_CEILING_DIRECTORIES', 'GIT_CONFIG', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_COUNT')) {{ Remove-Item -LiteralPath ('Env:' + $MewrkVar) -ErrorAction SilentlyContinue }}
  $env:GIT_OPTIONAL_LOCKS = '0'
  Push-Location -LiteralPath $C
  try {{
    & git -c core.fsmonitor=false check-ignore -q . 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) {{ $Ign = 'root' }}
    elseif ($LASTEXITCODE -eq 1) {{
      $Ign = 'git'{listing}
    }}
  }} catch {{ $Ign = 'names'; $Ignored = @() }} finally {{ Pop-Location; $ErrorActionPreference = $MewrkEap }}
}}
"#
    )
}

/// The ignore section of an `ls` or `find` answer: the mode, the ignored
/// entries, an empty line.
const IGNORE_SECTION: &str = "Out-Line $Ign\nforeach ($MewrkEntry in $Ignored) { Out-Line $MewrkEntry }\nOut-Line ''\n";

/// `Collapsed $Shown $Name`: whether the walk leaves a directory unexpanded —
/// version-control metadata always, and what the ignore mode says. The
/// shown path is the one the walk prints, `$C` plus `/`-joined names.
fn collapsed_function() -> String {
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|name| ps_single_quote(name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        r#"$MewrkVcs = @({vcs})
$MewrkDeps = @({deps})
$MewrkPrune = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
foreach ($MewrkEntry in $Ignored) {{
  if ($MewrkEntry.EndsWith('/') -and -not $MewrkEntry.StartsWith('"')) {{ [void]$MewrkPrune.Add($C.TrimEnd('/') + '/' + $MewrkEntry.TrimEnd('/')) }}
}}
function Collapsed([string]$Shown, [string]$Name) {{
  if ($MewrkVcs -contains $Name) {{ return $true }}
  if ($Ign -eq 'names' -and $MewrkDeps -contains $Name) {{ return $true }}
  return $MewrkPrune.Contains($Shown)
}}
"#,
        vcs = quoted(&crate::search_scope::VCS_DIRECTORIES),
        deps = quoted(crate::search_scope::DEPENDENCY_DIRECTORIES),
    )
}

/// Walks a directory the way the POSIX leg's `find` does: pre-order, one entry
/// per line, directories marked with a trailing `/`, links to directories
/// neither marked nor descended into, version-control metadata listed but not
/// entered, at most `limit` lines.
///
/// `name_filter`, when given, is a `-like` pattern the printed entries' names
/// must match; every directory is still walked, as `find -name` walks them.
/// It is a pre-filter for the host's own glob matcher, and PowerShell's
/// `-like` ignores case, so it only ever lets more through, never less.
fn walk(limit: usize, name_filter: Option<&str>) -> String {
    let filter = match name_filter {
        Some(pattern) => format!(
            "($it.Name -like {})",
            ps_single_quote(&pattern.replace('`', "``"))
        ),
        None => "$true".to_owned(),
    };
    format!(
        r#"$global:MewrkLeft = {limit}
function Walk([string]$Dir, [string]$Shown) {{
  try {{ $items = ([System.IO.DirectoryInfo]::new($Dir)).GetFileSystemInfos() }} catch {{ return }}
  foreach ($it in $items) {{
    if ($global:MewrkLeft -le 0) {{ return }}
    $path = $Shown + '/' + $it.Name
    $isDir = ($it -is [System.IO.DirectoryInfo]) -and -not ($it.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
    if ({filter}) {{
      if ($isDir) {{ Out-Line ($path + '/') }} else {{ Out-Line $path }}
      $global:MewrkLeft--
    }}
    if ($isDir -and -not ($MewrkVcs -contains $it.Name)) {{ Walk $it.FullName $path }}
  }}
}}
Walk $C $C.TrimEnd('/')
Quit 0
"#
    )
}

/// `ls`: the entries under the target, `max_depth` levels deep, breadth-first
/// and each directory in name order, at most `limit` lines — so the cut falls
/// on the deepest level reached, as on the host. Ignored directories are
/// printed but not entered.
pub(crate) fn listing(target: &Target<'_>, path: &str, max_depth: u64, limit: usize) -> String {
    let mut script = prologue(target, path, Mode::Existing);
    script.push_str("if (-not [System.IO.Directory]::Exists($C)) { Quit 67 }\n");
    script.push_str(&ignore_probe(true));
    script.push_str(IGNORE_SECTION);
    script.push_str(&collapsed_function());
    script.push_str(&format!(
        r#"$global:MewrkLeft = {limit}
$MewrkLevel = [System.Collections.Generic.List[object]]::new()
$MewrkLevel.Add(@($C, $C.TrimEnd('/')))
for ($MewrkDepth = 1; $MewrkDepth -le {max_depth} -and $MewrkLevel.Count -gt 0; $MewrkDepth++) {{
  $MewrkNext = [System.Collections.Generic.List[object]]::new()
  foreach ($MewrkDir in $MewrkLevel) {{
    try {{ $items = @(([System.IO.DirectoryInfo]::new($MewrkDir[0])).GetFileSystemInfos() | Sort-Object -Property Name) }} catch {{ continue }}
    foreach ($it in $items) {{
      if ($global:MewrkLeft -le 0) {{ Quit 0 }}
      $path = $MewrkDir[1] + '/' + $it.Name
      $isDir = ($it -is [System.IO.DirectoryInfo]) -and -not ($it.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
      if ($isDir) {{ Out-Line ($path + '/') }} else {{ Out-Line $path }}
      $global:MewrkLeft--
      if ($isDir -and -not (Collapsed $path $it.Name)) {{ $MewrkNext.Add(@($it.FullName, $path)) }}
    }}
  }}
  $MewrkLevel = $MewrkNext
}}
Quit 0
"#
    ));
    script
}

/// `find`: every entry under the target, optionally pre-filtered by name,
/// after the ignore section the host ranks the matches by.
pub(crate) fn find(target: &Target<'_>, path: &str, name_filter: Option<&str>, limit: usize) -> String {
    let mut script = prologue(target, path, Mode::Existing);
    script.push_str(&ignore_probe(true));
    script.push_str(IGNORE_SECTION);
    script.push_str(&format!(
        "$MewrkVcs = @({})\n",
        crate::search_scope::VCS_DIRECTORIES
            .iter()
            .map(|name| ps_single_quote(name))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    script.push_str(&walk(limit, name_filter));
    script
}

/// `grep`: `path:line:text` for every matching line, files over 2 MiB and
/// binary files (a NUL in the first 8000 bytes, as `grep -I` decides) skipped
/// under a directory, at most `limit` lines. Inside a Git work tree the files
/// are the ones `git ls-files` shows; otherwise the walk skips what
/// `Collapsed` names. A bad pattern exits 2 with .NET's complaint on stderr;
/// an unreadable file is reported on stderr and skipped.
pub(crate) fn grep(
    target: &Target<'_>,
    path: &str,
    pattern: &str,
    case_sensitive: bool,
    limit: usize,
) -> String {
    let options = if case_sensitive {
        "CultureInvariant"
    } else {
        "CultureInvariant, IgnoreCase"
    };
    let mut script = prologue(target, path, Mode::Existing);
    script.push_str(&format!(
        r#"$Pattern = {pattern}
$Options = [System.Text.RegularExpressions.RegexOptions]'{options}'
try {{ $Rx = [System.Text.RegularExpressions.Regex]::new($Pattern, $Options) }} catch {{
  $why = $_.Exception
  if ($why.InnerException) {{ $why = $why.InnerException }}
  Out-Err $why.Message
  Quit 2
}}
# A whole-file test first, so files with no match cost one regex pass. It is
# skipped for anchors that mean something different in a whole file.
$Prefilter = $null
if ($Pattern -notmatch '\\[AzZG]|\(\?<[=!]') {{ $Prefilter = [System.Text.RegularExpressions.Regex]::new($Pattern, $Options -bor [System.Text.RegularExpressions.RegexOptions]::Multiline) }}
$Utf8 = [System.Text.UTF8Encoding]::new($false, $false)
$global:MewrkLeft = {limit}
function Scan([string]$File, [string]$Shown, [bool]$SkipBinary) {{
  try {{ $bytes = [System.IO.File]::ReadAllBytes($File) }} catch {{ Out-Err ('grep: ' + $Shown + ': ' + $_.Exception.Message); return }}
  if ($SkipBinary -and [Array]::IndexOf($bytes, [byte]0, 0, [Math]::Min($bytes.Length, 8000)) -ge 0) {{ return }}
  $text = $Utf8.GetString($bytes).Replace("`r`n", "`n")
  if ($null -ne $Prefilter -and -not $Prefilter.IsMatch($text)) {{ return }}
  $lines = $text.Split("`n")
  for ($n = 0; $n -lt $lines.Length; $n++) {{
    if ($Rx.IsMatch($lines[$n])) {{
      Out-Line ($Shown + ':' + ($n + 1) + ':' + $lines[$n])
      $global:MewrkLeft--
      if ($global:MewrkLeft -le 0) {{ Quit 0 }}
    }}
  }}
}}
function Tree([string]$Dir, [string]$Shown) {{
  try {{ $items = ([System.IO.DirectoryInfo]::new($Dir)).GetFileSystemInfos() }} catch {{ Out-Err ('grep: ' + $Shown + ': ' + $_.Exception.Message); return }}
  foreach ($it in $items) {{
    if ($it.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {{ continue }}
    $path = $Shown + '/' + $it.Name
    if ($it -is [System.IO.DirectoryInfo]) {{ if (-not (Collapsed $path $it.Name)) {{ Tree $it.FullName $path }} }}
    elseif ($it.Length -le 2097152) {{ Scan $it.FullName $path $true }}
  }}
}}
"#,
        pattern = ps_single_quote(pattern),
    ));
    script.push_str(&ignore_probe(false));
    script.push_str(&collapsed_function());
    script.push_str(
        r#"if (-not [System.IO.Directory]::Exists($C)) { Scan $C $C $true; Quit 0 }
if ($Ign -eq 'git') {
  $MewrkEap = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  Push-Location -LiteralPath $C
  try { $MewrkFiles = @(& git -c core.fsmonitor=false -c core.quotepath=false ls-files --cached --others --exclude-standard -- . 2>$null) } finally { Pop-Location; $ErrorActionPreference = $MewrkEap }
  if ($LASTEXITCODE -ne 0) { $Ign = 'names' }
}
if ($Ign -ne 'git') { Tree $C $C.TrimEnd('/'); Quit 0 }
# A path whose directory has become a junction since Git indexed it is not
# followed, as the walk would not have followed it.
$MewrkLinked = @{}
function Linked([string]$Relative) {
  $cut = $Relative.LastIndexOf('/')
  if ($cut -lt 0) { return $false }
  $dir = $Relative.Substring(0, $cut)
  if ($MewrkLinked.ContainsKey($dir)) { return $MewrkLinked[$dir] }
  $info = [System.IO.DirectoryInfo]::new([System.IO.Path]::Combine($C, $dir))
  $answer = (Linked $dir) -or -not $info.Exists -or [bool]($info.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
  $MewrkLinked[$dir] = $answer
  return $answer
}
$MewrkPrev = $null
foreach ($MewrkFile in $MewrkFiles) {
  if (-not $MewrkFile -or $MewrkFile.StartsWith('"') -or $MewrkFile -eq $MewrkPrev) { continue }
  $MewrkPrev = $MewrkFile
  if (Linked $MewrkFile) { continue }
  try { $MewrkInfo = [System.IO.FileInfo]::new([System.IO.Path]::Combine($C, $MewrkFile)) } catch { continue }
  if (-not $MewrkInfo.Exists -or ($MewrkInfo.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or $MewrkInfo.Length -gt 2097152) { continue }
  Scan $MewrkInfo.FullName ($C.TrimEnd('/') + '/' + $MewrkFile) $true
}
Quit 0
"#,
    );
    script
}

/// `read`: the modification time in seconds, the size, then the bytes. A file
/// over `max_bytes` exits 68, a non-file 67.
pub(crate) fn read(target: &Target<'_>, path: &str, max_bytes: usize) -> String {
    let mut script = prologue(target, path, Mode::Existing);
    script.push_str(&format!(
        "if (-not [System.IO.File]::Exists($C)) {{ Quit 67 }}\n\
         $F = [System.IO.FileInfo]::new($C)\n\
         if ($F.Length -gt {max_bytes}) {{ Quit 68 }}\n\
         $B = [System.IO.File]::ReadAllBytes($C)\n\
         Out-Line (Unix-Seconds $F)\n\
         Out-Line $B.Length\n\
         Out-Bytes $B\n\
         Quit 0\n"
    ));
    script
}

/// The first trip of `write` and `edit`: `absent`, `file` or `other`, the
/// modification time, a fingerprint (time and SHA-256), then `text` and the
/// bytes when the file is at most `max_text` bytes, or `none`.
pub(crate) fn probe(target: &Target<'_>, path: &str, max_text: u64) -> String {
    let mut script = prologue(target, path, Mode::ForWrite);
    script.push_str(&format!(
        r#"if ([System.IO.File]::Exists($C)) {{
  $F = [System.IO.FileInfo]::new($C)
  $B = [System.IO.File]::ReadAllBytes($C)
  $MT = Unix-Seconds $F
  Out-Line 'file'
  Out-Line $MT
  Out-Line ([string]$MT + ' ' + (Sha256-Hex $B))
  if ($B.Length -le {max_text}) {{ Out-Line 'text'; Out-Bytes $B }} else {{ Out-Line 'none' }}
}} elseif ([System.IO.Directory]::Exists($C)) {{
  Out-Line 'other'; Out-Line '0'; Out-Line 'other'; Out-Line 'none'
}} else {{
  Out-Line 'absent'; Out-Line '0'; Out-Line 'absent'; Out-Line 'none'
}}
Quit 0
"#
    ));
    script
}

/// Plan mode's question about a write target: `repository` when Git would
/// count writing it as a change — tracked, or inside a work tree and not
/// ignored — and `free` otherwise, Git missing included. The POSIX form is
/// `remote_files::REPOSITORY_PROBE`.
///
/// The path goes to `git check-ignore --stdin` rather than on the command line,
/// because Windows PowerShell 5.1 re-splits a native program's arguments on
/// spaces and quotes; `-q` is not allowed with `--stdin`, so the listing is
/// dropped instead and only the status read.
pub(crate) fn repository_probe(target: &Target<'_>, path: &str) -> String {
    let mut script = prologue(target, path, Mode::ForWrite);
    script.push_str(
        r#"$MewrkAnswer = 'free'
$A = [System.IO.Path]::GetDirectoryName($C)
$R = [System.IO.Path]::GetFileName($C)
while ($A -and -not [System.IO.Directory]::Exists($A)) {
  $R = [System.IO.Path]::GetFileName($A) + '/' + $R
  $A = [System.IO.Path]::GetDirectoryName($A)
}
if ($A -and (Get-Command git -CommandType Application -ErrorAction SilentlyContinue)) {
  $MewrkEap = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  foreach ($MewrkVar in @('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_COMMON_DIR', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_NAMESPACE', 'GIT_CEILING_DIRECTORIES', 'GIT_CONFIG', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_COUNT')) { Remove-Item -LiteralPath ('Env:' + $MewrkVar) -ErrorAction SilentlyContinue }
  $env:GIT_OPTIONAL_LOCKS = '0'
  Push-Location -LiteralPath $A
  try {
    $R | & git -c core.fsmonitor=false check-ignore --stdin 2>$null | Out-Null
    if ($LASTEXITCODE -eq 1) { $MewrkAnswer = 'repository' }
  } catch { $MewrkAnswer = 'free' } finally { Pop-Location; $ErrorActionPreference = $MewrkEap }
}
Out-Line $MewrkAnswer
Quit 0
"#,
    );
    script
}

/// Reads the whole of standard input as bytes.
const READ_INPUT: &str = r#"function Read-Input {
  if ($PSVersionTable.PSVersion.Major -ge 6) { $in = [Console]::OpenStandardInput() } else {
    $native = [Console].Assembly.GetType('Microsoft.Win32.Win32Native')
    $get = if ($native) { $native.GetMethod('GetStdHandle', [System.Reflection.BindingFlags]'NonPublic, Static') }
    if ($get) { $handle = $get.Invoke($null, @([int]-10)) } else {
      Add-Type -Namespace MewrkInput -Name Native -MemberDefinition '[DllImport("kernel32.dll")] public static extern IntPtr GetStdHandle(int n);'
      $handle = [MewrkInput.Native]::GetStdHandle(-10)
    }
    $in = New-Object System.IO.FileStream((New-Object Microsoft.Win32.SafeHandles.SafeFileHandle($handle, $false)), ([System.IO.FileAccess]::Read))
  }
  $buffer = New-Object System.IO.MemoryStream
  $in.CopyTo($buffer)
  return ,$buffer.ToArray()
}
"#;

/// A script that runs the script it is given on standard input, for a script
/// too long to travel on the command line itself (see
/// `run_environment::run_remote_script`). The script is UTF-8 and runs in this
/// process, so its `exit` is the process's exit status.
pub(crate) fn run_script_from_input() -> String {
    format!("{READ_INPUT}Invoke-Expression ([System.Text.Encoding]::UTF8.GetString((Read-Input)))\n")
}

/// The compare-and-swap write: refuses with 69 unless the file is still what
/// the probe fingerprinted, then puts standard input in place through a
/// temporary file beside it and prints the new modification time.
pub(crate) fn cas_write(target: &Target<'_>, path: &str, fingerprint: &str) -> String {
    let mut script = prologue(target, path, Mode::ForWrite);
    script.push_str(READ_INPUT);
    script.push_str(&format!(
        r#"$FP = {fingerprint}
if ($FP -eq 'absent') {{
  if ([System.IO.File]::Exists($C) -or [System.IO.Directory]::Exists($C)) {{ Quit 69 }}
}} else {{
  if (-not [System.IO.File]::Exists($C)) {{ Quit 69 }}
  $current = [string](Unix-Seconds ([System.IO.FileInfo]::new($C))) + ' ' + (Sha256-Hex ([System.IO.File]::ReadAllBytes($C)))
  if ($current -ne $FP) {{ Quit 69 }}
}}
$D = [System.IO.Path]::GetDirectoryName([System.IO.Path]::GetFullPath($C))
try {{ [void][System.IO.Directory]::CreateDirectory($D) }} catch {{ Out-Err $_.Exception.Message; Quit 70 }}
$TMP = [System.IO.Path]::Combine($D, '.mewrk-write.' + $PID)
try {{
  [System.IO.File]::WriteAllBytes($TMP, (Read-Input))
  if ([System.IO.File]::Exists($C)) {{
    try {{ [System.IO.File]::Replace($TMP, $C, $null) }} catch {{ [System.IO.File]::Copy($TMP, $C, $true); [System.IO.File]::Delete($TMP) }}
  }} else {{
    [System.IO.File]::Move($TMP, $C)
  }}
}} catch {{
  Out-Err $_.Exception.Message
  if ([System.IO.File]::Exists($TMP)) {{ try {{ [System.IO.File]::Delete($TMP) }} catch {{}} }}
  Quit 70
}}
Out-Line (Unix-Seconds ([System.IO.FileInfo]::new($C)))
Quit 0
"#,
        fingerprint = ps_single_quote(fingerprint),
    ));
    script
}

/// The `lsp` probe: the prologue, the remote home, the project's and the
/// user's configuration file as counted blocks (`path`, byte count, bytes — or
/// `-`), the installed preset commands one per line ended by an empty line,
/// then the file — the POSIX probe's layout.
pub(crate) fn lsp_probe(
    target: &Target<'_>,
    path: &str,
    max_file: u64,
    max_config: u64,
    config_paths: &[String; 2],
    presets: &[&str],
) -> String {
    let mut script = prologue(target, path, Mode::Existing);
    let presets = presets
        .iter()
        .map(|command| ps_single_quote(command))
        .collect::<Vec<_>>()
        .join(", ");
    script.push_str(&format!(
        r#"if (-not [System.IO.File]::Exists($C)) {{ Quit 67 }}
if (([System.IO.FileInfo]::new($C)).Length -gt {max_file}) {{ Quit 68 }}
Out-Line (Slash $HOME)
function Emit([string]$Dir) {{
  foreach ($rel in @({preferred}, {legacy})) {{
    $f = [System.IO.Path]::Combine($Dir, $rel)
    if ([System.IO.File]::Exists($f)) {{
      $b = [System.IO.File]::ReadAllBytes($f)
      if ($b.Length -le {max_config}) {{ Out-Line (Slash $f); Out-Line $b.Length; Out-Bytes $b }} else {{ Out-Line '-' }}
      return
    }}
  }}
  Out-Line '-'
}}
Emit $ROOT
Emit (Slash $HOME)
foreach ($preset in @({presets})) {{ if (Get-Command -Name $preset -CommandType Application -ErrorAction SilentlyContinue) {{ Out-Line $preset }} }}
Out-Line ''
Out-Bytes ([System.IO.File]::ReadAllBytes($C))
Quit 0
"#,
        preferred = ps_single_quote(&config_paths[0]),
        legacy = ps_single_quote(&config_paths[1]),
    ));
    script
}

/// `git check-ignore` over `paths` in `root`; git's own exit status.
pub(crate) fn check_ignore(root: &str, paths: &[String]) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&enter_root(root));
    script.push_str("& git check-ignore --");
    for path in paths {
        script.push(' ');
        script.push_str(&ps_single_quote(path));
    }
    script.push_str("\nQuit $LASTEXITCODE\n");
    script
}

/// A file's current bytes, for re-syncing a live server's copy: 66 when it is
/// not a file, 68 when it is over `max_bytes`.
pub(crate) fn read_text(canonical: &str, max_bytes: u64) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&format!(
        "$f = Native-Path {}\n\
         if (-not [System.IO.File]::Exists($f)) {{ Quit 66 }}\n\
         $b = [System.IO.File]::ReadAllBytes($f)\n\
         if ($b.Length -gt {max_bytes}) {{ Quit 68 }}\n\
         Out-Bytes $b\n\
         Quit 0\n",
        ps_single_quote(canonical)
    ));
    script
}

/// Exit 0 when either spelling of the language-server configuration exists
/// under the root, 1 when neither does, 64 when the root cannot be entered.
pub(crate) fn declares(root: &str, config_paths: &[String; 2]) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&enter_root(root));
    script.push_str(&format!(
        "$R = (Get-Location).ProviderPath\n\
         if ([System.IO.File]::Exists([System.IO.Path]::Combine($R, {})) -or [System.IO.File]::Exists([System.IO.Path]::Combine($R, {}))) {{ Quit 0 }}\n\
         Quit 1\n",
        ps_single_quote(&config_paths[0]),
        ps_single_quote(&config_paths[1]),
    ));
    script
}

/// Starts a language server in `root`: the entry's variables set, the command
/// found as an application (127 when it is not), then run as the last thing on
/// the line — which PowerShell does not pipe through itself, so the server's
/// standard streams are the agent's own, byte for byte, as the proxy's are
/// when the bootstrap starts it the same way.
pub(crate) fn lsp_launch(
    root: &str,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> String {
    let mut script = String::from(
        "$ErrorActionPreference = 'Stop'\n$ProgressPreference = 'SilentlyContinue'\n",
    );
    // Only the helpers the launch needs: nothing may touch standard output
    // before the server owns it.
    script.push_str(
        "function Native-Path([string]$P) {\n\
         if ($P -eq '~') { return $HOME }\n\
         if ($P.StartsWith('~/') -or $P.StartsWith('~\\')) { return [System.IO.Path]::Combine($HOME, $P.Substring(2)) }\n\
         if ($P -match '^/cygdrive/([A-Za-z])(/.*)?$' -or $P -match '^/([A-Za-z])(/.*)?$') { $rest = if ($Matches[2]) { $Matches[2] } else { '/' }; return $Matches[1].ToUpper() + ':' + $rest }\n\
         return $P\n\
         }\n",
    );
    script.push_str(&format!(
        "try {{ Set-Location -LiteralPath (Native-Path {}) }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 64 }}\n\
         [System.Environment]::CurrentDirectory = (Get-Location).ProviderPath\n",
        ps_single_quote(root.trim())
    ));
    for (key, value) in env {
        script.push_str(&format!(
            "[System.Environment]::SetEnvironmentVariable({}, {})\n",
            ps_single_quote(key),
            ps_single_quote(value)
        ));
    }
    script.push_str(&format!(
        "$server = Get-Command -Name {} -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1\n\
         if ($null -eq $server) {{ [Console]::Error.WriteLine({}); exit 127 }}\n\
         & $server.Source",
        ps_single_quote(command),
        ps_single_quote(&format!("{command} is not on the remote PATH")),
    ));
    for arg in args {
        script.push(' ');
        script.push_str(&ps_single_quote(arg));
    }
    script.push_str("\nexit $LASTEXITCODE\n");
    script
}

/// The Git status probe of [`crate::remote_git`]: the POSIX probe's reads, in
/// its framing, byte for byte.
///
/// Each read runs Git as a process whose standard output is copied as bytes,
/// never through PowerShell's pipeline, which would decode `status -z`'s NULs
/// and rewrite its line endings. The tracked diffs are digested here with
/// .NET's SHA-256 rather than by `git hash-object`: the digest is opaque to
/// the host, so either serves. Every argument is fixed text without spaces or
/// quotes, so the command line needs no Windows quoting.
pub(crate) fn git_status_probe(root: &str) -> String {
    use crate::remote_git::{
        BRANCHES_FORMAT, DIGEST_DIFF, GIT_ENVIRONMENT, GIT_PREFIX, OPERATIONS, PROBE_MAGIC,
    };
    let mut script = String::from(HELPERS);
    script.push_str(&enter_root(root));
    let cleared = GIT_ENVIRONMENT
        .iter()
        .map(|name| ps_single_quote(name))
        .collect::<Vec<_>>()
        .join(", ");
    script.push_str(&format!(
        r#"$gitCommand = Get-Command git -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
if ($null -eq $gitCommand) {{ Out-Err 'git: command not found'; Quit 127 }}
$MewrkGit = $gitCommand.Path
$MewrkCwd = (Get-Location).ProviderPath
foreach ($name in @({cleared})) {{ Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }}
$env:GIT_OPTIONAL_LOCKS = '0'; $env:GIT_TERMINAL_PROMPT = '0'; $env:GIT_PAGER = 'cat'; $env:PAGER = 'cat'; $env:LC_ALL = 'C'; $env:LANG = 'C'
function Emit([string]$Name, [int]$Code, $Out, $Err) {{
  if ($null -eq $Out) {{ $Out = New-Object byte[] 0 }}
  if ($null -eq $Err) {{ $Err = New-Object byte[] 0 }}
  Out-Line ('{{0}} {{1}} {{2}} {{3}}' -f $Name, $Code, $Out.Length, $Err.Length)
  Out-Bytes $Out
  Out-Bytes $Err
}}
function Git-Run([string]$Arguments) {{
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $MewrkGit
  $psi.Arguments = {prefix} + ' ' + $Arguments
  $psi.WorkingDirectory = $MewrkCwd
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardInput = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $proc = [System.Diagnostics.Process]::Start($psi)
  $proc.StandardInput.Close()
  $errText = $proc.StandardError.ReadToEndAsync()
  $buffer = New-Object System.IO.MemoryStream
  $proc.StandardOutput.BaseStream.CopyTo($buffer)
  $proc.WaitForExit()
  return @{{ Code = $proc.ExitCode; Out = $buffer.ToArray(); Err = [System.Text.Encoding]::UTF8.GetBytes($errText.Result) }}
}}
function Run-Read([string]$Name, [string]$Arguments) {{
  $read = Git-Run $Arguments
  Emit $Name $read.Code $read.Out $read.Err
}}
function Run-Digest([string]$Name, [string]$Arguments) {{
  $read = Git-Run $Arguments
  $out = $read.Out
  if ($null -eq $out) {{ $out = New-Object byte[] 0 }}
  Emit $Name $read.Code ([System.Text.Encoding]::UTF8.GetBytes((Sha256-Hex $out))) $read.Err
}}
Out-Line {magic}
$top = Git-Run 'rev-parse --show-prefix --show-toplevel --absolute-git-dir --git-common-dir'
Emit 'rev-parse' $top.Code $top.Out $top.Err
if ($top.Code -ne 0) {{ Quit 0 }}
$lines = [System.Text.Encoding]::UTF8.GetString($top.Out).Split([char]10)
if ($lines.Count -lt 4 -or $lines[0].TrimEnd([char]13) -ne '') {{ Quit 0 }}
$gitDir = $lines[2].TrimEnd([char]13)
Run-Read 'version' '--version'
Run-Read 'status' 'status --porcelain=v2 -z --branch --show-stash --untracked-files=all'
$hasHead = Git-Run 'rev-parse -q --verify HEAD'
if ($hasHead.Code -eq 0) {{ Run-Read 'numstat' 'diff --no-ext-diff --no-textconv --numstat -z HEAD' }} else {{ Run-Read 'numstat' 'diff --no-ext-diff --no-textconv --numstat -z --cached' }}
Run-Digest 'staged-digest' {staged}
Run-Digest 'unstaged-digest' {unstaged}
Run-Read 'branches' {branches}
Run-Read 'upstream-oid' 'rev-parse -q --verify @{{upstream}}^{{commit}}'
Run-Read 'remotes' 'remote -v'
function State-Exists([string]$Relative) {{ return (Test-Path -LiteralPath ([System.IO.Path]::Combine($gitDir, $Relative))) }}
$operation = ''
$stateFiles = @()
"#,
        prefix = ps_single_quote(GIT_PREFIX),
        magic = ps_single_quote(PROBE_MAGIC),
        staged = ps_single_quote(&format!("{DIGEST_DIFF} --cached --")),
        unstaged = ps_single_quote(&format!("{DIGEST_DIFF} --")),
        branches = ps_single_quote(&format!("for-each-ref {BRANCHES_FORMAT} refs/heads")),
    ));
    for (index, (label, markers, files)) in OPERATIONS.iter().enumerate() {
        let test = markers
            .iter()
            .map(|marker| format!("(State-Exists {})", ps_single_quote(marker)))
            .collect::<Vec<_>>()
            .join(" -or ");
        let files = files
            .iter()
            .map(|file| ps_single_quote(file))
            .collect::<Vec<_>>()
            .join(", ");
        script.push_str(if index == 0 { "if (" } else { "} elseif (" });
        script.push_str(&format!(
            "{test}) {{ $operation = {}; $stateFiles = @({files}) ",
            ps_single_quote(label)
        ));
    }
    script.push_str(
        r#"}
$state = $operation + "`n"
foreach ($stateFile in $stateFiles) {
  $statePath = [System.IO.Path]::Combine($gitDir, $stateFile)
  if ([System.IO.File]::Exists($statePath)) { $state += $stateFile + ' ' + (Sha256-Hex ([System.IO.File]::ReadAllBytes($statePath))) + "`n" }
}
Emit 'operation' 0 ([System.Text.Encoding]::UTF8.GetBytes($state)) $null
Quit 0
"#,
    );
    script
}

/// Everything a workspace's `.mewrk` gives discovery, in the format the POSIX
/// probe of [`crate::remote_capabilities`] writes: the header, `windows`, the
/// machine's environment as one counted block, `mcp.json` and `hooks.json`
/// each as a path, a count and that many bytes (or `-`), the skills directory
/// read (or `-`), one `S` record per skill — its folder name, manifest path and
/// size, then the bytes when it is within `max_skill` — and `E`; then the
/// agents directory read (or `-`), one `A` record per role file — its name,
/// path and size, then the bytes when it is within `max_agent` — and `E`.
///
/// A folder, manifest or role file that is a reparse point is passed over, as
/// the local scan passes over a link: what enters the model's context is not
/// handed to whoever owns the link's target.
pub(crate) fn capabilities_probe(
    root: &str,
    max_config: u64,
    max_skill: u64,
    max_agent: u64,
) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&enter_root(root));
    script.push_str(&format!(
        r#"$R = (Get-Location).ProviderPath
Out-Line 'mewrk-capabilities 2'
Out-Line 'windows'
$envText = ((Get-ChildItem env: | ForEach-Object {{ $_.Name + '=' + $_.Value }}) -join "`n") + "`n"
$envBytes = [System.Text.Encoding]::UTF8.GetBytes($envText)
Out-Line $envBytes.Length
Out-Bytes $envBytes
function Pick([string]$Rel) {{
  $p = [System.IO.Path]::Combine($R, '.mewrk', $Rel)
  if (Test-Path -LiteralPath $p) {{ return $p }}
  return [System.IO.Path]::Combine($R, '.naiword', $Rel)
}}
function Emit-File([string]$Rel) {{
  $f = Pick $Rel
  if ([System.IO.File]::Exists($f)) {{
    $b = [System.IO.File]::ReadAllBytes($f)
    if ($b.Length -le {max_config}) {{ Out-Line (Slash $f); Out-Line $b.Length; Out-Bytes $b; return }}
  }}
  Out-Line '-'
}}
Emit-File 'mcp.json'
Emit-File 'hooks.json'
$S = Pick 'skills'
if ([System.IO.Directory]::Exists($S)) {{
  Out-Line (Slash $S)
  foreach ($d in ([System.IO.DirectoryInfo]::new($S)).GetDirectories()) {{
    if ($d.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {{ continue }}
    if ($d.Name.Contains("`n")) {{ continue }}
    $m = [System.IO.Path]::Combine($d.FullName, 'SKILL.md')
    if (-not [System.IO.File]::Exists($m)) {{ continue }}
    $mi = [System.IO.FileInfo]::new($m)
    if ($mi.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {{ continue }}
    Out-Line 'S'
    Out-Line $d.Name
    Out-Line (Slash $m)
    if ($mi.Length -le {max_skill}) {{ $b = [System.IO.File]::ReadAllBytes($m); Out-Line $b.Length; Out-Bytes $b }} else {{ Out-Line $mi.Length }}
  }}
}} else {{ Out-Line '-' }}
Out-Line 'E'
$A = Pick 'agents'
if ([System.IO.Directory]::Exists($A)) {{
  Out-Line (Slash $A)
  foreach ($f in ([System.IO.DirectoryInfo]::new($A)).GetFiles()) {{
    if ($f.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {{ continue }}
    if (-not $f.Name.EndsWith('.json', [System.StringComparison]::OrdinalIgnoreCase)) {{ continue }}
    if ($f.Name.Contains("`n")) {{ continue }}
    Out-Line 'A'
    Out-Line $f.Name
    Out-Line (Slash $f.FullName)
    if ($f.Length -le {max_agent}) {{ $b = [System.IO.File]::ReadAllBytes($f.FullName); Out-Line $b.Length; Out-Bytes $b }} else {{ Out-Line $f.Length }}
  }}
}} else {{ Out-Line '-' }}
Out-Line 'E'
Quit 0
"#
    ));
    script
}

/// Creates `relative` under the workspace root (with every parent) and prints
/// the directory it made or found, slash-spelled.
pub(crate) fn ensure_directory(root: &str, relative: &str) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&enter_root(root));
    script.push_str(&format!(
        "$d = [System.IO.Path]::Combine((Get-Location).ProviderPath, {})\n\
         [void][System.IO.Directory]::CreateDirectory($d)\n\
         Out-Line (Slash $d)\n\
         Quit 0\n",
        ps_single_quote(relative)
    ));
    script
}

/// Replaces `path` with standard input through a temporary file beside it.
pub(crate) fn replace_file(path: &str) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(READ_INPUT);
    script.push_str(&format!(
        "$f = Native-Path {}\n\
         $t = $f + '.mewrk-' + [System.Guid]::NewGuid().ToString('N') + '.tmp'\n\
         [System.IO.File]::WriteAllBytes($t, (Read-Input))\n\
         try {{ Move-Item -LiteralPath $t -Destination $f -Force }} catch {{ Remove-Item -LiteralPath $t -Force -ErrorAction SilentlyContinue; Out-Err $_.Exception.Message; Quit 73 }}\n\
         Quit 0\n",
        ps_single_quote(path)
    ));
    script
}

/// Creates `path` from standard input through a temporary file beside it,
/// unless something — a file, a folder, a link, a dangling one included — is
/// there already, which exits `taken` and writes nothing. The final move is
/// `File.Move`, which never replaces anything: what appears at the path in
/// the meantime is refused the same way.
pub(crate) fn create_file(path: &str, taken: i32) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(READ_INPUT);
    script.push_str(&format!(
        "$f = Native-Path {}\n\
         function Taken {{ ($null -ne (Get-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue)) -or [System.IO.File]::Exists($f) -or [System.IO.Directory]::Exists($f) }}\n\
         if (Taken) {{ Quit {taken} }}\n\
         $t = $f + '.mewrk-' + [System.Guid]::NewGuid().ToString('N') + '.tmp'\n\
         [System.IO.File]::WriteAllBytes($t, (Read-Input))\n\
         try {{ [System.IO.File]::Move($t, $f) }} catch {{ Remove-Item -LiteralPath $t -Force -ErrorAction SilentlyContinue; if (Taken) {{ Quit {taken} }}; Out-Err $_.Exception.Message; Quit 73 }}\n\
         Quit 0\n",
        ps_single_quote(path)
    ));
    script
}

/// Deletes the skill folder `directory` of `skills_root` for good, refusing
/// anything that is not a plain directory directly inside it (66).
pub(crate) fn remove_skill(skills_root: &str, directory: &str) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&format!(
        "$d = [System.IO.Path]::Combine((Native-Path {}), {})\n\
         $item = Get-Item -LiteralPath $d -Force -ErrorAction SilentlyContinue\n\
         if ($null -eq $item -or -not $item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {{ Quit 66 }}\n\
         Remove-Item -LiteralPath $d -Recurse -Force\n\
         Quit 0\n",
        ps_single_quote(skills_root),
        ps_single_quote(directory)
    ));
    script
}

/// Deletes the file `path` for good, refusing a folder or a link where the file
/// should be (66); a file that is already gone is not an error.
pub(crate) fn remove_file(path: &str) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&format!(
        "$f = Native-Path {}\n\
         $item = Get-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue\n\
         if ($null -ne $item) {{\n\
         if ($item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {{ Quit 66 }}\n\
         Remove-Item -LiteralPath $f -Force\n\
         }}\n\
         Quit 0\n",
        ps_single_quote(path)
    ));
    script
}

/// Runs a hook's `command` in `cwd` with `env` set, its standard input the
/// event, and exits with the command's own status.
pub(crate) fn hook_command(cwd: &str, env: &[(String, String)], command: &str) -> String {
    let mut script = format!(
        "try {{ Set-Location -LiteralPath {} }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 64 }}\n\
         [System.Environment]::CurrentDirectory = (Get-Location).ProviderPath\n",
        ps_single_quote(cwd.trim())
    );
    for (key, value) in env {
        script.push_str(&format!(
            "[System.Environment]::SetEnvironmentVariable({}, {})\n",
            ps_single_quote(key),
            ps_single_quote(value)
        ));
    }
    script.push_str(&crate::shell_backend::remote_powershell_command(command));
    script
}

/// The instruction probe of [`crate::remote_instructions`]: the POSIX
/// script's records (see `remote_instructions::posix_script`), in its framing,
/// byte for byte.
///
/// A link is a reparse point with a target — a symbolic link or a junction —
/// as `Canon` resolves them; any other reparse point (a cloud placeholder,
/// say) is read as the file or folder it presents. Paths are reported the way
/// `Canon` spells them, `C:/…`, which is what the host maps into its mirror.
pub(crate) fn instruction_probe(
    job: &crate::remote_instructions::Job<'_>,
    limits: &crate::remote_instructions::Limits,
) -> String {
    use crate::remote_instructions::Job;

    let mut script = String::from(HELPERS);
    if let Job::Startup { root, .. } = job {
        script.push_str(&enter_root(root));
    }
    script.push_str(&format!(
        r#"$global:MewrkBudget = {budget}
$global:MewrkLeft = {entries}
function Join-Remote([string]$Dir, [string]$Name) {{ if ($Dir.EndsWith('/')) {{ return $Dir + $Name }}; return $Dir + '/' + $Name }}
function Get-Entry([string]$Path) {{ try {{ return Get-Item -LiteralPath $Path -Force -ErrorAction Stop }} catch {{ return $null }} }}
function Test-Link($Entry) {{ return ($null -ne $Entry) -and [bool]($Entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -and [bool]$Entry.Target }}
function Emit-File([string]$Path) {{
  try {{ $size = ([System.IO.FileInfo]::new($Path)).Length }} catch {{ Out-Line 'U'; Out-Line $Path; return }}
  if ($size -le {max_file} -and $size -le $global:MewrkBudget) {{
    try {{ $bytes = [System.IO.File]::ReadAllBytes($Path) }} catch {{ Out-Line 'U'; Out-Line $Path; return }}
    Out-Line 'F'; Out-Line $Path; Out-Line ([string]$bytes.Length); Out-Bytes $bytes
    $global:MewrkBudget -= $bytes.Length
  }} else {{ Out-Line 'B'; Out-Line $Path; Out-Line ([string]$size) }}
}}
function Report-Item([string]$Path, [string]$Kind) {{
  Out-Line 'S'; Out-Line $Kind; Out-Line $Path
  if (Test-Link (Get-Entry $Path)) {{
    $target = Canon $Path
    if (-not $target -or $target -ieq $Path) {{ return }}
    Out-Line 'L'; Out-Line $Path; Out-Line $target
    Out-Line 'S'; Out-Line $Kind; Out-Line $target
    Report-Holds $target $Kind
  }} else {{ Report-Holds $Path $Kind }}
}}
function Report-Holds([string]$Path, [string]$Kind) {{
  if ($Kind -eq 'f') {{ if ([System.IO.File]::Exists($Path)) {{ Emit-File $Path }} }}
  elseif ($Kind -eq 'd') {{
    if ([System.IO.Directory]::Exists($Path)) {{
      Out-Line 'D'; Out-Line $Path
      Report-Item (Join-Remote $Path 'MEWRK.md') 'f'
      Report-Item (Join-Remote $Path 'rules') 't'
    }}
  }} elseif ([System.IO.Directory]::Exists($Path)) {{ Walk-Rules $Path 0 }}
}}
function Walk-Rules([string]$Dir, [int]$Depth) {{
  Out-Line 'D'; Out-Line $Dir
  if ($Depth -ge {depth}) {{ return }}
  try {{ $entries = ([System.IO.DirectoryInfo]::new($Dir)).GetFileSystemInfos() }} catch {{ return }}
  foreach ($entry in $entries) {{
    if ($global:MewrkLeft -le 0) {{ return }}
    $global:MewrkLeft--
    $path = Join-Remote $Dir $entry.Name
    $markdown = $entry.Name -match '.\.md$'
    if (Test-Link $entry) {{
      if (-not $markdown) {{ continue }}
      $target = Canon $path
      if (-not $target -or $target -ieq $path) {{ continue }}
      Out-Line 'L'; Out-Line $path; Out-Line $target; Out-Line 'S'; Out-Line 'f'; Out-Line $target
      if ([System.IO.File]::Exists($target)) {{ Emit-File $target }}
    }} elseif ($entry -is [System.IO.DirectoryInfo]) {{ Walk-Rules $path ($Depth + 1) }}
    elseif ($markdown) {{ Emit-File $path }}
  }}
}}
function Scan-Folder([string]$Dir) {{
  Out-Line 'D'; Out-Line $Dir
  Report-Item (Join-Remote $Dir 'MEWRK.md') 'f'
  Report-Item (Join-Remote $Dir 'MEWRK.local.md') 'f'
  Report-Item (Join-Remote $Dir '.mewrk') 'd'
}}
function Fetch-Path([string]$Path) {{
  $Path = $Path.Replace('\', '/')
  if ($Path -notmatch '^[A-Za-z]:/') {{ return }}
  $cur = $Path.Substring(0, 2).ToUpperInvariant() + '/'
  $parts = @($Path.Substring(3).Split([char[]]@('/'), [System.StringSplitOptions]::RemoveEmptyEntries))
  for ($i = 0; $i -lt $parts.Count; $i++) {{
    $next = Join-Remote $cur $parts[$i]
    $entry = Get-Entry $next
    if (Test-Link $entry) {{
      Out-Line 'S'; Out-Line 'f'; Out-Line $next
      $target = Canon $next
      if (-not $target -or $target -ieq $next) {{ return }}
      Out-Line 'L'; Out-Line $next; Out-Line $target
      $cur = $target
    }} elseif ($entry -is [System.IO.DirectoryInfo]) {{ Out-Line 'D'; Out-Line $next; $cur = $next }}
    elseif ($i -lt $parts.Count - 1) {{ Out-Line 'S'; Out-Line 'f'; Out-Line $next; return }}
    else {{ $cur = $next }}
  }}
  Out-Line 'S'; Out-Line 'f'; Out-Line $cur
  if ([System.IO.File]::Exists($cur)) {{ Emit-File $cur }} elseif ([System.IO.Directory]::Exists($cur)) {{ Out-Line 'D'; Out-Line $cur }}
}}
"#,
        budget = limits.budget,
        entries = limits.entries,
        max_file = limits.max_file,
        depth = limits.depth,
    ));
    let header = ps_single_quote(crate::remote_instructions::HEADER);
    match job {
        Job::Startup { project_folder, .. } => {
            script.push_str(&format!(
                r#"$MewrkRoot = Canon (Get-Location).ProviderPath
if (-not $MewrkRoot) {{ Out-Err 'cannot enter the workspace root'; Quit 64 }}
Out-Line {header}
Out-Line 'R'; Out-Line $MewrkRoot
$MewrkTop = $null
$MewrkDir = $MewrkRoot
while ($MewrkDir) {{
  if (Get-Entry (Join-Remote $MewrkDir '.git')) {{ $MewrkTop = $MewrkDir; break }}
  $MewrkUp = [System.IO.Path]::GetDirectoryName($MewrkDir)
  if (-not $MewrkUp) {{ break }}
  $MewrkDir = (Slash $MewrkUp).TrimEnd('/')
  if ($MewrkDir -match '^[A-Za-z]:$') {{ $MewrkDir += '/' }}
}}
if (-not $MewrkTop) {{ $MewrkTop = $MewrkRoot }}
Out-Line 'T'; Out-Line $MewrkTop
$MewrkCur = $MewrkTop
Scan-Folder $MewrkCur
foreach ($MewrkName in @($MewrkRoot.Substring($MewrkTop.Length).Split([char[]]@('/'), [System.StringSplitOptions]::RemoveEmptyEntries))) {{
  $MewrkCur = Join-Remote $MewrkCur $MewrkName
  Scan-Folder $MewrkCur
}}
"#
            ));
            if let Some(folder) = project_folder {
                script.push_str(&format!(
                    "try {{\n  Set-Location -LiteralPath (Native-Path {})\n  $MewrkProject = Canon (Get-Location).ProviderPath\n  if ($MewrkProject) {{ Out-Line 'P'; Out-Line $MewrkProject; Out-Line 'D'; Out-Line $MewrkProject; Report-Item (Join-Remote $MewrkProject 'MEWRK.local.md') 'f' }}\n}} catch {{}}\n",
                    ps_single_quote(folder.trim())
                ));
            }
        }
        Job::Folders(folders) => {
            script.push_str(&format!(
                "Out-Line {header}\nforeach ($MewrkDir in @({})) {{\n  if (-not [System.IO.Directory]::Exists($MewrkDir) -or (Test-Link (Get-Entry $MewrkDir))) {{ break }}\n  $MewrkActual = Canon $MewrkDir\n  if (-not $MewrkActual -or $MewrkActual -ine $MewrkDir) {{ break }}\n  Scan-Folder $MewrkDir\n}}\n",
                quoted_list(folders),
            ));
        }
        Job::Paths(paths) => {
            script.push_str(&format!(
                "Out-Line {header}\nforeach ($MewrkPath in @({})) {{ Fetch-Path $MewrkPath }}\n",
                quoted_list(paths),
            ));
        }
    }
    script.push_str("Out-Line 'E'\nQuit 0\n");
    script
}

fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| ps_single_quote(item))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What every memory script of [`crate::remote_memory`] starts with: the
/// workspace entered, `$MewrkTier` its `.mewrk` and `$MewrkMemory` that
/// folder's `memory`. `Kind-Of` answers 0 (nothing), 1 (a real folder), 2 (a
/// link or other reparse point), 3 (a regular file); `Fingerprint` is a
/// file's modification second beside its SHA-256 — opaque to the host, which
/// only ever hands it back to the same machine.
fn memory_prologue(root: &str) -> String {
    let mut script = String::from(HELPERS);
    script.push_str(&enter_root(root));
    script.push_str(
        r#"$MewrkTier = [System.IO.Path]::Combine((Get-Location).ProviderPath, '.mewrk')
$MewrkMemory = [System.IO.Path]::Combine($MewrkTier, 'memory')
function Kind-Of([string]$Path) {
  try { $entry = Get-Item -LiteralPath $Path -Force -ErrorAction Stop } catch { return 0 }
  if ($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) { return 2 }
  if ($entry -is [System.IO.DirectoryInfo]) { return 1 }
  return 3
}
function Fingerprint([string]$Path) { return [string](Unix-Seconds ([System.IO.FileInfo]::new($Path))) + ' ' + (Sha256-Hex ([System.IO.File]::ReadAllBytes($Path))) }
"#,
    );
    script
}

/// The memory snapshot, the POSIX script's layout (see
/// `remote_memory::posix_snapshot`).
pub(crate) fn memory_snapshot(root: &str, max_index: usize) -> String {
    let mut script = memory_prologue(root);
    script.push_str(&format!(
        r#"Out-Line 'mewrk-memory 1'
$MewrkTierKind = Kind-Of $MewrkTier
$MewrkMemoryKind = 0
if ($MewrkTierKind -eq 1) {{ $MewrkMemoryKind = Kind-Of $MewrkMemory }}
if ($MewrkTierKind -gt 1 -or $MewrkMemoryKind -gt 1) {{ Out-Line 'occupied'; Out-Line 'E'; Quit 0 }}
if ($MewrkMemoryKind -ne 1) {{ Out-Line 'empty'; Out-Line 'E'; Quit 0 }}
Out-Line 'ready'
$MewrkIndex = [System.IO.Path]::Combine($MewrkMemory, 'MEMORY.md')
$MewrkIndexKind = Kind-Of $MewrkIndex
if ($MewrkIndexKind -eq 0) {{ Out-Line 'absent' }}
elseif ($MewrkIndexKind -ne 3) {{ Out-Line 'other' }}
else {{
  $bytes = [System.IO.File]::ReadAllBytes($MewrkIndex)
  $print = [string](Unix-Seconds ([System.IO.FileInfo]::new($MewrkIndex))) + ' ' + (Sha256-Hex $bytes)
  if ($bytes.Length -le {max_index}) {{ Out-Line 'index'; Out-Line $print; Out-Line ([string]$bytes.Length); Out-Bytes $bytes }}
  else {{ Out-Line 'large'; Out-Line $print }}
}}
foreach ($entry in ([System.IO.DirectoryInfo]::new($MewrkMemory)).GetFiles('*.md')) {{
  if ($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {{ continue }}
  if (-not $entry.Name.EndsWith('.md', [System.StringComparison]::Ordinal) -or $entry.Name -eq 'MEMORY.md') {{ continue }}
  Out-Line $entry.Name
}}
Out-Line 'E'
Quit 0
"#
    ));
    script
}

/// One memory document: 66 when there is none (or a link stands in for it or
/// a folder on the way), 68 when it is over `max_bytes`, otherwise its
/// fingerprint, size and bytes.
pub(crate) fn memory_read(root: &str, name: &str, max_bytes: usize) -> String {
    let mut script = memory_prologue(root);
    script.push_str(&format!(
        r#"$MewrkDocument = [System.IO.Path]::Combine($MewrkMemory, {name})
if ((Kind-Of $MewrkTier) -ne 1 -or (Kind-Of $MewrkMemory) -ne 1 -or (Kind-Of $MewrkDocument) -ne 3) {{ Quit 66 }}
$bytes = [System.IO.File]::ReadAllBytes($MewrkDocument)
if ($bytes.Length -gt {max_bytes}) {{ Quit 68 }}
Out-Line ([string](Unix-Seconds ([System.IO.FileInfo]::new($MewrkDocument))) + ' ' + (Sha256-Hex $bytes))
Out-Line ([string]$bytes.Length)
Out-Bytes $bytes
Quit 0
"#,
        name = ps_single_quote(name),
    ));
    script
}

/// The compare-and-swap write of a memory file, the POSIX script's contract
/// (see `remote_memory::posix_write`): 67, 69 or 70, else the new
/// fingerprint.
pub(crate) fn memory_write(root: &str, name: &str, expected: &str) -> String {
    let mut script = memory_prologue(root);
    script.push_str(READ_INPUT);
    script.push_str(&format!(
        r#"function Make-Folder([string]$Path) {{
  $kind = Kind-Of $Path
  if ($kind -eq 1) {{ return }}
  if ($kind -ne 0) {{ Quit 67 }}
  try {{ [void][System.IO.Directory]::CreateDirectory($Path) }} catch {{ Out-Err $_.Exception.Message; Quit 70 }}
  if ((Kind-Of $Path) -ne 1) {{ Quit 70 }}
}}
Make-Folder $MewrkTier
Make-Folder $MewrkMemory
$MewrkDocument = [System.IO.Path]::Combine($MewrkMemory, {name})
$MewrkExpected = {expected}
$MewrkKind = Kind-Of $MewrkDocument
if ($MewrkExpected -eq 'absent') {{
  if ($MewrkKind -ne 0) {{ Quit 69 }}
}} else {{
  if ($MewrkKind -eq 0) {{ Quit 69 }}
  if ($MewrkKind -ne 3) {{ Quit 67 }}
  if ((Fingerprint $MewrkDocument) -ne $MewrkExpected) {{ Quit 69 }}
}}
$MewrkTemporary = [System.IO.Path]::Combine($MewrkMemory, '.mewrk-write.' + $PID)
try {{
  [System.IO.File]::WriteAllBytes($MewrkTemporary, (Read-Input))
  if ([System.IO.File]::Exists($MewrkDocument)) {{
    try {{ [System.IO.File]::Replace($MewrkTemporary, $MewrkDocument, $null) }} catch {{ [System.IO.File]::Copy($MewrkTemporary, $MewrkDocument, $true); [System.IO.File]::Delete($MewrkTemporary) }}
  }} else {{
    [System.IO.File]::Move($MewrkTemporary, $MewrkDocument)
  }}
}} catch {{
  Out-Err $_.Exception.Message
  if ([System.IO.File]::Exists($MewrkTemporary)) {{ try {{ [System.IO.File]::Delete($MewrkTemporary) }} catch {{}} }}
  Quit 70
}}
Out-Line (Fingerprint $MewrkDocument)
Quit 0
"#,
        name = ps_single_quote(name),
        expected = ps_single_quote(expected),
    ));
    script
}

/// Removes a memory file only while it still has `fingerprint`; 67 when a
/// link stands in the way, 69 when the file changed.
pub(crate) fn memory_remove(root: &str, name: &str, fingerprint: &str) -> String {
    let mut script = memory_prologue(root);
    script.push_str(&format!(
        r#"if ((Kind-Of $MewrkTier) -eq 2 -or (Kind-Of $MewrkMemory) -eq 2) {{ Quit 67 }}
$MewrkDocument = [System.IO.Path]::Combine($MewrkMemory, {name})
$MewrkKind = Kind-Of $MewrkDocument
if ($MewrkKind -eq 0) {{ Quit 0 }}
if ($MewrkKind -ne 3) {{ Quit 67 }}
if ((Fingerprint $MewrkDocument) -ne {fingerprint}) {{ Quit 69 }}
[System.IO.File]::Delete($MewrkDocument)
Quit 0
"#,
        name = ps_single_quote(name),
        fingerprint = ps_single_quote(fingerprint),
    ));
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_git_probe_reads_what_the_posix_probe_reads_in_its_framing() {
        let script = git_status_probe("C:/Users/dev/it's");
        assert!(
            script.contains("Native-Path 'C:/Users/dev/it''s'"),
            "{script}"
        );
        for section in [
            "'rev-parse'",
            "'version'",
            "'status'",
            "'numstat'",
            "'staged-digest'",
            "'unstaged-digest'",
            "'branches'",
            "'upstream-oid'",
            "'remotes'",
            "'operation'",
        ] {
            assert!(script.contains(section), "{section}");
        }
        assert!(script.contains("Quit 127"));
        assert!(script.contains("} elseif ((State-Exists 'rebase-merge') -or (State-Exists 'rebase-apply')) { $operation = 'rebase'"), "{script}");
        // Standard output is copied as bytes, never read as text.
        assert!(script.contains("StandardOutput.BaseStream.CopyTo"));
        // Nothing the script calls leaves an object on the pipeline, where
        // PowerShell's formatter would print it into the answer.
        assert!(!script.contains("return $read"));
    }

    /// The capabilities probe writes the skills section and then the agents
    /// section, each ending in `E`, and passes a role file that is a link, or
    /// is not `*.json`, over.
    #[test]
    fn the_capabilities_probe_ends_with_an_agents_section() {
        let script = capabilities_probe("C:/Users/dev/it's", 1000, 2000, 3000);
        assert!(script.contains("Native-Path 'C:/Users/dev/it''s'"), "{script}");
        assert!(script.contains("Out-Line 'mewrk-capabilities 2'"), "{script}");
        let skills = script.find("$S = Pick 'skills'").expect("skills");
        let agents = script.find("$A = Pick 'agents'").expect("agents");
        assert!(skills < agents);
        let section = &script[agents..];
        assert!(section.contains("Out-Line 'A'"), "{section}");
        assert!(section.contains("$f.Length -le 3000"), "{section}");
        assert!(section.contains("$f.Attributes -band [System.IO.FileAttributes]::ReparsePoint"), "{section}");
        assert!(section.contains("EndsWith('.json', [System.StringComparison]::OrdinalIgnoreCase)"), "{section}");
        assert!(section.contains("GetFiles()"), "only direct children: {section}");
        assert!(!section.contains("AllDirectories"), "{section}");
        assert_eq!(script.matches("Out-Line 'E'").count(), 2, "{script}");
        assert!(script.trim_end().ends_with("Quit 0"), "{script}");
    }

    #[test]
    fn the_file_removal_names_one_file_and_never_recurses() {
        let script = remove_file("C:/Users/dev/it's/.mewrk/agents/a b.json");
        assert!(script.contains("$f = Native-Path 'C:/Users/dev/it''s/.mewrk/agents/a b.json'"), "{script}");
        assert!(script.contains("{ Quit 66 }"), "{script}");
        assert!(script.contains("Remove-Item -LiteralPath $f -Force\n"), "{script}");
        assert!(!script.contains("-Recurse"), "{script}");
        assert!(script.trim_end().ends_with("Quit 0"), "{script}");
    }

    fn target(confine: bool) -> Target<'static> {
        Target {
            root: "C:/Users/dev/app",
            confine,
            also: &[],
        }
    }

    /// A confined script admits the files the conversation's instruction files
    /// import on the machine, each a single-quoted literal compared exactly.
    #[test]
    fn a_confined_script_admits_each_imported_file() {
        let also = ["C:/Users/dev/notes/it's.md".to_owned()];
        let target = Target {
            root: "C:/Users/dev/app",
            confine: true,
            also: &also,
        };
        let script = prologue(&target, "../notes/it's.md", Mode::Existing);
        assert!(
            script.contains("if (-not ((Inside $C $ROOT) -or (Inside $C 'C:/Users/dev/notes/it''s.md'))) { Out-Err $C; Quit 65 }"),
            "{script}"
        );
    }

    #[test]
    fn operands_reach_the_script_as_single_quoted_literals() {
        let script = listing(&target(true), "it's $(here)", 2, 11);
        assert!(script.contains("$REQ = 'it''s $(here)'"), "{script}");
        assert!(script.contains("Native-Path 'C:/Users/dev/app'"), "{script}");
        assert!(script.contains("Inside $C $ROOT"), "confinement is compiled in");
        assert!(!listing(&target(false), ".", 2, 11).contains("Inside $C $ROOT"));
        // PowerShell's typographic quotes delimit strings too.
        let script = grep(&target(true), ".", "a\u{2019}b", true, 5);
        assert!(script.contains("'a\u{2019}\u{2019}b'"), "{script}");
    }

    #[test]
    fn every_reserved_exit_code_is_where_the_posix_leg_has_it() {
        let read = read(&target(true), "a.txt", 100);
        for code in ["Quit 64", "Quit 65", "Quit 66", "Quit 67", "Quit 68"] {
            assert!(read.contains(code), "{code}");
        }
        let write = cas_write(&target(true), "a.txt", "absent");
        for code in ["Quit 69", "Quit 70"] {
            assert!(write.contains(code), "{code}");
        }
        assert!(grep(&target(true), ".", "(", false, 5).contains("Quit 2"));
        assert!(lsp_launch("C:/w", "gopls", &[], &[]).contains("exit 127"));
    }

    #[test]
    fn the_launch_writes_nothing_before_the_server_owns_standard_output() {
        let script = lsp_launch(
            "C:/w",
            "typescript-language-server",
            &["--stdio".to_owned()],
            &[("NODE_OPTIONS".to_owned(), "--max-old-space-size=4096".to_owned())],
        );
        assert!(!script.contains("OpenStandardOutput"));
        assert!(!script.contains("Out-Line"));
        assert!(script.contains("& $server.Source '--stdio'\nexit $LASTEXITCODE"), "{script}");
        assert!(script.contains("SetEnvironmentVariable('NODE_OPTIONS', '--max-old-space-size=4096')"));
    }

    /// PowerShell names ignore case, so nothing after the prologue may assign
    /// or loop over `$C`, `$T`, `$ROOT` or `$REQ` in any spelling — the
    /// language-server probe once looped over its presets as `$c` and read
    /// the last preset's name as the file.
    #[test]
    fn no_script_reuses_the_prologues_variables_in_another_case() {
        let paths = [".mewrk/lsp.json".to_owned(), ".naiword/lsp.json".to_owned()];
        let scripts = [
            listing(&target(true), ".", 2, 11),
            find(&target(true), ".", Some("*.rs"), 10),
            grep(&target(true), ".", "x", false, 10),
            read(&target(true), "a", 10),
            probe(&target(true), "a", 10),
            cas_write(&target(true), "a", "absent"),
            lsp_probe(&target(true), "a", 10, 10, &paths, &["gopls", "clangd"]),
        ];
        let loops = regex::Regex::new(r"(?i)foreach\s*\(\s*\$(c|t|root|req)\s+in").unwrap();
        let assigned = regex::Regex::new(r"(?im)^\s*\$(c|t|root|req)\s*=").unwrap();
        for script in &scripts {
            assert!(!loops.is_match(script), "{script}");
            let body = &script[script.find("Out-Line $C\n").expect("prologue")..];
            assert!(!assigned.is_match(body), "{body}");
        }
    }

    #[test]
    fn the_name_prefilter_escapes_powershells_own_escape() {
        let script = find(&target(true), ".", Some("a`b*.rs"), 10);
        assert!(script.contains("-like 'a``b*.rs'"), "{script}");
        let script = find(&target(true), ".", None, 10);
        assert!(script.contains("if ($true)"), "{script}");
    }
}
