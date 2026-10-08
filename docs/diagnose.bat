@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

echo ============================================================
echo   bilibili-song-request - portable diagnostics
echo ============================================================
echo.

rem Find the diagnostic script next to this .bat, or in a few known spots.
set "DIAG=%~dp0diagnose.ps1"
if not exist "%DIAG%" set "DIAG=%~dp0docs\diagnose.ps1"
if not exist "%DIAG%" (
  echo [FAIL] diagnose.ps1 not found next to this file.
  echo        Put both files in the portable folder:
  echo          bilibili-song-request.exe
  echo          diagnose.bat
  echo          diagnose.ps1
  echo.
  goto :pause
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%DIAG%"

:pause
echo.
echo ============================================================
echo   Finished. Copy everything above and send it back.
echo ============================================================
pause
