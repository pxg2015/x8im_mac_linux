; X8Assist 融合安装脚本（A+ 方案：perMachine 安装 + 安装期设每机随机密码）
; 由 tauri.conf.json 的 bundle.windows.nsis.installerHooks 引入，在 Tauri 生成的
; NSIS 安装器内执行（perMachine → 安装器已提权，以下操作无需再弹 UAC）。
;
; 关键事实（2026-10-16 实测）：
;   · RustDesk 的 is_installed() 只认 exe 是否位于 Program Files；
;     装到 %LOCALAPPDATA% 的副本即使以管理员身份执行 --password 也被拒。
;   · 故本脚本不依赖 X8Assist-Setup.exe（其自安装硬编码 per-user、且会弹 GUI），
;     改为把内置的完整 X8Assist 文件树（resources: bin/X8Assist/**）直接铺到
;     C:\Program Files\X8Assist\，再调用其 --password 写入加密密码。
;   · 密码明文落 C:\Program Files\X8Assist\x8pwd.txt（该目录由 step1 robocopy 保证存在；
;     曾用 C:\ProgramData\X8Assist 但 POSTINSTALL 的 CreateDirectory 未生效 → 文件缺失）。
;   · 密码生成用 PowerShell 内联（nsExec::ExecToStack 捕获 stdout 至 $1），不经临时 .ps1
;     （后者在 POSTINSTALL 时机下 $PLUGINSDIR 可能已被清理，导致 $1 为空、静默设成空密码）。

; ───── 安装前：强杀正在运行的旧实例（2026-10-23 老8：同一台电脑只许一个实例）─────
;
; 根因（实测）：Tauri 的 NSIS 模板**没有**「应用正在运行」检查，静默升级
; （updater-task.cmd → setup.exe /S /UPDATE /R）也不会 taskkill 旧进程。NSIS 覆盖
; 正在运行的 exe 用的是「改名旧文件 + 写新文件」，所以旧进程照旧活着；装完 .cmd
; 再经 explorer 启动新 exe → 一台机器上出现两个 x8im.exe（旧实例还占着单实例锁，
; 新实例反倒成了"第二个"）。本机 2026-10-23 实测就是 9:13 与 14:03 两个实例。
;
; 放在 PREINSTALL（覆盖文件之前）执行，连杀两次中间留 800ms——第一次杀掉后文件句柄
; 释放需要一点时间，否则第二次覆盖仍可能失败（NSIS 会重试，但界面会卡住几秒）。
!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToLog 'taskkill /F /IM x8im.exe'
  Sleep 800
  nsExec::ExecToLog 'taskkill /F /IM x8im.exe'
  Sleep 400
  ; 2026-11-03 晚：升级前清空安装目录，彻底清掉旧版本遗留文件
  ; （NSIS /UPDATE 只覆盖同名文件、不删多余残留；不同名旧文件会一直留着）。
  ; $INSTDIR 为纯程序目录（用户数据在 AppData\com.x8fde.im），完整包重建无副作用。
  RMDir /r "$INSTDIR"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; ==== 分步诊断日志（写入 $TEMP\x8inst.log，供安装后排查）====
  FileOpen $8 "$TEMP\x8inst.log" w
  FileWrite $8 "INSTDIR=[$INSTDIR]$\r$\nSYSDIR=[$SYSDIR]$\r$\nWINDIR=[$WINDIR]$\r$\nLOCALAPPDATA=[$LOCALAPPDATA]$\r$\nPROFILE=[$PROFILE]$\r$\nTEMP=[$TEMP]$\r$\n"
  FileClose $8

  ; 1) 将内置 X8Assist 完整文件树铺到 Program Files（perMachine 标准位置）
  ;    robocopy 退出码 0-7 视为成功（1=复制了新文件），>=8 才是错误。
  nsExec::ExecToStack 'robocopy "$INSTDIR\bin\X8Assist" "C:\Program Files\X8Assist" /E /NFL /NDL /NJH /NJS /NP'
  Pop $0
  Pop $R7
  FileOpen $8 "$TEMP\x8inst.log" a
  FileWrite $8 "step1_robocopy_exit=[$0]$\r$\n"
  FileClose $8

  ; 2) 生成每机随机 12 位密码（字符池 0-9A-Za-z），明文落 C:\Program Files\X8Assist\x8pwd.txt；
  ;    同一密码经 $1 回传给第 4 步 --password（$1 与落盘值同源）。
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "$$p = -join((48..57)+(65..90)+(97..122) | Get-Random -Count 12 | ForEach-Object {[char]$$_}); [IO.File]::WriteAllText(\"C:\Program Files\X8Assist\x8pwd.txt\", $$p); [Console]::Out.Write($$p)"'
  Pop $0
  Pop $1
  FileOpen $8 "$TEMP\x8inst.log" a
  FileWrite $8 "step2_ps_exit=[$0] pw=[$1]$\r$\n"
  FileClose $8

  ; 3) 起 --server，让 IPC 管道就绪（--password 需经 IPC 写入配置）
  ;    2026-10-26 无感（老8 报「安装完一闪而过的全屏窗」）：这是个 Flutter GUI，
  ;    Exec 拉起后 runner 自己 ShowWindow，脚本层拿不到初始 HWND，只能「启动后高频
  ;    FindWindow + SW_HIDE」压住（与运行时 hide_assist_windows() 同一套路）。
  ;    实测窗口类名 FLUTTER_RUNNER_WIN32_WINDOW、标题 X8Assist（本机探测确认）。
  ;    原来的 Sleep 5000 直接换成 100×50ms 的隐藏轮询 —— 不额外拉长安装时间。
  Exec '"C:\Program Files\X8Assist\X8Assist.exe" --server'
  StrCpy $R4 0
  x8assist_hide_loop:
    System::Call 'user32::FindWindowW(t "FLUTTER_RUNNER_WIN32_WINDOW", t "") i .r1'
    IntCmp $R1 0 +2
      System::Call 'user32::ShowWindow(i r1, i 0)'
    Sleep 50
    IntOp $R4 $R4 + 1
    IntCmp $R4 100 x8assist_hide_done x8assist_hide_loop x8assist_hide_loop
  x8assist_hide_done:

  ; 4) 由 X8Assist 自己按 1.5 加密格式写入同一密码（is_installed && is_root 均满足）
  ExecWait '"C:\Program Files\X8Assist\X8Assist.exe" --password $1'

  ; 5) 关掉安装期临时拉起的提权实例
  ;    （避免高完整性 IPC 管道挡住普通权限 X8.IM 的连接）
  ;    2026-10-16 老8 定：此后不留任何常驻 —— X8Assist 由 X8.IM 在「接受远程协助」时按需拉起。
  nsExec::ExecToLog 'taskkill /F /IM X8Assist.exe'
  nsExec::ExecToLog 'sc stop X8Assist'
  nsExec::ExecToLog 'sc delete X8Assist'
  nsExec::ExecToLog 'taskkill /F /IM X8Assist.exe'

  ; 6) 清理历史 per-user 残留副本（旧版曾装到 %LOCALAPPDATA%\x8assist）
  ;    ⚠️ 实测：本安装器上下文的 $LOCALAPPDATA 被解析为 C:\ProgramData（非用户 LocalAppData），
  ;    故改用 $PROFILE\AppData\Local 精确定位。
  nsExec::ExecToLog 'cmd /c rmdir /s /q "$PROFILE\AppData\Local\x8assist"'
  nsExec::ExecToLog 'cmd /c rmdir /s /q "$PROFILE\AppData\Local\X8Assist"'

  ; 6a) 2026-11-04 起：不再清理 WebView2 缓存目录。
  ;     历史上无论整目录删除还是只删缓存子目录，都会让 Chromium 判定 profile 损坏而重建，
  ;     连 Local Storage 里的 x8_token 一起清掉 → 升级后必重新扫码。
  ;     现 index.html 为 no-cache、JS 文件名带 hash、?v= 每次发版变化，旧缓存不会命中，
  ;     无需清理即可避免白屏，且登录态得以保留。故此处不再执行任何删除。

  ; 7) 2026-10-16 老8 定（性能优先）：**取消开机自启** —— X8Assist 改为按需启动
  ;    （收到并「接受」远程协助时才由 X8.IM 拉起；协助结束/空闲由 X8.IM 回收），平时零常驻零占用。
  ;    显式删除历史版本写入的自启值：已装机器只需升级安装本包即完成清理（HKLM + HKCU 都清）。
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "X8Assist"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "X8Assist"

  ; 8) 2026-10-16 老8 定：**关掉「RDP 会话共享」，永久消除「选择 Windows 会话」弹框**
  ;    根因（RustDesk 1.5.0 源码实测，2026-10-16）：
  ;      · src/server/connection.rs:2269-2285 —— 被控端上报 windows_sessions（控制端据此弹
  ;        「选择会话」框，且默认选中空的 Console → 连过去黑屏）需要四个前提同时成立，
  ;        其中之一就是 platform::windows::is_share_rdp() 为真；
  ;      · src/platform/windows.rs:1059 `share_rdp()` = `get_reg("share_rdp") != "false"`，
  ;        **注册表值不存在（返回空串）也算作「开启」** → 所以没配置过的机器默认就是开启态，
  ;        这正是弹框的来源（不是网络问题，也不是我们漏了什么配置）。
  ;    写入 "false" 后 is_share_rdp()=false → 被控端不再上报多会话 → 控制端不弹框，
  ;    直接接管被控端进程所在会话（按需裸启动时即用户自己的桌面）。
  ;    效果与「改内核重编」等价，但零重编、可逆（删掉该值即恢复原行为）、且可被用户
  ;    在 X8Assist 设置里自行改回。subkey 按 get_valid_subkey() 的优先级全部写入，
  ;    不同安装来源（本安装器 / rdgen 的 X8Assist-Setup.exe）命中不同键，四写必中。
  WriteRegStr HKLM "Software\X8Assist\InstallState\X8Assist" "share_rdp" "false"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\{54E86BC2-6C85-41F3-A9EB-1A94AC9B1F93}_is1" "share_rdp" "false"
  WriteRegStr HKLM "Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{54E86BC2-6C85-41F3-A9EB-1A94AC9B1F93}_is1" "share_rdp" "false"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\X8Assist" "share_rdp" "false"

  ; 9) 2026-10-22 Windows 静默升级基础设施（安装期一次性授权，此后升级零 UAC 零点击）：
  ;    【2026-10-25 品牌迁移】产品对外名 = 邦信；安装目录/任务/脚本名一律保留 ASCII
  ;    （productName=BangXin → $INSTDIR=C:\Program Files\BangXin），刻意不用中文目录：
  ;    下面 .cmd 必须全 ASCII 落盘（见本条注释末尾铁律），中文路径会乱码致升级失效。
  ;    ① updater-task.cmd 落 C:\ProgramData\BangXin\ —— 该目录继承 ProgramData ACL
  ;       （仅管理员可写、普通用户只读），脚本内容不可被普通用户篡改；
  ;    ② 注册计划任务 "BangXin AutoUpdate"：以安装时这位管理员身份、最高权限
  ;       （/RL HIGHEST）、仅交互登录时运行（/IT，保证装完能经 explorer 中转重启）。
  ;    之后客户端发现新版本只需 schtasks /Run 触发（普通权限即可触发自己名下的
  ;    任务，不弹 UAC），由该任务以最高权限跑安装器 /S /UPDATE /R，实现全自动升级。
  ;    .cmd 内容必须全 ASCII：FileWrite 在 Unicode 构建下按 ANSI 字节原样落盘
  ;    （2026-10-22 实测），中文注释在某些代码页下会乱码甚至引入控制字符。
  ;    任务与脚本的清理见 NSIS_HOOK_PREUNINSTALL。
  ;    注意：NSIS 无 $PROGRAMDATA 内置变量（实测会被当未知变量名整段吞掉），
  ;    用 ExpandEnvStrings 展开 %ProgramData%（系统级环境变量，任何进程都在）。
  ExpandEnvStrings $9 "%ProgramData%"
  SetOutPath "$9\BangXin"
  FileOpen $8 "$9\BangXin\updater-task.cmd" w
  FileWrite $8 "@echo off$\r$\n"
  FileWrite $8 "rem BangXin silent patch-update task entry - scheduled task $\"BangXin AutoUpdate$\"$\r$\n"
  FileWrite $8 "rem highest privileges + interactive only. Applies the signed portable zip the$\r$\n"
  FileWrite $8 "rem app downloaded and verified into updates\, then relaunches BangXin. No setup.exe$\r$\n"
  FileWrite $8 "rem wizard is shown. Falls back to the legacy NSIS installer if only that is present.$\r$\n"
  FileWrite $8 "setlocal EnableExtensions$\r$\n"
  FileWrite $8 "set UPD=%ProgramData%\BangXin\updates$\r$\n"
  FileWrite $8 "rem --- component patch (preferred, no setup.exe wizard) ---$\r$\n"
  FileWrite $8 "for /f $\"delims=$\" %%f in ('dir /b /o-d $\"%UPD%\BangXin_*_x64-portable.zip$\" 2^>nul') do ($\r$\n"
  FileWrite $8 "  echo %DATE% %TIME% > $\"%UPD%\task-started.txt$\"$\r$\n"
  FileWrite $8 "  taskkill /F /IM x8im.exe$\r$\n"
  FileWrite $8 "  timeout /t 1 >nul$\r$\n"
  FileWrite $8 "  powershell -NoProfile -ExecutionPolicy Bypass -Command $\"Expand-Archive -Force -Path '%UPD%\%%f' -DestinationPath '$INSTDIR'$\"$\r$\n"
  FileWrite $8 "  call $\"$INSTDIR\post-update.cmd$\"$\r$\n"
  FileWrite $8 "  start $\"$\" $\"$INSTDIR\x8im.exe$\"$\r$\n"
  FileWrite $8 "  del /q $\"%UPD%\%%f$\"$\r$\n"
  FileWrite $8 "  exit /b 0$\r$\n"
  FileWrite $8 ")$\r$\n"
  FileWrite $8 "rem --- legacy NSIS silent install (old clients / manual channel) ---$\r$\n"
  FileWrite $8 "for /f $\"delims=$\" %%f in ('dir /b /o-d $\"%UPD%\BangXin_*_x64-setup.exe$\" 2^>nul') do ($\r$\n"
  FileWrite $8 "  echo %DATE% %TIME% > $\"%UPD%\task-started.txt$\"$\r$\n"
  FileWrite $8 "  $\"%UPD%\%%f$\" /S /UPDATE /R$\r$\n"
  FileWrite $8 "  start $\"$\" $\"$INSTDIR\x8im.exe$\"$\r$\n"
  FileWrite $8 "  del /q $\"%UPD%\%%f$\"$\r$\n"
  FileWrite $8 "  exit /b 0$\r$\n"
  FileWrite $8 ")$\r$\n"
  FileWrite $8 "exit /b 0$\r$\n"
  FileClose $8
  SetOutPath "$INSTDIR"
  ; 2026-10-23 修正（本机事故）：原命令只给 /RL HIGHEST /IT、**没有 /RU**，
  ; 此时 schtasks 会把这个任务挂到 SYSTEM 账户下 → 静默升级由 SYSTEM 跑，
  ; 它启动的 x8im.exe 也是 SYSTEM 身份：任务栏里看不见主窗、普通权限和提权 taskkill
  ; 全都「拒绝访问」，只能重启电脑才能清掉（本机 9:13 那个 37492 就是这么来的，
  ; 还占着 WebView2 user data 目录，导致新版本反复起不来）。
  ; 显式 /RU "$USERNAME"（安装器提权后仍是同一个登录用户）+ /IT → 任务以「当前用户、
  ; 最高权限、仅登录时」运行，进程归属正常，安装器的 PREINSTALL taskkill 也杀得掉。
  ; ⚠️ 上面这条 /RU 用法已作废：裸用户名 schtasks 不认，见下方 2026-10-24 修正。
  ; 2026-10-24 修正（本机 1.5.43 事故）：原来 /RU 只传裸用户名 "$USERNAME"（=t8user），
  ; schtasks 报「帐户名与安全标识间无任何映射完成」→ 任务压根没建出来，
  ; 客户端 win_auto_update 查不到 "X8.IM AutoUpdate" → 静默升级永远失败。
  ; 用户侧表现就是「发版了，但客户端不提示、也不升级」。必须带域/计算机名前缀：
  ; 本地账户下 %USERDOMAIN% 就等于计算机名（本机 = trae11），拼成 trae11\t8user 才认。
  ; /ST 也从 00:00 改 23:59：/SC ONCE 下 00:00 早于当前时间会给一条无效警告，噪音扰排查。
  ExpandEnvStrings $R6 "%USERDOMAIN%\%USERNAME%"
  nsExec::ExecToStack 'schtasks /Create /TN "BangXin AutoUpdate" /TR "cmd /c C:\ProgramData\BangXin\updater-task.cmd" /SC ONCE /ST 23:59 /RL HIGHEST /IT /RU "$R6" /F'
  Pop $0
  Pop $R7
  FileOpen $8 "$TEMP\x8inst.log" a
  FileWrite $8 "step9_schtasks_exit=[$0] out=[$R7]$\r$\n"
  FileClose $8

  ; 9b) 2026-10-25 电池策略修正：schtasks /Create 用的是任务计划默认设置，其中
  ;     DisallowStartIfOnBatteries / StopIfGoingOnBatteries 默认 true →
  ;     **笔记本用电池供电时 schtasks /Run 直接被拒（0x41303「任务未运行」）**，
  ;     客户端侧表现为「静默升级失败/点了没反应」，而台式机永远测不出来。
  ;     schtasks /Change 不支持改这两项，故建完任务立刻用 PowerShell 精确改属性
  ;     （只动这两个开关，不碰 /IT /RL /RU 等既有设置）。
  ;     失败不阻断安装：仅记日志（老系统无 ScheduledTasks 模块时保持原样，
  ;     客户端侧仍有「失败→回退手动下载条」兜底）。
  nsExec::ExecToStack "powershell -NoProfile -ExecutionPolicy Bypass -Command $\"$$t=Get-ScheduledTask -TaskName 'BangXin AutoUpdate' -ErrorAction SilentlyContinue; if($$t){ $$t.Settings.DisallowStartIfOnBatteries=$$false; $$t.Settings.StopIfGoingOnBatteries=$$false; Set-ScheduledTask -TaskName 'BangXin AutoUpdate' -Settings $$t.Settings | Out-Null; 'battery-off' } else { 'task-missing' }$\""
  Pop $0
  Pop $R7
  FileOpen $8 "$TEMP\x8inst.log" a
  FileWrite $8 "step9b_battery_exit=[$0] out=[$R7]$\r$\n"
  FileClose $8

  ; 10) 2026-10-25 品牌迁移收口（X8.IM → 邦信）：把旧品牌遗留的安装身份清理掉，
  ;     否则一台机器上会同时存在旧目录 + 旧计划任务（旧任务脚本仍指向 C:\Program Files\X8.IM），
  ;     客户端查新任务名、旧任务却还能被外部触发 → 升级写错目录。
  ;     ⚠️ 只清旧品牌的安装足迹；**不动** %LOCALAPPDATA%\com.x8fde.im（WebView2 用户数据，
  ;     identifier 未变 → 里面是登录态，删了就要重新扫码）。
  nsExec::ExecToLog 'schtasks /Delete /TN "X8.IM AutoUpdate" /F'
  nsExec::ExecToLog 'cmd /c rmdir /s /q "C:\ProgramData\X8.IM"'
  nsExec::ExecToLog 'cmd /c rmdir /s /q "$PROFILE\AppData\Local\X8.IM"'
  ; 旧程序目录：仅当本次不是装到同一目录时才删（同目录升级时 $INSTDIR 就是它，不能删）
  StrCmp "$INSTDIR" "C:\Program Files\X8.IM" skip_legacy_dir
  nsExec::ExecToLog 'cmd /c rmdir /s /q "C:\Program Files\X8.IM"'
  skip_legacy_dir:
  ; 旧卸载项（老目录已被删，留着就是「程序和功能」里点不动的僵尸条目）
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\X8.IM"
  DeleteRegKey HKLM "Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\X8.IM"

  FileOpen $8 "$TEMP\x8inst.log" a
  FileWrite $8 "step10_legacy_cleaned$\r$\n"
  FileClose $8

  ; 11) 2026-10-25 品牌收口：快捷方式显示名统一为中文「邦信」，图标为橙底白「邦」字。
  ;     Tauri 默认模板用 productName（=BangXin，ASCII，供安装目录用）命名 .lnk，
  ;     老机器还残留 X8.IM 时代的旧快捷方式。这里删除旧英文/旧品牌快捷方式，
  ;     统一重建为「邦信.lnk」（开始菜单 + 桌面），图标取 exe 内嵌的「邦」字图标。
  ;     ⚠️ 仅改快捷方式显示名与图标，不动 $INSTDIR / 计划任务 / .cmd（那些必须 ASCII）。
  ;     删除：旧品牌 X8.IM.lnk + 新品牌英文 BangXin.lnk（开始菜单根目录 + 桌面）。
  Delete "$SMPROGRAMS\X8.IM.lnk"
  Delete "$SMPROGRAMS\BangXin.lnk"
  Delete "$DESKTOP\X8.IM.lnk"
  Delete "$DESKTOP\BangXin.lnk"
  ; Tauri 模板在 STARTMENUFOLDER 非空时会建到子文件夹，这里一并清掉
  Delete "$SMPROGRAMS\${PRODUCTNAME}\X8.IM.lnk"
  Delete "$SMPROGRAMS\${PRODUCTNAME}\BangXin.lnk"
  ; 重建中文「邦信」快捷方式（显示名=文件名=邦信）
  CreateShortcut "$SMPROGRAMS\邦信.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  CreateShortcut "$DESKTOP\邦信.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"

  ; 11b) 2026-10-26 根治「桌面出现两个快捷方式」：上面第 11 段删旧快捷方式只命中了
  ;     $DESKTOP 所在的桌面作用域（公共 / 用户其一），老机器上旧版 BangXin.lnk / X8.IM.lnk
  ;     若落在另一个作用域就删不掉，于是「邦信.lnk」与残留的「BangXin.lnk」并存于合并桌面。
  ;     这里用 PowerShell 按「目标 exe 路径」精确匹配，把指向本机 BangXin\x8im.exe 的
  ;     旧英文名快捷方式（BangXin.lnk / X8.IM.lnk）从用户+公共桌面及开始菜单全部删掉，
  ;     只保留「邦信.lnk」。全程 ASCII 传参，避开中文字面量在非 UTF-8 代码页下变乱码。
  nsExec::ExecToLog "powershell -NoProfile -ExecutionPolicy Bypass -Command $\"$$sh=New-Object -ComObject WScript.Shell; foreach($$d in @(($$env:PUBLIC + '\Desktop'), ($$env:USERPROFILE + '\Desktop'), ($$env:ProgramData + '\Microsoft\Windows\Start Menu\Programs'), ($$env:APPDATA + '\Microsoft\Windows\Start Menu\Programs'))){ Get-ChildItem -LiteralPath $$d -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object { $$s=$$sh.CreateShortcut($$.FullName); if(($$s.TargetPath -ieq '$INSTDIR\x8im.exe') -and (($$_.Name -eq 'BangXin.lnk') -or ($$_.Name -eq 'X8.IM.lnk'))){ Remove-Item -LiteralPath $$_.FullName -Force } } }$\""

  ; 11a) 2026-10-26 图标缓存根治（老8 报「桌面快捷方式还是 X8 图标」）：
  ;   实测结论：1.6.13 的 x8im.exe 内嵌图标已经是橙底白「邦」（从 PE 里 extract 出来
  ;   取 32/64/128/256 四档，md5 与仓库 icons/*.png 完全一致），图标源没问题。
  ;   显示成 X8 的原因是 **Windows 图标缓存按「目标 exe 路径」命中**：老机器的
  ;   C:\Program Files\BangXin\x8im.exe 这个路径下曾经是 X8 图标的 exe，覆盖后
  ;   Explorer 仍读 iconcache_*.db 里的旧图，直到缓存过期或手工重建。
  ;   两个动作一起做才稳：
  ;     ① 把快捷方式的 IconLocation 显式指向**独立 ico 文件**（$INSTDIR\icons\icon.ico，
  ;        由 tauri.conf.json resources 带入）—— 新路径 = 新缓存键，立即显示「邦」，
  ;        不依赖缓存失效时机；以后换图标只需换这个文件内容 + 版本号递增。
  ;     ② 顺手删掉当前用户的图标缓存并让 shell 重建（解决资源管理器/开始菜单里的旧图）。
  ;   注意：本上下文 $LOCALAPPDATA 会被解析成 C:\ProgramData（见第 6 条注释），
  ;   必须用 $PROFILE\AppData\Local 精确定位用户缓存。
  ;   ⚠️ 实现细节（踩过的坑）：脚本里的 .lnk 路径**不要写中文字面量**——nsExec 传给
  ;   PowerShell 的命令行在非 UTF-8 代码页下会把「邦信」传成乱码，CreateShortcut 直接失败。
  ;   改为在 PowerShell 内部用 $env:PUBLIC / $env:ProgramData 拼目录，再按「目标 exe」
  ;   匹配 .lnk（与 post-update.cmd 同一套逻辑），全程 ASCII 传参。
  IfFileExists "$INSTDIR\icons\icon.ico" 0 ico_fallback
  StrCpy $R5 "$INSTDIR\icons\icon.ico"
  Goto ico_apply
  ico_fallback:
  ; 独立 ico 万一没落地（resources 映射异常时的防御分支）：IconLocation 写实为 exe，
  ; 再靠下面的缓存清理兜底
  StrCpy $R5 "$INSTDIR\x8im.exe"
  ico_apply:
  nsExec::ExecToLog "powershell -NoProfile -ExecutionPolicy Bypass -Command $\"$$sh=New-Object -ComObject WScript.Shell; foreach($$d in @(($$env:PUBLIC + '\Desktop'), ($$env:ProgramData + '\Microsoft\Windows\Start Menu\Programs'))){ Get-ChildItem -LiteralPath $$d -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object { $$s=$$sh.CreateShortcut($$.FullName); if($$s.TargetPath -ieq '$INSTDIR\x8im.exe'){ $$s.IconLocation='$R5,0'; $$s.Save() } } }$\""
  nsExec::ExecToLog 'cmd /c del /f /q "$PROFILE\AppData\Local\IconCache.db"'
  nsExec::ExecToLog 'cmd /c del /f /q "$PROFILE\AppData\Local\Microsoft\Windows\Explorer\iconcache_*.db"'
  nsExec::ExecToLog '"$SYSDIR\ie4uinit.exe" -show'
  FileOpen $8 "$TEMP\x8inst.log" a
  FileWrite $8 "step11_shortcut_branded$\r$\n"
  FileWrite $8 "done$\r$\n"
  FileClose $8
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog 'taskkill /F /IM X8Assist.exe'
  nsExec::ExecToLog 'sc delete X8Assist'
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "X8Assist"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "X8Assist"
  RMDir /r "C:\Program Files\X8Assist"
  ; 2026-10-22：清理静默升级计划任务与任务脚本（先删任务再删脚本，避免卸载后
  ; 任务还能被触发）；顺带清掉用户级的更新暂存目录（%LOCALAPPDATA%\BangXin\updates）。
  ; 2026-10-25 品牌迁移：新旧两套名字都删，避免残留任务在卸载后仍可被触发。
  nsExec::ExecToLog 'schtasks /Delete /TN "BangXin AutoUpdate" /F'
  nsExec::ExecToLog 'schtasks /Delete /TN "X8.IM AutoUpdate" /F'
  ExpandEnvStrings $9 "%ProgramData%"
  RMDir /r "$9\BangXin"
  RMDir /r "$9\X8.IM"
  nsExec::ExecToLog 'cmd /c rmdir /s /q "$PROFILE\AppData\Local\BangXin"'
  nsExec::ExecToLog 'cmd /c rmdir /s /q "$PROFILE\AppData\Local\X8.IM"'
  ; 2026-11-07 修「升级后要重新扫码」：卸载钩子也不再删登录凭据。
  ;   %APPDATA%\Roaming\com.x8fde.im = app_data_dir()，里面是 hostAuth 的 auth.json；
  ;   %LOCALAPPDATA%\com.x8fde.im\EBWebView = WebView2 的 localStorage(x8_token)。
  ;   此处历史上两条 rmdir 会把两份凭据一起删掉，覆盖安装 / 卸载重装后必扫码。
  ;   与安装段(6a, 2026-11-04) 及 post-update.cmd(7) 的口径保持一致。
  ;   如确需清缓存，只可清 EBWebView\Default\Cache 子目录，不碰 Local Storage 与 app_data_dir。
!macroend
