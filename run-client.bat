@echo off
title NetFusion Client Engine
echo ===================================================
echo     Starting NetFusion Internet Bonding Engine
echo ===================================================
echo.
echo Checking Administrator privileges...
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo [WARNING] Please run this script as Administrator to allow
    echo           Wintun virtual adapter creation and route management.
    echo.
)

set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cargo run --release -p netfusion-client -- --config configs\client.json
pause
