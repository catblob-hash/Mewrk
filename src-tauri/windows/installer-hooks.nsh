; NSIS installer hooks (bundle.windows.nsis.installerHooks in tauri.windows.conf.json).
;
; Mewrk installers up to 1.2.3 shipped the AI SDK sidecar and the Claude Code
; CLI as Tauri external binaries, so they sit beside mewrk.exe in the install
; folder (~340 MB together). Newer versions no longer carry them: the sidecar is
; fetched into the app's data folder at start-up, and the Claude Agent SDK and
; CLI are installed from the Claude Agent provider page. An NSIS upgrade only
; overwrites the files the new version has and never removes the ones it no
; longer has, so the old copies would stay behind, unused and unmaintained.
;
; Both hooks ignore failure: a copy that is still running cannot be deleted, and
; whether it is gone changes nothing about the install itself.

!macro NSIS_HOOK_POSTINSTALL
  ; Upgrade from an installer that still shipped them: remove the orphans.
  Delete "$INSTDIR\mewrk-aisdk.exe"
  Delete "$INSTDIR\claude.exe"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Uninstalling an installation that was upgraded from such a version: the
  ; uninstaller only knows the files of its own version, so remove the orphans
  ; here too, and the install folder with them when nothing else is left
  ; (Tauri's own RMDir ran before this hook, while they were still there).
  Delete "$INSTDIR\mewrk-aisdk.exe"
  Delete "$INSTDIR\claude.exe"
  RMDir "$INSTDIR"
!macroend
