; ---------------------------------------------------------------------------
; 卸载器删除护栏的**可执行**验证（WSL2 上经 wine 真实运行 NSIS 二进制）。
;
; 做法：直接 include 生产用的 uninstall.nsh，只调用其中的判定宏
; `TIEZ_GUARD_DECIDE_INSTDIR_PURGE`，对同一个 $INSTDIR 逐个布置目录形状，
; 观察它是否放行递归删除。这样验证的是**真正会被编进安装包的那份逻辑**，
; 而不是复制一份改写的近似物。
;
; 每个用例打印 CASE / EXPECT / ACTUAL，任一不符即令退出码非 0。
;
; 运行方法（Windows 宿主或 WSL2+wine）：
;   makensis -V2 src-tauri/nsis/guard-selftest.nsi
;   wine src-tauri/nsis/guard-selftest.exe /S
;   结果写入 %TEMP%\tiez-guard-result.txt（WSL2 下为
;   ~/.wine/drive_c/users/<user>/AppData/Local/Temp/tiez-guard-result.txt）
;
; 【反向对照】把本文件第 16 行的 include 换成下面这个"改动前行为"的宏再跑一遍，
; 10/13 条必须变红（3 条不红的是负向对照用例——"干净安装应放行"——本就该通过）：
;
;   !macro TIEZ_GUARD_DECIDE_INSTDIR_PURGE
;     StrCpy $TiezInstDirPurgeOk 1
;     StrCpy $TiezInstDirHasData 0
;   !macroend
;   Var TiezInstDirPurgeOk
;   Var TiezInstDirHasData
;   Var TiezGuardTmp
;   Var TiezGuardMark
; ---------------------------------------------------------------------------

Unicode true
!include "LogicLib.nsh"

; 生产文件。harness 与被验证对象共用同一份源码。
!include "${__FILEDIR__}/uninstall.nsh"

!define WORKROOT "$TEMP\tiez-guard-cases"

Var Failures
Var CasesRun

Var CaseFailed
Var LogFile
Var LogLine

!macro LOG _text
  FileWrite $LogFile "${_text}$\r$\n"
!macroend

!macro EXPECT_CASE _name _expect
  IntOp $CasesRun $CasesRun + 1
  StrCpy $CaseFailed 0
  !insertmacro TIEZ_GUARD_DECIDE_INSTDIR_PURGE
  ${If} $TiezInstDirPurgeOk == ${_expect}
    !insertmacro LOG "CASE=${_name} EXPECT=${_expect} ACTUAL=$TiezInstDirPurgeOk HASDATA=$TiezInstDirHasData -> PASS"
  ${Else}
    StrCpy $CaseFailed 1
    IntOp $Failures $Failures + 1
    !insertmacro LOG "CASE=${_name} EXPECT=${_expect} ACTUAL=$TiezInstDirPurgeOk HASDATA=$TiezInstDirHasData -> FAIL"
  ${EndIf}
!macroend

!macro RESET_CASE _dir
  RMDir /r "${WORKROOT}"
  CreateDirectory "${WORKROOT}"
  StrCpy $INSTDIR "${_dir}"
!macroend

Section "guard"
  StrCpy $CasesRun 0
  StrCpy $Failures 0
  StrCpy $TiezInstDirPurgeOk 0
  StrCpy $TiezInstDirHasData 0
  SetErrorLevel 0
  FileOpen $LogFile "$TEMP\tiez-guard-result.txt" w

  ; --- 闸 1：$INSTDIR 合法性 -------------------------------------------------
  ; 空路径：不得递归删除（空字符串作 -LiteralPath 是灾难性输入）。
  !insertmacro RESET_CASE ""
  !insertmacro EXPECT_CASE "empty-install-dir" 0

  ; 盘根：长度闸拦下（`C:\` 只有 3 个字符）。
  !insertmacro RESET_CASE "C:\"
  !insertmacro EXPECT_CASE "drive-root-with-slash" 0

  ; 裸盘符。
  !insertmacro RESET_CASE "C:"
  !insertmacro EXPECT_CASE "bare-drive-letter" 0

  ; 以反斜杠结尾的非盘根路径（可能被用户选成某个共享根）。
  !insertmacro RESET_CASE "C:\SomeRoot\"
  !insertmacro EXPECT_CASE "trailing-backslash" 0

  ; --- 闸 2：安装标记 --------------------------------------------------------
  ; 目录存在但没有本应用任何标记（用户把一个数据目录填进了 $INSTDIR）。
  !insertmacro RESET_CASE "${WORKROOT}\NotOurs"
  CreateDirectory "${WORKROOT}\NotOurs"
  FileOpen $9 "${WORKROOT}\NotOurs\user-file.txt" w
  FileWrite $9 "user data"
  FileClose $9
  !insertmacro EXPECT_CASE "no-app-marker" 0

  ; 只有 uninstall.exe，无数据 → 放行。
  !insertmacro RESET_CASE "${WORKROOT}\CleanInstall"
  CreateDirectory "${WORKROOT}\CleanInstall"
  FileOpen $9 "${WORKROOT}\CleanInstall\uninstall.exe" w
  FileWrite $9 "stub"
  FileClose $9
  FileOpen $9 "${WORKROOT}\CleanInstall\tiez-next.exe" w
  FileWrite $9 "stub"
  FileClose $9
  !insertmacro EXPECT_CASE "clean-install-passes" 1

  ; 标记是历史主程序名（从旧版覆盖安装的形态）→ 也应放行。
  !insertmacro RESET_CASE "${WORKROOT}\LegacyExe"
  CreateDirectory "${WORKROOT}\LegacyExe"
  FileOpen $9 "${WORKROOT}\LegacyExe\TieZ.exe" w
  FileWrite $9 "stub"
  FileClose $9
  !insertmacro EXPECT_CASE "legacy-exe-passes" 1

  ; --- 闸 3：数据痕迹（本次改动的核心）--------------------------------------
  ; 便携形态：安装目录里 data/ 且**装着数据库** → 必须保留，绝不递归删除。
  !insertmacro RESET_CASE "${WORKROOT}\PortableWithData"
  CreateDirectory "${WORKROOT}\PortableWithData"
  CreateDirectory "${WORKROOT}\PortableWithData\data"
  FileOpen $9 "${WORKROOT}\PortableWithData\uninstall.exe" w
  FileWrite $9 "stub"
  FileClose $9
  FileOpen $9 "${WORKROOT}\PortableWithData\data\clipboard.db" w
  FileWrite $9 "SQLite format 3"
  FileClose $9
  !insertmacro EXPECT_CASE "portable-with-database-is-preserved" 0

  ; data/ 是空目录也算数据痕迹（保守：宁可留残留也不碰）。
  !insertmacro RESET_CASE "${WORKROOT}\EmptyDataDir"
  CreateDirectory "${WORKROOT}\EmptyDataDir"
  CreateDirectory "${WORKROOT}\EmptyDataDir\data"
  FileOpen $9 "${WORKROOT}\EmptyDataDir\uninstall.exe" w
  FileWrite $9 "stub"
  FileClose $9
  !insertmacro EXPECT_CASE "empty-data-dir-still-preserved" 0

  ; 数据库直接躺在安装目录根（另一种历史布局）。
  !insertmacro RESET_CASE "${WORKROOT}\DbAtRoot"
  CreateDirectory "${WORKROOT}\DbAtRoot"
  FileOpen $9 "${WORKROOT}\DbAtRoot\uninstall.exe" w
  FileWrite $9 "stub"
  FileClose $9
  FileOpen $9 "${WORKROOT}\DbAtRoot\clipboard.db" w
  FileWrite $9 "SQLite format 3"
  FileClose $9
  !insertmacro EXPECT_CASE "database-at-install-root-is-preserved" 0

  ; attachments/ 目录同样算数据痕迹（附件是不可再生的用户资产）。
  !insertmacro RESET_CASE "${WORKROOT}\Attachments"
  CreateDirectory "${WORKROOT}\Attachments"
  CreateDirectory "${WORKROOT}\Attachments\attachments"
  FileOpen $9 "${WORKROOT}\Attachments\uninstall.exe" w
  FileWrite $9 "stub"
  FileClose $9
  !insertmacro EXPECT_CASE "attachments-dir-is-preserved" 0

  ; 无安装标记、但目录里有数据（$INSTDIR 被指到了用户自己的数据目录）：
  ; 保留 + 走"检测到数据"的强提示，而不是"身份未确认"。
  !insertmacro RESET_CASE "${WORKROOT}\NotOursWithData"
  CreateDirectory "${WORKROOT}\NotOursWithData"
  CreateDirectory "${WORKROOT}\NotOursWithData\attachments"
  FileOpen $9 "${WORKROOT}\NotOursWithData\clipboard.db" w
  FileWrite $9 "SQLite format 3"
  FileClose $9
  !insertmacro EXPECT_CASE "no-marker-but-has-data-is-preserved" 0

  ; 与本次架构决策配套的对照：数据目录在安装目录**之外**时放行，
  ; 说明护栏不会因为"默认数据目录在 %LOCALAPPDATA%"而误拦正常的干净卸载。
  !insertmacro RESET_CASE "${WORKROOT}\SeparatedLayout"
  CreateDirectory "${WORKROOT}\SeparatedLayout"
  CreateDirectory "${WORKROOT}\SeparatedLayoutData"
  FileOpen $9 "${WORKROOT}\SeparatedLayout\uninstall.exe" w
  FileWrite $9 "stub"
  FileClose $9
  FileOpen $9 "${WORKROOT}\SeparatedLayoutData\clipboard.db" w
  FileWrite $9 "SQLite format 3"
  FileClose $9
  !insertmacro EXPECT_CASE "data-outside-install-dir-passes" 1

  RMDir /r "${WORKROOT}"

  !insertmacro LOG "SUMMARY cases=$CasesRun failures=$Failures"
  FileClose $LogFile
  ${If} $Failures != 0
    SetErrorLevel 1
  ${EndIf}
SectionEnd
