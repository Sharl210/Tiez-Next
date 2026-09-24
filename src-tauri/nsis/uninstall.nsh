!include "LogicLib.nsh"

# ============================================================================
# 数据与执行者分离 · 删除护栏
# ============================================================================
#
# 【要防的事】本文件的 POSTUNINSTALL 会在删除文件之后拉起一个后台清理助手，用
# `Remove-Item -Recurse -Force` 递归删除 $INSTDIR。递归删除是"整个目录全抹掉"，只要
# $INSTDIR 指向的对象不是我们自己的安装目录，被抹掉的就可能是**用户不可再生的数据**
# ——历史上便携版正是把 `data\` 放在程序目录里的。
#
# 【三道闸】递归删除只在三条**同时**成立时放行：
#   1. $INSTDIR 非空、长度合理，且不是盘根（`C:\` 这种）——防"用户把安装目录选成盘根"；
#   2. 目录里确实有本应用的安装标记（uninstall.exe 或主程序）——没有标记就不是我们的
#      安装目录，宁可留下一些残留，也不删不属于我们的东西；
#   3. 目录里**没有任何数据痕迹**（`data\`、`clipboard.db`、`attachments\`）——命中就
#      整段保留目录并提示用户，绝不静默递归删除。
#
# 【为什么判定必须在 PREUNINSTALL】模板在 `Section Uninstall` 里会先删主程序与
# uninstall.exe（见生成产物 installer.nsi 的删除段），等 POSTUNINSTALL 再想确认
# "这真是我们的安装目录"时，证据已经被自己删掉了。因此身份与数据判定在 PREUNINSTALL
# 一次做完、结论存进变量，POSTUNINSTALL 只按结论行事，不重新推断。
#
# 【应用数据目录不归这里管】`%APPDATA%\com.tieznext` 与 `%LOCALAPPDATA%\com.tieznext`
# 的删除**只由模板的"同时删除应用数据"复选框驱动**（`$DeleteAppDataCheckboxState`），
# 本文件一行都不碰——这正是"卸载默认不碰数据"的实现方式。注意：本版起
# `%LOCALAPPDATA%\com.tieznext` 已是新安装的**默认数据目录**，因此那个复选框一勾就会
# 删掉真实数据；它是用户的显式选择，我们保持其原样语义，不额外扩大删除面。
#
# 变量：$TiezInstDirPurgeOk = 1 表示放行递归删除；$TiezInstDirHasData = 1 表示目录内
# 检测到数据（用于提示与保留）。两者互斥，前者为 0 时一律保留。

Var TiezInstDirPurgeOk
Var TiezInstDirHasData
Var TiezGuardTmp
Var TiezGuardMark

!macro TIEZ_GUARD_DECIDE_INSTDIR_PURGE
  StrCpy $TiezInstDirPurgeOk 0
  StrCpy $TiezInstDirHasData 0

  ${If} $INSTDIR == ""
    DetailPrint "[护栏] $INSTDIR 为空，拒绝任何递归删除。"
  ${Else}
    StrLen $TiezGuardTmp $INSTDIR
    ${If} $TiezGuardTmp <= 3
      # `C:\`(3) / `C:`(2) / 空壳都落在这里：长度不足以构成一个应用安装目录。
      DetailPrint "[护栏] $INSTDIR 过短（疑为盘根或裸盘符），拒绝递归删除。"
    ${Else}
      StrCpy $TiezGuardTmp $INSTDIR 1 -1
      ${If} $TiezGuardTmp == "\"
        DetailPrint "[护栏] $INSTDIR 以反斜杠结尾（形如盘根），拒绝递归删除。"
      ${Else}
        StrCpy $TiezGuardTmp $INSTDIR 1 -1
        ${If} $TiezGuardTmp == ":"
          DetailPrint "[护栏] $INSTDIR 以冒号结尾（裸盘符），拒绝递归删除。"
        ${Else}
          # ---- 数据痕迹检测（历史便携形态）----
          # 先于身份判定做：即便目录里找不到本应用标记，只要出现数据形态的痕迹，
          # 也要用"检测到数据"这个更强的提示告诉用户——那正是最需要提醒的情形
          # （$INSTDIR 被指到了用户自己的数据目录）。目录本身存在即算命中，不要求
          # 里面一定有文件；保守方向是"宁可留下残留也不碰数据"。
          ${If} ${FileExists} "$INSTDIR\data"
            StrCpy $TiezInstDirHasData 1
          ${EndIf}
          ${If} ${FileExists} "$INSTDIR\data\clipboard.db"
            StrCpy $TiezInstDirHasData 1
          ${EndIf}
          ${If} ${FileExists} "$INSTDIR\clipboard.db"
            StrCpy $TiezInstDirHasData 1
          ${EndIf}
          ${If} ${FileExists} "$INSTDIR\attachments"
            StrCpy $TiezInstDirHasData 1
          ${EndIf}

          # ---- 闸 2：本应用的安装标记 ----
          # 用专用变量而非 $1：模板的卸载确认页把 $1 当窗口句柄用，复用会埋下污染。
          StrCpy $TiezGuardMark 0
          ${If} ${FileExists} "$INSTDIR\uninstall.exe"
            StrCpy $TiezGuardMark 1
          ${ElseIf} ${FileExists} "$INSTDIR\tiez-next.exe"
            StrCpy $TiezGuardMark 1
          ${ElseIf} ${FileExists} "$INSTDIR\TieZ.exe"
            StrCpy $TiezGuardMark 1
          ${ElseIf} ${FileExists} "$INSTDIR\tiez-app.exe"
            StrCpy $TiezGuardMark 1
          ${EndIf}

          ${If} $TiezGuardMark == 0
            DetailPrint "[护栏] $INSTDIR 内找不到本应用安装标记，拒绝递归删除（保留目录）。"
          ${ElseIf} $TiezInstDirHasData == 1
            DetailPrint "[护栏] $INSTDIR 内检测到数据痕迹，保留目录、不做递归删除。"
          ${Else}
            StrCpy $TiezInstDirPurgeOk 1
            DetailPrint "[护栏] $INSTDIR 已确认为本应用安装目录且不含数据，放行残留清理。"
          ${EndIf}
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Stopping Tiez-Next before uninstall..."

  # 应用关闭主窗口时会缩到托盘，卸载器不能依赖普通的关闭请求。
  # 这里在开始删除文件前强制结束已安装的进程，避免 exe 被占用。
  # Tiez-Next 为本项目现用名；TieZ / tiez-app / tie-z 为上游遗留名，保留以便从旧版覆盖安装的用户也能干净卸载。
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /T /IM tiez-next.exe'
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /T /IM TieZ.exe'
  nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /T /IM tiez-app.exe'
  Sleep 1200

  # 删除动作开始**之前**完成身份与数据判定（此刻 uninstall.exe 还在，证据完整）。
  !insertmacro TIEZ_GUARD_DECIDE_INSTDIR_PURGE

  # 目录里有数据时**必须提示用户**，不能静默保留让他以为卸载没生效。
  # 静默安装（/S）与被动模式（应用内更新）下不弹窗：更新路径本就不该打扰用户，
  # 详情仍会写进卸载日志供排查。
  ${If} $TiezInstDirHasData == 1
    IfSilent tiez_guard_no_prompt
    ${If} $PassiveMode <> 1
      MessageBox MB_OK|MB_ICONEXCLAMATION \
        "检测到安装目录内含有数据：$\r$\n$INSTDIR$\r$\n$\r$\n为保护这些数据，卸载程序不会删除该目录，只移除程序文件。$\r$\n请自行确认并处理该目录内的数据。$\r$\n$\r$\nData was found inside the install directory. It has been preserved and will not be deleted. Please review it yourself."
    ${EndIf}
    tiez_guard_no_prompt:
  ${EndIf}

  # 先清理常见的自启动注册表项，避免系统在卸载后继续拉起已删除的程序。
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Tiez-Next"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "TieZ"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "tie-z"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "tiez-app"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DetailPrint "Restoring Windows Clipboard settings..."
  
  # 1. Restore EnableClipboardHistory and EnableCloudClipboard to default (1)
  # This ensures Win+V works again even if the app was used to disable it.
  WriteRegDWORD HKCU "Software\Microsoft\Clipboard" "EnableClipboardHistory" 1
  WriteRegDWORD HKCU "Software\Microsoft\Clipboard" "EnableCloudClipboard" 1
  
  # 2. Remove 'V' from DisabledHotkeys
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced" "DisabledHotkeys"
  ${If} $0 != ""
    # Simple primitive string removal for 'V' and 'v'
    Push "V" # String to replace
    Push ""  # Replace with
    Push $0  # Original string
    Call un.StrReplace
    Pop $0
    
    Push "v" # String to replace
    Push ""  # Replace with
    Push $0  # Original string
    Call un.StrReplace
    Pop $0
    
    ${If} $0 == ""
      DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced" "DisabledHotkeys"
    ${Else}
      WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced" "DisabledHotkeys" $0
    ${EndIf}
  ${EndIf}

  # 3. Clean up Policy if it exists
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Policies\Explorer" "DisallowClipboardHistory"
  DeleteRegValue HKCU "Software\Policies\Microsoft\Windows\System" "AllowClipboardHistory"
  DeleteRegValue HKCU "Software\Policies\Microsoft\Windows\System" "AllowCrossDeviceClipboard"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Tiez-Next"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "TieZ"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "tie-z"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "tiez-app"
  # 清理 NSIS 记录的上次安装路径，避免后续静默安装继续复用旧目录。
  DeleteRegKey HKCU "Software\tieznext\Tiez-Next"
  DeleteRegKey /ifempty HKCU "Software\tieznext"
  DeleteRegKey HKCU "Software\tiez\TieZ"
  DeleteRegKey /ifempty HKCU "Software\tiez"
  
  DetailPrint "Windows Clipboard settings restored."
  
  # 4. Restart Explorer to make DisabledHotkeys changes take effect
  # We use a silent powershell command to be as non-intrusive as possible
  DetailPrint "Restarting Explorer to apply changes..."
  nsExec::Exec '"powershell.exe" -NoProfile -WindowStyle Hidden -Command "Stop-Process -Name explorer -Force; Start-Process explorer"'
  DetailPrint "Explorer restarted."

  # 5. 如果卸载时程序刚被关闭，或者 uninstall.exe 还没完全退出，
  # NSIS 可能暂时删不掉安装目录。这里额外拉起一个后台清理助手，
  # 等卸载器退出后再尝试删除残留目录和文件。
  #
  # 【护栏】递归删除只在 PREUNINSTALL 的三道闸全部放行时才执行
  # （$INSTDIR 合法 + 含本应用安装标记 + 目录内无任何数据痕迹）。
  # 不满足时**保留目录**：多留一个空目录是无害的，删掉用户数据是不可逆的。
  ${If} $TiezInstDirPurgeOk == 1
    DetailPrint "Scheduling leftover install directory cleanup..."
    nsExec::Exec '"$SYSDIR\cmd.exe" /C start "" /MIN powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command "Start-Sleep -Seconds 3; Stop-Process -Name tiez-next,TieZ,tiez-app -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath ''$INSTDIR'' -Force -Recurse -ErrorAction SilentlyContinue"'
  ${ElseIf} $TiezInstDirHasData == 1
    DetailPrint "[护栏] 安装目录含数据，已保留：$INSTDIR（不执行递归删除，数据完整）"
  ${Else}
    DetailPrint "[护栏] 未确认安装目录身份，已跳过递归删除：$INSTDIR"
  ${EndIf}
!macroend

# Function for string replacement (Uninstall version)
Function un.StrReplace
  Exch $0 # Original string (input/output)
  Exch
  Exch $1 # Replace with
  Exch
  Exch 2
  Exch $2 # String to replace
  Exch 2
  Push $3 # Length of string to replace
  Push $4 # Current original string length
  Push $5 # Length of replacement string
  Push $6 # Current index
  Push $7 # Current substring
  
  StrLen $3 $2
  ${If} $3 == 0
    Goto StrReplace_End
  ${EndIf}
  
  StrLen $4 $0
  StrLen $5 $1
  StrCpy $6 0
  
  StrReplace_Loop:
    StrCpy $7 $0 $3 $6
    ${If} $7 == $2
      # Found a match
      StrCpy $7 $0 $6 # Text before match
      IntOp $6 $6 + $3
      StrCpy $0 $0 "" $6 # Text after match
      StrCpy $0 $7$1$0 # New string
      StrLen $4 $0 # New length
      IntOp $6 $7 + $5 # Move index past replacement
    ${Else}
      IntOp $6 $6 + 1
    ${EndIf}
    
    ${If} $6 < $4
      Goto StrReplace_Loop
    ${EndIf}
    
  StrReplace_End:
  Pop $7
  Pop $6
  Pop $5
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Exch $0
FunctionEnd
