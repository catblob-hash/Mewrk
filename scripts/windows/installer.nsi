# Mewrk for Windows: the x64 installer. Built by scripts/package-windows.sh; by hand it is
#   makensis /DVERSION=1.0.0 /DSTAGE=C:\...\stage /DOUTFILE=C:\...\Mewrk-1.0.0-x64-setup.exe scripts\windows\installer.nsi
# STAGE holds what gets installed, recursively: mewrk.exe, remote-agents\..., LICENSE and the notices.
#
# Per machine, admin required, x64 Windows 10 or later only. The installer and the uninstaller are in English and Simplified Chinese,
# and NSIS picks the language from the system UI. /S runs without any prompt; then it never touches a previous version and
# fails with a nonzero exit code where it would otherwise have to ask.
#
# The uninstaller removes the program, the shortcuts and the Add/Remove Programs entry. It never touches %APPDATA%, where the data lives.

Unicode true
SetCompressor /SOLID lzma
ManifestDPIAware true
RequestExecutionLevel admin
ShowInstDetails show

!ifndef VERSION
  !error "VERSION is not set: makensis /DVERSION=1.0.0 ..."
!endif
!ifndef STAGE
  !error "STAGE is not set: makensis /DSTAGE=<directory with the files to install> ..."
!endif
!ifndef OUTFILE
  !error "OUTFILE is not set: makensis /DOUTFILE=<installer path> ..."
!endif
!if /FileExists "${STAGE}\mewrk.exe"
!else
  !error "${STAGE}\mewrk.exe does not exist"
!endif

!define APP_NAME "Mewrk"
!define APP_ID "dev.mewrk.native"
!define APP_EXE "mewrk.exe"
!define APP_PUBLISHER "Mewrk"
!define APP_URL "https://github.com/catblob-hash/Mewrk"
!define APP_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"
# The discontinued 1.0.x (Tauri-built) installer registered itself under this name.
!define OLD_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Mewrk"
!define REMOTE_AGENT "remote-agents\x86_64-pc-windows-msvc\mewrk-remote.exe"
# WebView2 Runtime: the client key holds its version in "pv" when the runtime is installed.
!define WV2_MACHINE_KEY "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WV2_USER_KEY "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WV2_URL "https://developer.microsoft.com/microsoft-edge/webview2/"
# Relative to this script, as NSIS resolves icon paths.
!define ICON "..\..\crates\mewrk\assets\icon.ico"
!define VI_VERSION "${VERSION}.0"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
BrandingText "${APP_NAME} ${VERSION}"

!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "FileFunc.nsh"
!insertmacro GetParent
!insertmacro GetSize

Var OldUninstaller
Var OldDir
Var ShortcutName

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchMewrk

!include "MUI2.nsh"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

# The first language is the fallback. NSIS takes the one matching the system UI when there is one.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"

LangString MSG_NEEDS_X64 ${LANG_ENGLISH} "Mewrk needs 64-bit Windows 10 or Windows 11. Setup will stop."
LangString MSG_NEEDS_X64 ${LANG_SIMPCHINESE} "Mewrk 需要 64 位的 Windows 10 或 Windows 11。安装程序将退出。"

LangString MSG_NEEDS_WIN10 ${LANG_ENGLISH} "Mewrk needs Windows 10 or Windows 11. Setup will stop."
LangString MSG_NEEDS_WIN10 ${LANG_SIMPCHINESE} "Mewrk 需要 Windows 10 或 Windows 11。安装程序将退出。"

LangString MSG_APP_RUNNING ${LANG_ENGLISH} "Mewrk is still running. Close it, then choose Retry. Cancel stops the installation."
LangString MSG_APP_RUNNING ${LANG_SIMPCHINESE} "Mewrk 仍在运行。请先关闭它，然后选择“重试”。选择“取消”将停止安装。"

LangString MSG_APP_RUNNING_UN ${LANG_ENGLISH} "Mewrk is still running. Close it, then choose Retry. Cancel stops the uninstall."
LangString MSG_APP_RUNNING_UN ${LANG_SIMPCHINESE} "Mewrk 仍在运行。请先关闭它，然后选择“重试”。选择“取消”将停止卸载。"

LangString MSG_OLD_FOUND ${LANG_ENGLISH} "Mewrk 1.0.x is installed on this PC. Uninstall it before installing this version?$\r$\n$\r$\nYour conversations and settings are kept. This version keeps its data outside the program folder, and removing the old program leaves that data alone.$\r$\n$\r$\nChoose No to install this version next to the old one, in Mewrk Native."
LangString MSG_OLD_FOUND ${LANG_SIMPCHINESE} "此电脑上安装了 Mewrk 1.0.x。是否先卸载它再安装本版本？$\r$\n$\r$\n你的对话和设置会保留。本版本把数据存放在程序文件夹之外，卸载旧程序不会动这些数据。$\r$\n$\r$\n选择“否”则把本版本安装到 Mewrk Native 文件夹，与旧版本并存。"

LangString MSG_OLD_FAILED ${LANG_ENGLISH} "The previous version could not be uninstalled. Setup has stopped. Uninstall Mewrk 1.0.x from Apps & features, then run this setup again."
LangString MSG_OLD_FAILED ${LANG_SIMPCHINESE} "无法卸载之前的版本。安装程序已停止。请在“应用和功能”中卸载 Mewrk 1.0.x，然后重新运行本安装程序。"

LangString MSG_WEBVIEW2 ${LANG_ENGLISH} "The preview pane needs the Microsoft Edge WebView2 Runtime, which is not installed on this PC. Mewrk still installs; the preview pane stays empty until the runtime is installed.$\r$\n$\r$\nOpen the WebView2 download page now?"
LangString MSG_WEBVIEW2 ${LANG_SIMPCHINESE} "预览窗格需要 Microsoft Edge WebView2 运行时，本电脑尚未安装。Mewrk 仍会正常安装，但在安装该运行时之前预览窗格将为空。$\r$\n$\r$\n现在打开 WebView2 下载页面吗？"


LangString SEC_MAIN ${LANG_ENGLISH} "Mewrk"
LangString SEC_MAIN ${LANG_SIMPCHINESE} "Mewrk"
LangString SEC_DESKTOP ${LANG_ENGLISH} "Desktop shortcut"
LangString SEC_DESKTOP ${LANG_SIMPCHINESE} "桌面快捷方式"
LangString DESC_MAIN ${LANG_ENGLISH} "The program, its Start Menu shortcut and the uninstaller."
LangString DESC_MAIN ${LANG_SIMPCHINESE} "程序本体、开始菜单快捷方式和卸载程序。"
LangString DESC_DESKTOP ${LANG_ENGLISH} "A shortcut to Mewrk on the desktop of every user."
LangString DESC_DESKTOP ${LANG_SIMPCHINESE} "在所有用户的桌面上创建 Mewrk 快捷方式。"

# Refuses to start: a message box when someone can answer it, and always a nonzero exit code.
!macro Refuse MSG
  IfSilent +2 +1
  MessageBox MB_OK|MB_ICONSTOP "$(${MSG})"
  SetErrorLevel 1
  Abort
!macroend

# Defines ExeLocked and CloseApp, once for the installer and once for the uninstaller (PREFIX "" or "un.", TAG a unique label prefix).
# Input of CloseApp: $R9, the install directory. It returns when no Mewrk program runs from that directory. Otherwise it asks for Retry or
# Cancel, and Cancel stops the setup. Silent runs stop at once with exit code 3, because nobody can answer.
!macro DefineAppLock PREFIX TAG MSG
Function ${PREFIX}ExeLocked
  # In: $0 is a path. Out: $0 is 1 when the file exists and another process holds it open, else 0.
  # Writing to a running program's executable is refused by Windows, so a failed open for append means it runs.
  Push $1
  Push $2
  StrCpy $1 0
  IfFileExists "$0" ${TAG}Probe ${TAG}Done
  ${TAG}Probe:
  ClearErrors
  FileOpen $2 "$0" a
  IfErrors ${TAG}Locked
  FileClose $2
  Goto ${TAG}Done
  ${TAG}Locked:
  StrCpy $1 1
  ${TAG}Done:
  StrCpy $0 $1
  Pop $2
  Pop $1
FunctionEnd

Function ${PREFIX}CloseApp
  Push $0
  ${TAG}Check:
  StrCpy $0 "$R9\${APP_EXE}"
  Call ${PREFIX}ExeLocked
  ${If} $0 == 0
    StrCpy $0 "$R9\${REMOTE_AGENT}"
    Call ${PREFIX}ExeLocked
  ${EndIf}
  ${If} $0 == 0
    Pop $0
    Return
  ${EndIf}
  IfSilent ${TAG}Silent ${TAG}Ask
  ${TAG}Ask:
  MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION|MB_DEFBUTTON1 "$(${MSG})" IDRETRY ${TAG}Check
  Abort
  ${TAG}Silent:
  SetErrorLevel 3
  Abort
FunctionEnd
!macroend

!insertmacro DefineAppLock "" Inst MSG_APP_RUNNING
!insertmacro DefineAppLock "un." Un MSG_APP_RUNNING_UN

Function .onInit
  SetRegView 64
  SetShellVarContext all
  StrCpy $ShortcutName "${APP_NAME}"
  StrCpy $OldUninstaller ""
  StrCpy $OldDir ""

  ${IfNot} ${RunningX64}
    !insertmacro Refuse MSG_NEEDS_X64
  ${EndIf}
  ${IfNot} ${AtLeastWin10}
    !insertmacro Refuse MSG_NEEDS_WIN10
  ${EndIf}

  # A discontinued 1.0.x install: HKLM first, then the per-user key. A stale key without its uninstaller is ignored.
  ReadRegStr $0 HKLM "${OLD_KEY}" "UninstallString"
  ${If} $0 == ""
    ReadRegStr $0 HKCU "${OLD_KEY}" "UninstallString"
  ${EndIf}
  ${If} $0 != ""
    # The string is "\"path\\uninstall.exe\"", possibly without the quotes. Drop them.
    StrCpy $1 $0 1
    ${If} $1 == '"'
      StrCpy $0 $0 "" 1
    ${EndIf}
    StrLen $2 $0
    ${If} $2 > 0
      IntOp $2 $2 - 1
      StrCpy $1 $0 1 $2
      ${If} $1 == '"'
        StrCpy $0 $0 $2
      ${EndIf}
    ${EndIf}
    IfFileExists "$0" +2 +1
    StrCpy $0 ""
    ${If} $0 != ""
      StrCpy $OldUninstaller $0
      ${GetParent} "$0" $OldDir
    ${EndIf}
  ${EndIf}

  ${If} $OldUninstaller != ""
    IfSilent old_keep_both
    MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON1 "$(MSG_OLD_FOUND)" IDYES old_remove
    Goto old_keep_both
    old_remove:
    StrCpy $R9 $OldDir
    Call CloseApp
    ExecWait '"$OldUninstaller" /S _?=$OldDir' $0
    ${If} $0 != 0
      SetErrorLevel 1
      MessageBox MB_OK|MB_ICONSTOP "$(MSG_OLD_FAILED)"
      Abort
    ${EndIf}
    # The old folder is free again.
    Goto old_done
    old_keep_both:
    # Beside it, so that neither overwrites the other, shortcuts included. A folder given with /D is already apart from it.
    StrCpy $ShortcutName "${APP_NAME} Native"
    ${If} $INSTDIR == $OldDir
      StrCpy $INSTDIR "$PROGRAMFILES64\${APP_NAME} Native"
    ${EndIf}
    old_done:
  ${EndIf}
FunctionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
  # Empty without the entry the install wrote, and then no shortcut is known to be this install's: "Mewrk.lnk" could be Mewrk 1.0's.
  ReadRegStr $ShortcutName HKLM "${APP_KEY}" "ShortcutName"

  StrCpy $R9 $INSTDIR
  Call un.CloseApp
FunctionEnd

Function .onInstSuccess
  # The preview pane needs WebView2. Setup goes on either way; a silent run only skips the question.
  IfSilent wv2_done
  ReadRegStr $0 HKLM "${WV2_MACHINE_KEY}" "pv"
  ${If} $0 == ""
    ReadRegStr $0 HKCU "${WV2_USER_KEY}" "pv"
  ${EndIf}
  ${If} $0 == ""
    MessageBox MB_YESNO|MB_ICONINFORMATION "$(MSG_WEBVIEW2)" IDNO wv2_done
    ExecShell "open" "${WV2_URL}"
  ${EndIf}
  wv2_done:
FunctionEnd

Function LaunchMewrk
  # Started by explorer.exe, so the program runs as the desktop user and not with the installer's administrator token.
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\${APP_EXE}"'
FunctionEnd

Section "$(SEC_MAIN)" SecMain
  SectionIn RO
  StrCpy $R9 $INSTDIR
  Call CloseApp

  ClearErrors
  SetOutPath "$INSTDIR"
  File /r "${STAGE}\*.*"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  # A folder that can't be written, for one, must not look like a finished install to whoever ran this with /S.
  ${If} ${Errors}
    SetErrorLevel 4
    Abort
  ${EndIf}

  CreateShortcut "$SMPROGRAMS\$ShortcutName.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0

  WriteRegStr HKLM "${APP_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${APP_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${APP_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKLM "${APP_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE},0"
  WriteRegStr HKLM "${APP_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${APP_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${APP_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKLM "${APP_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKLM "${APP_KEY}" "ShortcutName" "$ShortcutName"
  WriteRegDWORD HKLM "${APP_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${APP_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKLM "${APP_KEY}" "EstimatedSize" $0
SectionEnd

Section "$(SEC_DESKTOP)" SecDesktop
  CreateShortcut "$DESKTOP\$ShortcutName.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} $(DESC_MAIN)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DESC_DESKTOP)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  StrCpy $R9 $INSTDIR
  Call un.CloseApp

  ${If} $ShortcutName != ""
    Delete "$SMPROGRAMS\$ShortcutName.lnk"
    Delete "$DESKTOP\$ShortcutName.lnk"
  ${EndIf}

  # What this setup installed and nothing else, so that a folder given with /D keeps whatever else is in it. The data under
  # %APPDATA% is not here at all.
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${REMOTE_AGENT}"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.md"
  Delete "$INSTDIR\THIRD-PARTY-LICENSES.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR\remote-agents\x86_64-pc-windows-msvc"
  RMDir "$INSTDIR\remote-agents"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "${APP_KEY}"
SectionEnd

VIProductVersion "${VI_VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileDescription" "${APP_NAME} installer"
VIAddVersionKey /LANG=${LANG_ENGLISH} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_ENGLISH} "LegalCopyright" "GPL-3.0-or-later"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_SIMPCHINESE} "LegalCopyright" "GPL-3.0-or-later"
