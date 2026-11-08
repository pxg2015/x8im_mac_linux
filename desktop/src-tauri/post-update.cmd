@echo off
setlocal EnableExtensions
rem BangXin post-update steps (run by the silent-update scheduled task, highest privileges).
rem Refreshes the bundled X8Assist component and clears WebView2 cache after an in-place
rem component patch, so the patched app launches cleanly. Mirrors installer.nsh POSTINSTALL.
set INSTDIR=C:\Program Files\BangXin
set XA=C:\Program Files\X8Assist

rem 1) Refresh X8Assist files (idempotent copy)
robocopy "%INSTDIR%\bin\X8Assist" "%XA%" /E /NFL /NDL /NJH /NJS /NP

rem 2) Ensure per-machine password (reuse existing, else generate 12-char)
set PW=
if exist "%XA%\x8pwd.txt" (
  set /p PW=<"%XA%\x8pwd.txt"
)
if not defined PW (
  for /f "delims=" %%p in ('powershell -NoProfile -ExecutionPolicy Bypass -Command "$p=-join((48..57)+(65..90)+(97..122)|Get-Random -Count 12|ForEach-Object{[char]$_});[IO.File]::WriteAllText('"%XA%\x8pwd.txt"', $p);[Console]::Out.Write($p)"') do set PW=%%p
)

rem 3) Start server, write password, then stop the temp instance
start "" "%XA%\X8Assist.exe" --server
timeout /t 5 >nul
"%XA%\X8Assist.exe" --password %PW%
taskkill /F /IM X8Assist.exe >nul 2>&1
sc stop X8Assist >nul 2>&1
sc delete X8Assist >nul 2>&1
taskkill /F /IM X8Assist.exe >nul 2>&1

rem 4) Remove legacy per-user copies
rd /s /q "%USERPROFILE%\AppData\Local\x8assist" >nul 2>&1
rd /s /q "%USERPROFILE%\AppData\Local\X8Assist" >nul 2>&1

rem 5) Disable RDP session share (no "select session" prompt)
reg add HKLM\Software\X8Assist\InstallState\X8Assist /v share_rdp /t REG_SZ /d false /f >nul 2>&1
reg add HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\{54E86BC2-6C85-41F3-A9EB-1A94AC9B1F93}_is1 /v share_rdp /t REG_SZ /d false /f >nul 2>&1
reg add HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{54E86BC2-6C85-41F3-A9EB-1A94AC9B1F93}_is1 /v share_rdp /t REG_SZ /d false /f >nul 2>&1
reg add HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\X8Assist /v share_rdp /t REG_SZ /d false /f >nul 2>&1

rem 6) Disable autorun (on-demand only)
reg delete HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v X8Assist /f >nul 2>&1
reg delete HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v X8Assist /f >nul 2>&1

rem 7) 2026-11-04 起：不再清理 WebView2 缓存目录。
rem     历史上清缓存会让 Chromium 判定 profile 损坏而重建，连 Local Storage 里的 x8_token
rem     一起清掉 → 升级后必重新扫码。现 index.html 为 no-cache、JS 文件名带 hash、?v= 每
rem     次发版变化，旧缓存不会命中，无需清理即可避免白屏，且登录态得以保留。

rem 8) Sync "Programs and Features" DisplayVersion with the applied build (2026-10-26).
rem    Patch updates replace files in place and never touch the uninstall key, so the
rem    control panel kept showing the version of the ORIGINAL install (e.g. 1.6.7 after
rem    a 1.6.8 patch). Read the version straight from the patched x8im.exe: it carries
rem    ProductVersion = tauri.conf.json version, i.e. the build we just applied.
rem    (goto is used to break out of the for /f: inside a for body, %VAR% is expanded
rem    once for the whole block, so "if not defined" would not stop on the first line.)
set NEWVER=
for /f "delims=" %%v in ('powershell -NoProfile -ExecutionPolicy Bypass -Command "(Get-Item -LiteralPath '%INSTDIR%\x8im.exe').VersionInfo.ProductVersion" 2^>nul') do (
  set NEWVER=%%v
  goto X8GOTVER
)
:X8GOTVER
if defined NEWVER (
  for %%k in (BangXin X8.IM) do (
    reg query "HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\%%~k" >nul 2>&1 && reg add "HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\%%~k" /v DisplayVersion /t REG_SZ /d %NEWVER% /f >nul 2>&1
    reg query "HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\%%~k" >nul 2>&1 && reg add "HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\%%~k" /v DisplayVersion /t REG_SZ /d %NEWVER% /f >nul 2>&1
  )
)

rem 9) Shortcut icon refresh (2026-10-26). A component patch replaces x8im.exe in place,
rem    and Windows keys its icon cache by that exe path, so the desktop / start-menu
rem    shortcut keeps rendering the OLD icon (X8) even though the new exe embeds the
rem    current one. Point every BangXin shortcut at the standalone ico shipped in
rem    %INSTDIR%\icons\icon.ico (a different cache key -> correct icon immediately),
rem    then drop the per-user icon cache so Explorer rebuilds it.
set ICO=%INSTDIR%\icons\icon.ico
if not exist "%ICO%" set ICO=%INSTDIR%\x8im.exe
powershell -NoProfile -ExecutionPolicy Bypass -Command "$sh=New-Object -ComObject WScript.Shell; foreach($d in @('%PUBLIC%\Desktop','%ProgramData%\Microsoft\Windows\Start Menu\Programs','%USERPROFILE%\Desktop')){ Get-ChildItem -LiteralPath $d -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object { $s=$sh.CreateShortcut($_.FullName); if($s.TargetPath -ieq '%INSTDIR%\x8im.exe'){ $s.IconLocation='%ICO%,0'; $s.Save() } } }" >nul 2>&1
del /f /q "%USERPROFILE%\AppData\Local\IconCache.db" >nul 2>&1
del /f /q "%USERPROFILE%\AppData\Local\Microsoft\Windows\Explorer\iconcache_*.db" >nul 2>&1
ie4uinit.exe -show >nul 2>&1

rem 9b) 2026-10-26 根治「桌面出现两个快捷方式」：旧版 BangXin.lnk / X8.IM.lnk 可能残留在
rem     用户或公共桌面 / 开始菜单的任一作用域。按目标 exe 路径精确匹配，删掉指向本机
rem     BangXin\x8im.exe 的旧英文名快捷方式，只保留「邦信.lnk」。下次静默补丁升级即自动清理。
powershell -NoProfile -ExecutionPolicy Bypass -Command "$sh=New-Object -ComObject WScript.Shell; foreach($d in @('%PUBLIC%\Desktop','%USERPROFILE%\Desktop','%ProgramData%\Microsoft\Windows\Start Menu\Programs','%APPDATA%\Microsoft\Windows\Start Menu\Programs')){ Get-ChildItem -LiteralPath $d -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object { $s=$sh.CreateShortcut($_.FullName); if(($s.TargetPath -ieq '%INSTDIR%\x8im.exe') -and (($_.Name -eq 'BangXin.lnk') -or ($_.Name -eq 'X8.IM.lnk'))){ Remove-Item -LiteralPath $_.FullName -Force } } }" >nul 2>&1
:SKIPICON

endlocal
exit /b 0
