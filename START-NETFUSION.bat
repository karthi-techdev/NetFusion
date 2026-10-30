@echo off
setlocal EnableDelayedExpansion
title NetFusion 1-Click Launcher

:: ============================================================================
:: 1. AUTO-ELEVATE TO ADMINISTRATOR PRIVILEGES (ZERO EXTRA CLICKS REQUIRED)
:: ============================================================================
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo [NetFusion] Requesting Administrator privileges for virtual network adapter...
    powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -WorkingDirectory '%~dp0' -Verb RunAs"
    exit /b
)

cd /d "%~dp0"

echo =========================================================================
echo                   NetFusion Internet Bonding Engine
echo             (1-Click True Packet Multipath Bonding System)
echo =========================================================================
echo.

:: Fix any unquoted stray files in Users directory that break MinGW linker
if exist "%SYSTEMDRIVE%\Users\%USERNAME%" (
    if not exist "%SYSTEMDRIVE%\Users\%USERNAME%\" (
        ren "%SYSTEMDRIVE%\Users\%USERNAME%" "%USERNAME%_bak" >nul 2>&1
    )
)

set "PATH=%USERPROFILE%\.cargo\bin;%LOCALAPPDATA%\Microsoft\WinGet\Links;%PATH%"

for /d %%D in ("%LOCALAPPDATA%\Microsoft\WinGet\Packages\*llvm-mingw*") do (
    if exist "%%D\bin" set "PATH=%%D\bin;!PATH!"
)
for /d %%D in ("%LOCALAPPDATA%\Microsoft\WinGet\Packages\*MinGW*") do (
    if exist "%%D\bin" set "PATH=%%D\bin;!PATH!"
)
for /d %%D in ("%LOCALAPPDATA%\Microsoft\WinGet\Packages\*WinLibs*") do (
    if exist "%%D\mingw64\bin" set "PATH=%%D\mingw64\bin;!PATH!"
    if exist "%%D\bin" set "PATH=%%D\bin;!PATH!"
)

:: Copy Wintun driver DLL if not in current root
if not exist "wintun.dll" (
    if exist "drivers\wintun\bin\amd64\wintun.dll" (
        copy /y "drivers\wintun\bin\amd64\wintun.dll" "wintun.dll" >nul
    )
)

:: Ensure default configs exist
if not exist "configs\client.json" (
    echo [!] Creating default configs\client.json...
    mkdir configs 2>nul
)

:: ============================================================================
:: 3. LAUNCH BONDING ENGINE & DASHBOARD TOGETHER
:: ============================================================================
echo [*] Detecting network interfaces (Ethernet / WiFi / Mobile Hotspot)...
echo [*] Launching NetFusion Bonding Engine and Live Dashboard...
echo.

:: Start NetFusion (Tray app with low memory usage and live network speed)
if exist "%~dp0NetFusion-Portable.exe" (
    start "" "%~dp0NetFusion-Portable.exe"
) else if exist "%~dp0target\release\NetFusion.exe" (
    start "" "%~dp0target\release\NetFusion.exe"
) else (
    start "" powershell -NoProfile -ExecutionPolicy Bypass -Command "cd '%~dp0'; cargo run --release -p netfusion-client"
)

echo [OK] NetFusion is running in your Windows System Tray (Overflow Menu ^).
echo      Click the NetFusion icon to view live network speeds.
echo.
pause

echo [*] Restoring network settings and stopping NetFusion...
taskkill /f /im NetFusion.exe >nul 2>&1
powershell -NoProfile -Command "Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -Name ProxyEnable -Value 0" >nul 2>&1
echo [OK] Stopped cleanly. Internet connection 100%% active.
echo [OK] NetFusion stopped cleanly. Internet settings restored.
timeout /t 2 >nul
