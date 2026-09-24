; ---- reinstall-page default-selection behaviour probe (GENERATED) ---------
; Do not edit by hand -- regenerate with src-tauri/nsis/gen-reinstall-probe.py
;
; WHAT IT TESTS
;   Which of the two radio buttons on the maintenance ("reinstall") page ends up
;   CHECKED by default, in each of the three situations the installer
;   distinguishes, and for each button the user may have picked before.
;
;   The block that makes that decision is spliced VERBATIM from the production
;   template, so this measures the code that ships, not a copy of it.
;
; INPUTS, as the production template defines them
;   $R0                  0 = same version installed, 1 = upgrading, -1 = downgrading
;   $ReinstallPageCheck  0 = first entry, 1 = first button chosen last time,
;                        2 = second button chosen last time
;
; EXPECTED OUTCOME (this project's decision, see docs/MAINTENANCE-LIABILITIES.md M-01)
;   upgrading / downgrading, first entry  -> 2nd button ("do not uninstall")
;   same version, first entry             -> 1st button (upstream behaviour kept)
;   previously picked 2nd                 -> 2nd button (kept)
;   previously picked 1st, while upgrading-> 2nd button (overridden on purpose:
;                                            the destructive default is exactly
;                                            what this project removed)
;
; HOW TO RUN
;   makensis -V2 reinstall-default-selftest.nsi
;   wine reinstall-default-selftest.exe /MODE=<n>            # n = 1..6
;   Result: C:\\tiez-probe\\result.txt inside the wine prefix.
;   Must run in GUI mode -- NSIS creates no pages in silent mode.

Unicode true
!include "LogicLib.nsh"
!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "FileFunc.nsh"

!ifndef PROBE_OUTFILE
  !define PROBE_OUTFILE "reinstall-default-selftest.exe"
!endif

Var ReinstallPageCheck        ; same variable name as the production template
Var R2Enabled
Var R3Enabled
Var Observed
Var Expected
Var LogFile

LangString uninstallBeforeInstalling ${LANG_ENGLISH} "Uninstall before installing"
LangString dontUninstall            ${LANG_ENGLISH} "Do not uninstall"

Name "reinstall-default-selftest"
OutFile "${PROBE_OUTFILE}"
InstallDir "$TEMP\tiez-reinstall-selftest"

Page custom ProbeCreate
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ; NOTE: `/MODE=1` makes GetOptions for "/MODE" return the string "=1". Match the
  ; equals sign so $9 really is the number; otherwise every mode falls through to
  ; the same branch and the probe passes vacuously.
  ${GetOptions} $CMDLINE "/MODE=" $9
  StrCpy $ReinstallPageCheck 0
  StrCpy $Expected 99          ; 99 = "unknown mode" -> must FAIL loudly
  ${If} $9 == 1          ; upgrade    , first entry
    StrCpy $R0 1
    StrCpy $Expected 2
  ${ElseIf} $9 == 2      ; downgrade  , first entry
    StrCpy $R0 -1
    StrCpy $Expected 2
  ${ElseIf} $9 == 3      ; same ver.  , first entry
    StrCpy $R0 0
    StrCpy $Expected 1
  ${ElseIf} $9 == 4      ; upgrade    , user had picked 1st
    StrCpy $R0 1
    StrCpy $ReinstallPageCheck 1
    StrCpy $Expected 2
  ${ElseIf} $9 == 5      ; same ver.  , user had picked 1st
    StrCpy $R0 0
    StrCpy $ReinstallPageCheck 1
    StrCpy $Expected 1
  ${ElseIf} $9 == 6      ; upgrade    , user had picked 2nd
    StrCpy $R0 1
    StrCpy $ReinstallPageCheck 2
    StrCpy $Expected 2
  ${Else}
    ; Unknown or unparsed mode: keep a neutral $R0 and let the assertion fail, so
    ; a harness regression can never masquerade as a PASS.
    StrCpy $R0 1
    StrCpy $Expected 99
  ${EndIf}
FunctionEnd

Function ProbeCreate
  nsDialogs::Create 1018
  Pop $0

  ; --- radio buttons built exactly like the production template -------------
  StrCpy $R2 "$(uninstallBeforeInstalling)"
  StrCpy $R3 "$(dontUninstall)"
  ${NSD_CreateRadioButton} 30u 50u -30u 8u $R2
  Pop $R2
  ${NSD_CreateRadioButton} 30u 70u -30u 8u $R3
  Pop $R3

  ; --- >>>>> VERBATIM PRODUCTION BLOCK UNDER TEST <<<<< --------------------

    ; --- Tiez-Next 本地改动（上游模板原文见下，取值见官方 installer.nsi）-----------
    ; Default the reinstall page to the *non-destructive* option when upgrading or
    ; downgrading, so that hitting Next never runs the old uninstaller.
    ; Same version keeps upstream behaviour ("add or reinstall" is harmless there).
    ; $R0 = 0 same, 1 upgrading, -1 downgrading.
    ;
    ; This is the ONLY intentional divergence from the upstream template; a precise
    ; diff, the upstream sync procedure and a behavioural probe live in
    ; docs/MAINTENANCE-LIABILITIES.md (entry M-01) and
    ; src-tauri/nsis/reinstall-default-selftest.nsi.
    ; Check the first radio button if this the first time
    ; we enter this page or if the second button wasn't
    ; selected the last time we were on this page
    ; Upstream compares `= 1` / `= -1`; `<> 0` is the same test and needs 4 fewer
    ; instructions (this block runs on every entry into the page).
    ${If} $ReinstallPageCheck <> 2
      ${If} $R0 <> 0
        SendMessage $R3 ${BM_SETCHECK} ${BST_CHECKED} 0
      ${Else}
        SendMessage $R2 ${BM_SETCHECK} ${BST_CHECKED} 0
      ${EndIf}
    ${Else}
      SendMessage $R3 ${BM_SETCHECK} ${BST_CHECKED} 0
    ${EndIf}
    ; --- Tiez-Next 本地改动结束 ---------------------------------------------------

    ${If} $R0 <> 0
      ${NSD_SetFocus} $R3
    ${Else}
      ${NSD_SetFocus} $R2
    ${EndIf}
  ; --- <<<<< END VERBATIM BLOCK -------------------------------------------

  ; Read the state straight back. ${NSD_GetState} sends BM_GETCHECK to the real
  ; control -- the same call the installer's own leave function makes -- so the
  ; outcome is observable without anyone having to click Next.
  ${NSD_GetState} $R2 $2
  ${NSD_GetState} $R3 $3
  ${If} $2 == ${BST_CHECKED}
    StrCpy $Observed 1
  ${ElseIf} $3 == ${BST_CHECKED}
    StrCpy $Observed 2
  ${Else}
    ; Neither button checked: report it distinctly instead of silently looking
    ; like "the second button is checked" (that ambiguity would hide a real bug).
    StrCpy $Observed 0
  ${EndIf}
  ; IsWindowEnabled is not an NSIS instruction (it lives in the WinVer/System
  ; plugin), so an "is it disabled?" reading is left to the edge-case harness.
  StrCpy $R2Enabled -1
  StrCpy $R3Enabled -1

  FileOpen $LogFile "C:\tiez-probe\result.txt" w
  FileWrite $LogFile "MODE=$9 R0=$R0 REPAGE=$ReinstallPageCheck EXPECT=$Expected OBSERVED=$Observed R2STATE=$2 R3STATE=$3 R2EN=$R2Enabled R3EN=$R3Enabled$\r$\n"
  ${If} $Observed == $Expected
    FileWrite $LogFile "RESULT=PASS$\r$\n"
  ${Else}
    FileWrite $LogFile "RESULT=FAIL$\r$\n"
  ${EndIf}
  FileClose $LogFile

  ; Never reach nsDialogs::Show: this probe asserts the DEFAULT selection only and
  ; must not walk through the rest of a (stub) install.
  Quit
FunctionEnd

Section "probe"
SectionEnd
