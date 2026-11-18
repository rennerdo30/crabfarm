; crabfarm per-user Windows setup (NSIS 3). Built by scripts/package.ps1:
;   makensis /DVERSION=1.0.0 /DVI_VERSION=1.0.0.0 /DSRC=<dir> /DOUTFILE=<file> installer\crabfarm.nsi
; SRC holds crabfarm.exe, crabfarmd.exe, cargo-crabfarm.exe, crabfarm.ico, LICENSE.txt and
; GETTING-STARTED.md.
;
; Installs to %LOCALAPPDATA%\Programs\crabfarm without admin rights, with an
; Installed apps entry. Two optional components do their work by running
; crabfarm itself (`path add`, `service install`), so the installer and the
; command line change the machine in exactly the same way:
;   - "Add crabfarm to PATH"                         (default on)
;   - "Start the crabfarm node when I sign in"       (default on)
; Silent switches (CI, scripts): /S  /NOPATH  /NOAUTOSTART
; Uninstall: /S  /DELETEDATA (also the farm identity and cache; otherwise
; they stay, so a reinstall rejoins the same farm).
;
; Files in use (CF-27): a running node is asked to stop and the result is
; CHECKED (crabfarm node --stop waits until node.lock is free, 20 s). An exe
; that cargo or rust-analyzer still runs is renamed aside (a running exe can
; be renamed, not overwritten) and the leftover is deleted on the next run or
; at reboot. If a file cannot be replaced or removed the setup stops with a
; message and a non-zero exit code (silent mode: 3 install, 4 uninstall); the
; uninstall entry stays until the files are gone.

!macro REQUIRE name
  !ifndef ${name}
    !error "Pass /D${name}=..."
  !endif
!macroend
!insertmacro REQUIRE VERSION
!insertmacro REQUIRE VI_VERSION
!insertmacro REQUIRE SRC
!insertmacro REQUIRE OUTFILE

Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include MUI2.nsh
!include LogicLib.nsh
!include FileFunc.nsh
!include Sections.nsh
!include nsDialogs.nsh

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\crabfarm"

Name "crabfarm"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\crabfarm"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
BrandingText "crabfarm ${VERSION}"

VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "crabfarm"
VIAddVersionKey "CompanyName" "renner.dev"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "crabfarm ${VERSION} setup"
VIAddVersionKey "LegalCopyright" "MIT License"

!define MUI_ICON "..\assets\crabfarm.ico"
!define MUI_UNICON "..\assets\crabfarm.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_TITLE "crabfarm is installed"
!define MUI_FINISHPAGE_TEXT "Open a new terminal, then follow the guide: create a farm on one machine, join the others, and run `crabfarm enable` in a Rust project.$\r$\n$\r$\nThe first time the node runs, Windows Firewall asks once: allow it on private networks."
!define MUI_FINISHPAGE_LINK "Read the getting-started guide"
!define MUI_FINISHPAGE_LINK_LOCATION "https://crabfarm.renner.dev/guide.html"

!insertmacro MUI_PAGE_LICENSE "${SRC}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
UninstPage custom un.DataPageCreate un.DataPageLeave
!insertmacro MUI_UNPAGE_INSTFILES
!define MUI_FINISHPAGE_TITLE "crabfarm is removed"
!define MUI_FINISHPAGE_TEXT "Projects where you ran `crabfarm enable` keep a rustc-wrapper entry in their .cargo\config.toml, and cargo fails there until it is removed. Remove it with `crabfarm disable` in each such project (install crabfarm again first if it is already gone), or delete that entry by hand.$\r$\n$\r$\nA global entry (`crabfarm enable --global`) pointing at this install was removed by the uninstaller."
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"

Var DeleteData
Var DeleteDataBox
Var Remaining

; ------------------------------------------------------------------ helpers

; Ask a running node (if any) to shut down, using the installed crabfarm.exe:
; no process is killed by name. `node --stop` returns only once the node has
; released node.lock, or fails after 20 s, so a non-zero result means the
; node is still running: the errors flag is set and the caller stops.
!macro STOP_NODE un
Function ${un}StopNode
  ClearErrors
  ${If} ${FileExists} "$INSTDIR\crabfarm.exe"
    nsExec::ExecToLog /TIMEOUT=40000 '"$INSTDIR\crabfarm.exe" node --stop'
    Pop $0
    DetailPrint "crabfarm node --stop: $0"
    ${If} $0 != 0
      SetErrors
    ${EndIf}
  ${EndIf}
FunctionEnd
!macroend
!insertmacro STOP_NODE ""
!insertmacro STOP_NODE "un."

; Make $R0 (a full path) free: delete it, or, when a process still runs it,
; rename it aside as <name>.old<N> (a running exe can be renamed) and delete
; that at the next opportunity (now, else at reboot). Sets the errors flag
; when $R0 is still in the way afterwards.
!macro MOVE_ASIDE un
Function ${un}MoveAside
  ClearErrors
  ${IfNot} ${FileExists} "$R0"
    Return
  ${EndIf}
  Delete "$R0"
  ${IfNot} ${FileExists} "$R0"
    Return
  ${EndIf}
  StrCpy $R1 0
  ${Do}
    ${If} $R1 == 0
      StrCpy $R2 "$R0.old"
    ${Else}
      StrCpy $R2 "$R0.old$R1"
    ${EndIf}
    ${IfNot} ${FileExists} "$R2"
      ClearErrors
      Rename "$R0" "$R2"
      ${IfNot} ${Errors}
        DetailPrint "$R0 is in use; moved aside to $R2"
        Delete /REBOOTOK "$R2"
        Return
      ${EndIf}
    ${EndIf}
    IntOp $R1 $R1 + 1
  ${LoopUntil} $R1 > 9
  SetErrors
FunctionEnd
!macroend
!insertmacro MOVE_ASIDE ""
!insertmacro MOVE_ASIDE "un."

; Stop the setup with a clear message and a non-zero exit code.
!macro FAIL code msg
  SetErrorLevel ${code}
  DetailPrint "${msg}"
  MessageBox MB_OK|MB_ICONSTOP "${msg}" /SD IDOK
  Abort "${msg}"
!macroend

; ------------------------------------------------------------------ sections

Section "crabfarm (required)" SecCore
  SectionIn RO
  Call StopNode
  ${If} ${Errors}
    !insertmacro FAIL 3 "The running crabfarm node did not stop, so crabfarm cannot be updated. Run `crabfarm node --stop` (or end crabfarmd.exe in Task Manager), then run setup again. Nothing was changed."
  ${EndIf}
  SetOutPath "$INSTDIR"
  ; Leftovers of an earlier update that were still in use then.
  Delete "$INSTDIR\crabfarm.exe.old*"
  Delete "$INSTDIR\crabfarmd.exe.old*"
  Delete "$INSTDIR\cargo-crabfarm.exe.old*"
  ; cargo or rust-analyzer may be running crabfarm.exe right now: move the old
  ; file out of the way instead of failing to overwrite it.
  StrCpy $R0 "$INSTDIR\crabfarm.exe"
  Call MoveAside
  ${If} ${Errors}
    !insertmacro FAIL 3 "crabfarm.exe is in use and cannot be replaced. Close cargo, rust-analyzer and editors that build Rust, then run setup again. Nothing was changed."
  ${EndIf}
  StrCpy $R0 "$INSTDIR\crabfarmd.exe"
  Call MoveAside
  ${If} ${Errors}
    !insertmacro FAIL 3 "crabfarmd.exe is in use and cannot be replaced. End it in Task Manager, then run setup again."
  ${EndIf}
  StrCpy $R0 "$INSTDIR\cargo-crabfarm.exe"
  Call MoveAside
  ${If} ${Errors}
    !insertmacro FAIL 3 "cargo-crabfarm.exe is in use and cannot be replaced. Close cargo, then run setup again."
  ${EndIf}
  File "${SRC}\crabfarm.exe"
  File "${SRC}\crabfarmd.exe"
  File "${SRC}\cargo-crabfarm.exe"
  File "${SRC}\crabfarm.ico"
  File "${SRC}\LICENSE.txt"
  File "${SRC}\GETTING-STARTED.md"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "crabfarm"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "renner.dev"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://crabfarm.renner.dev"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\crabfarm.ico"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "Add crabfarm to PATH" SecPath
  nsExec::ExecToLog /TIMEOUT=30000 '"$INSTDIR\crabfarm.exe" path add'
  Pop $0
  ${If} $0 != 0
    DetailPrint "crabfarm path add failed ($0)"
    MessageBox MB_OK|MB_ICONEXCLAMATION "crabfarm could not be added to PATH (error $0). Run `crabfarm path add` later, or call crabfarm by its full path." /SD IDOK
  ${EndIf}
SectionEnd

Section "Start the crabfarm node when I sign in" SecAutostart
  nsExec::ExecToLog /TIMEOUT=30000 '"$INSTDIR\crabfarm.exe" service install'
  Pop $0
  ${If} $0 != 0
    DetailPrint "crabfarm service install failed ($0)"
    MessageBox MB_OK|MB_ICONEXCLAMATION "Autostart could not be turned on (error $0). Run `crabfarm service install` later." /SD IDOK
  ${EndIf}
SectionEnd

Function .onInit
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/NOPATH" $1
  ${IfNot} ${Errors}
    !insertmacro UnselectSection ${SecPath}
  ${EndIf}
  ClearErrors
  ${GetOptions} $0 "/NOAUTOSTART" $1
  ${IfNot} ${Errors}
    !insertmacro UnselectSection ${SecAutostart}
  ${EndIf}
FunctionEnd

LangString DESC_Core ${LANG_ENGLISH} "crabfarm, cargo-crabfarm (for `cargo crabfarm build`) and crabfarmd (the node, which runs without a window)."
LangString DESC_Path ${LANG_ENGLISH} "Lets you type `crabfarm` in any new terminal."
LangString DESC_Autostart ${LANG_ENGLISH} "Starts the node when you sign in and now. You can turn this off later with `crabfarm service uninstall`."
!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore} $(DESC_Core)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecPath} $(DESC_Path)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAutostart} $(DESC_Autostart)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ----------------------------------------------------------------- uninstall

Function un.onInit
  StrCpy $DeleteData 0
  ${un.GetParameters} $0
  ClearErrors
  ${un.GetOptions} $0 "/DELETEDATA" $1
  ${IfNot} ${Errors}
    StrCpy $DeleteData 1
  ${EndIf}
FunctionEnd

Function un.DataPageCreate
  ; Silent uninstalls decide with /DELETEDATA instead.
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 60u "crabfarm will be removed. Before you continue: run `crabfarm disable` in every project where you ran `crabfarm enable`, or those projects keep a rustc-wrapper entry that no longer works. Your farm identity (the keys that make this machine a member of your farm) and the cache stay on this machine unless you tick the box below; keeping them lets a reinstall rejoin the same farm."
  Pop $0
  ${NSD_CreateCheckbox} 0 70u 100% 12u "Also delete my farm identity and cache"
  Pop $DeleteDataBox
  ${If} $DeleteData == 1
    ${NSD_Check} $DeleteDataBox
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function un.DataPageLeave
  ${NSD_GetState} $DeleteDataBox $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $DeleteData 1
  ${Else}
    StrCpy $DeleteData 0
  ${EndIf}
FunctionEnd

Section "Uninstall"
  Call un.StopNode
  ${If} ${Errors}
    !insertmacro FAIL 4 "The running crabfarm node did not stop, so crabfarm cannot be removed. Run `crabfarm node --stop` (or end crabfarmd.exe in Task Manager), then run the uninstaller again. Nothing was removed."
  ${EndIf}
  ; A global `crabfarm enable --global` entry that points at THIS install
  ; would leave every cargo build failing once the exe is gone.
  ${If} ${FileExists} "$INSTDIR\crabfarm.exe"
    nsExec::ExecToLog /TIMEOUT=30000 '"$INSTDIR\crabfarm.exe" disable --global --this-install-only'
    Pop $0
    DetailPrint "crabfarm disable --global: $0"
    nsExec::ExecToLog /TIMEOUT=30000 '"$INSTDIR\crabfarm.exe" service uninstall'
    Pop $0
    DetailPrint "crabfarm service uninstall: $0"
    nsExec::ExecToLog /TIMEOUT=30000 '"$INSTDIR\crabfarm.exe" path remove'
    Pop $0
    DetailPrint "crabfarm path remove: $0"
  ${EndIf}
  StrCpy $Remaining ""
  ; Leftovers of updates that were in use at the time.
  Delete "$INSTDIR\crabfarm.exe.old*"
  Delete "$INSTDIR\crabfarmd.exe.old*"
  Delete "$INSTDIR\cargo-crabfarm.exe.old*"
  Delete "$INSTDIR\crabfarm.ico"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\GETTING-STARTED.md"
  StrCpy $R0 "$INSTDIR\crabfarmd.exe"
  Call un.MoveAside
  ${If} ${Errors}
    StrCpy $Remaining "$Remaining crabfarmd.exe"
  ${EndIf}
  StrCpy $R0 "$INSTDIR\cargo-crabfarm.exe"
  Call un.MoveAside
  ${If} ${Errors}
    StrCpy $Remaining "$Remaining cargo-crabfarm.exe"
  ${EndIf}
  StrCpy $R0 "$INSTDIR\crabfarm.exe"
  Call un.MoveAside
  ${If} ${Errors}
    StrCpy $Remaining "$Remaining crabfarm.exe"
  ${EndIf}
  ; Files are still there: keep the uninstaller and its Installed apps entry
  ; so the user can finish the job, and say so (exit code 4 when silent).
  ${If} $Remaining != ""
    !insertmacro FAIL 4 "crabfarm could not remove:$Remaining. Close cargo, rust-analyzer and editors that build Rust, then run the uninstaller again (the Installed apps entry was kept)."
  ${EndIf}
  Delete "$INSTDIR\uninstall.exe"
  RMDir /REBOOTOK "$INSTDIR"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  ${If} $DeleteData == 1
    ; The folder `directories` uses for the node's data: keys, peers, cache.
    RMDir /r "$APPDATA\renner\crabfarm"
    RMDir "$APPDATA\renner"
  ${EndIf}
SectionEnd
