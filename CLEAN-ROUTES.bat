@echo off
title NetFusion Network Route Cleaner
net session >nul 2>&1
if %errorLevel% neq 0 (
    powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)
echo =========================================================================
echo                  NetFusion Windows Routing Cleaner
echo =========================================================================
echo.
echo [*] Purging all conflicting /8 static routes from Windows kernel...
for /L %%i in (1,1,223) do (
    route delete %%i.0.0.0 mask 255.0.0.0 >nul 2>&1
)
echo [*] Restoring default Windows proxy settings (direct internet)...
powershell -NoProfile -Command "Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -Name ProxyEnable -Value 0" >nul 2>&1
echo.
echo [SUCCESS] Windows routing table cleaned! 100%% direct internet restored.
echo.
pause

