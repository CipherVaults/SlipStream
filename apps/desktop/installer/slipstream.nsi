; SlipStream Per-User Windows Installer (NSIS 3 Unicode)
Unicode true

!ifndef VERSION
  !error "VERSION must be defined (/DVERSION=x.y.z)"
!endif
!ifndef STAGE_DIR
  !error "STAGE_DIR must be defined (/DSTAGE_DIR=path)"
!endif
!ifndef OUTFILE
  !error "OUTFILE must be defined (/DOUTFILE=path)"
!endif

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "StrFunc.nsh"
${StrStr}

Name "SlipStream ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\SlipStream"
InstallDirRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "InstallLocation"
RequestExecutionLevel user
ShowInstDetails nevershow
ShowUnInstDetails nevershow
SetCompressor /SOLID lzma

!define MUI_ICON "..\icons\icon.ico"
!define MUI_UNICON "..\icons\icon.ico"
!define MUI_ABORTWARNING

; Modern UI pages
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\SlipStream.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch SlipStream"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Var IsUpdateMode
Var ShouldRelaunch
Var WaitTries

Function .onInit
  StrCpy $IsUpdateMode "0"
  StrCpy $ShouldRelaunch "0"
  ${GetParameters} $R0

  ${StrStr} $R1 $R0 "/UPDATE"
  ${If} $R1 != ""
    StrCpy $IsUpdateMode "1"
  ${EndIf}

  ${StrStr} $R2 $R0 "/P"
  ${If} $R2 != ""
    SetSilent silent
  ${EndIf}

  ${StrStr} $R3 $R0 "/R"
  ${If} $R3 != ""
    StrCpy $ShouldRelaunch "1"
  ${EndIf}

  ${If} $INSTDIR == ""
    StrCpy $INSTDIR "$LOCALAPPDATA\SlipStream"
  ${EndIf}
FunctionEnd

Function EnsureExeUnlocked
  Pop $R9 ; path to check
  StrCpy $WaitTries 0
  loop_wait:
    ${If} ${FileExists} "$R9"
      ClearErrors
      FileOpen $0 "$R9" a
      ${If} ${Errors}
        IntOp $WaitTries $WaitTries + 1
        ${If} $WaitTries == 8
          ; Graceful stop first via PID/path-scoped PowerShell query (never /T)
          nsExec::ExecToStack 'powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -Command "Get-Process | Where-Object { try { $_.Path -eq ''$R9'' } catch { $false } } | Stop-Process -ErrorAction SilentlyContinue"'
          Pop $1
          Pop $2
        ${ElseIf} $WaitTries == 14
          nsExec::ExecToStack 'powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -Command "Get-Process | Where-Object { try { $_.Path -eq ''$R9'' } catch { $false } } | Stop-Process -Force -ErrorAction SilentlyContinue"'
          Pop $1
          Pop $2
        ${ElseIf} $WaitTries >= 22
          Abort "Close SlipStream before installing or updating ($R9 is still in use)."
        ${EndIf}
        Sleep 350
        Goto loop_wait
      ${Else}
        FileClose $0
      ${EndIf}
    ${EndIf}
FunctionEnd

Section "SlipStream" SecMain
  SectionIn RO

  ; Wait cleanly for any existing installed SlipStream / native-host processes to unlock binaries
  Push "$INSTDIR\SlipStream.exe"
  Call EnsureExeUnlocked
  Push "$INSTDIR\slipstream-desktop.exe"
  Call EnsureExeUnlocked
  Push "$INSTDIR\bin\slipstream-native-host.exe"
  Call EnsureExeUnlocked

  SetOutPath "$INSTDIR"
  File "${STAGE_DIR}\SlipStream.exe"
  File "${STAGE_DIR}\THIRD_PARTY_NOTICES.md"

  SetOutPath "$INSTDIR\bin"
  File "${STAGE_DIR}\bin\slipstream-native-host.exe"
  File "${STAGE_DIR}\bin\yt-dlp.exe"
  File "${STAGE_DIR}\bin\ffmpeg.exe"
  File "${STAGE_DIR}\bin\ffprobe.exe"
  File "${STAGE_DIR}\bin\yt-dlp-LICENSE.txt"
  File "${STAGE_DIR}\bin\ffmpeg-LICENSE.txt"
  File "${STAGE_DIR}\bin\ffmpeg-README.txt"

  ; Install Chrome and Firefox extensions cleanly without deleting non-owned directories/files
  SetOutPath "$INSTDIR\extensions\chrome"
  File "${STAGE_DIR}\extensions\chrome\manifest.json"
  File "${STAGE_DIR}\extensions\chrome\popup.html"
  File "${STAGE_DIR}\extensions\chrome\early.js"
  File "${STAGE_DIR}\extensions\chrome\background.js"
  File "${STAGE_DIR}\extensions\chrome\content.js"
  SetOutPath "$INSTDIR\extensions\chrome\assets"
  File /r "${STAGE_DIR}\extensions\chrome\assets\*.*"
  SetOutPath "$INSTDIR\extensions\chrome\icons"
  File "${STAGE_DIR}\extensions\chrome\icons\icon16.png"
  File "${STAGE_DIR}\extensions\chrome\icons\icon32.png"
  File "${STAGE_DIR}\extensions\chrome\icons\icon48.png"
  File "${STAGE_DIR}\extensions\chrome\icons\icon128.png"

  SetOutPath "$INSTDIR\extensions\firefox"
  File "${STAGE_DIR}\extensions\firefox\manifest.json"
  File "${STAGE_DIR}\extensions\firefox\popup.html"
  File "${STAGE_DIR}\extensions\firefox\early.js"
  File "${STAGE_DIR}\extensions\firefox\background.js"
  File "${STAGE_DIR}\extensions\firefox\content.js"
  SetOutPath "$INSTDIR\extensions\firefox\assets"
  File /r "${STAGE_DIR}\extensions\firefox\assets\*.*"
  SetOutPath "$INSTDIR\extensions\firefox\icons"
  File "${STAGE_DIR}\extensions\firefox\icons\icon16.png"
  File "${STAGE_DIR}\extensions\firefox\icons\icon32.png"
  File "${STAGE_DIR}\extensions\firefox\icons\icon48.png"
  File "${STAGE_DIR}\extensions\firefox\icons\icon128.png"

  ; Remove legacy v1.x Tauri desktop binary cleanly after replacement
  ${If} ${FileExists} "$INSTDIR\slipstream-desktop.exe"
    Delete "$INSTDIR\slipstream-desktop.exe"
  ${EndIf}

  SetOutPath "$INSTDIR"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Write Add/Remove Programs metadata (per-user HKCU)
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "DisplayName" "SlipStream"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "Publisher" "Mohamed Sayed"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "DisplayIcon" "$INSTDIR\SlipStream.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream" "EstimatedSize" "$0"

  ; Update Start Menu and Desktop shortcuts to point to SlipStream.exe
  CreateDirectory "$SMPROGRAMS\SlipStream"
  CreateShortCut "$SMPROGRAMS\SlipStream\SlipStream.lnk" "$INSTDIR\SlipStream.exe" "" "$INSTDIR\SlipStream.exe" 0
  CreateShortCut "$SMPROGRAMS\SlipStream\Uninstall SlipStream.lnk" "$INSTDIR\Uninstall.exe" "" "$INSTDIR\Uninstall.exe" 0
  ${If} $IsUpdateMode != "1"
    CreateShortCut "$DESKTOP\SlipStream.lnk" "$INSTDIR\SlipStream.exe" "" "$INSTDIR\SlipStream.exe" 0
  ${ElseIf} ${FileExists} "$DESKTOP\SlipStream.lnk"
    CreateShortCut "$DESKTOP\SlipStream.lnk" "$INSTDIR\SlipStream.exe" "" "$INSTDIR\SlipStream.exe" 0
  ${EndIf}

  ; Relaunch whenever /R was passed (covers both v1.3.0 /P and v2.0.0 /S unattended updates)
  ${If} $ShouldRelaunch == "1"
    Exec '"$INSTDIR\SlipStream.exe"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  nsExec::ExecToStack 'powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -Command "Get-Process | Where-Object { try { $_.Path -like ''$INSTDIR\*'' } catch { $false } } | Stop-Process -Force -ErrorAction SilentlyContinue"'
  Pop $0
  Pop $1
  Sleep 450

  Delete "$SMPROGRAMS\SlipStream\SlipStream.lnk"
  Delete "$SMPROGRAMS\SlipStream\Uninstall SlipStream.lnk"
  RMDir "$SMPROGRAMS\SlipStream"
  Delete "$DESKTOP\SlipStream.lnk"

  ; Remove only installer-owned files (preserve user data/downloads inside $INSTDIR)
  Delete "$INSTDIR\bin\slipstream-native-host.exe"
  Delete "$INSTDIR\bin\yt-dlp.exe"
  Delete "$INSTDIR\bin\ffmpeg.exe"
  Delete "$INSTDIR\bin\ffprobe.exe"
  Delete "$INSTDIR\bin\yt-dlp-LICENSE.txt"
  Delete "$INSTDIR\bin\ffmpeg-LICENSE.txt"
  Delete "$INSTDIR\bin\ffmpeg-README.txt"
  RMDir "$INSTDIR\bin"

  RMDir /r "$INSTDIR\extensions\chrome\assets"
  Delete "$INSTDIR\extensions\chrome\icons\icon16.png"
  Delete "$INSTDIR\extensions\chrome\icons\icon32.png"
  Delete "$INSTDIR\extensions\chrome\icons\icon48.png"
  Delete "$INSTDIR\extensions\chrome\icons\icon128.png"
  RMDir "$INSTDIR\extensions\chrome\icons"
  Delete "$INSTDIR\extensions\chrome\manifest.json"
  Delete "$INSTDIR\extensions\chrome\popup.html"
  Delete "$INSTDIR\extensions\chrome\early.js"
  Delete "$INSTDIR\extensions\chrome\background.js"
  Delete "$INSTDIR\extensions\chrome\content.js"
  RMDir "$INSTDIR\extensions\chrome"

  RMDir /r "$INSTDIR\extensions\firefox\assets"
  Delete "$INSTDIR\extensions\firefox\icons\icon16.png"
  Delete "$INSTDIR\extensions\firefox\icons\icon32.png"
  Delete "$INSTDIR\extensions\firefox\icons\icon48.png"
  Delete "$INSTDIR\extensions\firefox\icons\icon128.png"
  RMDir "$INSTDIR\extensions\firefox\icons"
  Delete "$INSTDIR\extensions\firefox\manifest.json"
  Delete "$INSTDIR\extensions\firefox\popup.html"
  Delete "$INSTDIR\extensions\firefox\early.js"
  Delete "$INSTDIR\extensions\firefox\background.js"
  Delete "$INSTDIR\extensions\firefox\content.js"
  RMDir "$INSTDIR\extensions\firefox"
  RMDir "$INSTDIR\extensions"

  Delete "$INSTDIR\SlipStream.exe"
  Delete "$INSTDIR\slipstream-desktop.exe"
  Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SlipStream"
SectionEnd
