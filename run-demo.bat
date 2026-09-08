@echo off
REM ============================================================================
REM  Sovereign AI Workbench - one-click demo launcher (Windows)
REM
REM  Double-click this file to start the app. It exists because launching by
REM  hand needs a PATH that a fresh terminal does not always have: `winget`
REM  installs tools onto the MACHINE path, which an already-open shell cannot
REM  see, and the app shells out to `ollama` by name.
REM
REM  Leave this window open while you demo - closing it stops the app.
REM ============================================================================

title Sovereign AI Workbench - demo
cd /d "%~dp0"

echo.
echo  Sovereign AI Workbench
echo  ----------------------
echo.

REM --- Put the toolchain on PATH for this window only -------------------------
set "PATH=%USERPROFILE%\.cargo\bin;%LOCALAPPDATA%\Programs\Ollama;C:\Program Files\CMake\bin;%PATH%"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"

REM --- Ollama must be up; the app talks to it over HTTP ------------------------
echo  [1/3] Checking Ollama...
curl -s -o nul --max-time 3 http://127.0.0.1:11434/api/tags
if errorlevel 1 (
    echo        not running - starting it in the background...
    start "" /min ollama serve
    REM Give the server a moment to bind its port before the app probes it.
    timeout /t 4 /nobreak >nul
) else (
    echo        already running.
)

REM --- Frontend deps ----------------------------------------------------------
echo  [2/3] Checking frontend dependencies...
if not exist "node_modules\" (
    echo        installing (first run only, a few minutes^)...
    call npm install
    if errorlevel 1 goto :failed
) else (
    echo        present.
)

REM --- Launch -----------------------------------------------------------------
echo  [3/3] Starting the app...
echo.
echo  NOTE: the very first launch compiles the Rust backend and can take
echo        45-60 minutes. Later launches take seconds. The window opens
echo        on its own when the build finishes.
echo.

call npm run tauri dev
if errorlevel 1 goto :failed

echo.
echo  App closed.
pause
exit /b 0

:failed
echo.
echo  ---------------------------------------------------------------
echo   Something went wrong. Most common causes:
echo     - Rust / CMake / LLVM not installed (see README Prerequisites)
echo     - not enough free disk (the build needs ~15 GB)
echo   The app's own logs are under:
echo     %APPDATA%\com.anubhav_anand.tauri-app\logs
echo  ---------------------------------------------------------------
echo.
pause
exit /b 1
