@echo off
title NetFusion Dashboard
echo ===================================================
echo     Launching NetFusion Real-Time Dashboard
echo ===================================================
echo.

set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cargo run --release -p netfusion-gui
pause
