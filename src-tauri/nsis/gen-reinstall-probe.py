#!/usr/bin/env python3
"""Generate the behaviour probe for the installer's reinstall-page default.

WHAT THIS IS FOR
    The installer's maintenance ("reinstall") page shows two radio buttons:
    "uninstall before installing" and "do not uninstall". Upstream's template
    always defaults to the FIRST one, i.e. running the old uninstaller. This
    project overrides that default for upgrades and downgrades.

    The override lives inside the page-creation function of a large script that
    cannot be compiled or stepped through on its own, so this generator splices
    the *verbatim* block out of the production template into a tiny standalone
    installer, which can then actually be run (see below) to observe which radio
    button ends up checked.

    Because the block is spliced rather than copied by hand, the probe cannot
    drift away from the code that ships.

HOW TO USE

    # 1. regenerate the probe from the shipped template
    python3 src-tauri/nsis/gen-reinstall-probe.py

    # 2. build and run it (one run per mode; each run exits by itself).
    #    It MUST run in GUI mode: NSIS does not create pages in silent mode, so
    #    the block under test would never execute there. Needs an X display
    #    (on WSL2: wine + the WSLg/desktop display).
    makensis -V2 src-tauri/nsis/reinstall-default-selftest.nsi
    wine src-tauri/nsis/reinstall-default-selftest.exe /MODE=1     # ... up to 6
    cat ~/.wine/drive_c/tiez-probe/result.txt

    # 3. reverse control -- the same probe built against the UPSTREAM template
    #    must FAIL for modes 1, 2 and 4. If it passes there too, the probe is not
    #    testing anything:
    python3 src-tauri/nsis/gen-reinstall-probe.py <upstream-installer.nsi> /tmp/upstream-probe.nsi
    makensis -V2 -DPROBE_OUTFILE=upstream-probe.exe /tmp/upstream-probe.nsi
    wine /tmp/upstream-probe.exe /MODE=1        # expect RESULT=FAIL

    Where to get the upstream template for the reverse control: the template the
    bundler ships is embedded uncompressed in the CLI binary, starting at the
    offset of `Unicode true` (see docs/MAINTENANCE-LIABILITIES.md, entry M-01).
"""
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
DEFAULT_SRC = HERE / "installer.nsi"
DEFAULT_OUT = HERE / "reinstall-default-selftest.nsi"

SRC = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_SRC
OUT = pathlib.Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else DEFAULT_OUT

text = SRC.read_text(encoding="utf-8")
PATCHED = "$R0 <> 0" in text

# The block under test runs from right after the two radio buttons are created,
# up to (but not including) the call that shows the dialog.
START = "${NSD_OnClick} $R3 PageReinstallUpdateSelection\n"
END = "\n    nsDialogs::Show"
i = text.index(START) + len(START)
j = text.index(END, i)
snippet = text[i:j]

if PATCHED:
    # Guardrails: make sure we grabbed the patched block, not an upstream one.
    assert "Tiez-Next" in snippet, "snippet is not the patched block"
    assert "$R0 <> 0" in snippet, "patched comparison missing from snippet"
else:
    assert "$R0 <> 0" not in snippet, "expected an upstream (unpatched) snippet"

PROBE = r'''; ---- reinstall-page default-selection behaviour probe (GENERATED) ---------
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
__SNIPPET__
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
'''

OUT.write_text(PROBE.replace("__SNIPPET__", snippet.rstrip()), encoding="utf-8")
print("wrote %s (%d bytes)" % (OUT, OUT.stat().st_size))
print("spliced %d bytes from %s%s" % (len(snippet), SRC, "" if PATCHED else "  [UPSTREAM - reverse control]"))
