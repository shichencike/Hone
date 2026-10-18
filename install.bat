@echo off
rem ============================================================================
rem  Hone one-click installer for Windows (pure batch, ASCII only, no PowerShell)
rem  Usage: download install.bat and double-click, or run from a terminal:
rem     curl -O https://github.com/shichencike/Hone/releases/latest/download/install.bat
rem     install.bat
rem
rem  Features (parity with install.sh):
rem    - Mirror first + automatic fallback to the official source
rem      (ghproxy.net prefix proxy, HONE_MIRROR overridable)
rem    - Resume + automatic retry (curl -C - --retry)
rem    - sha256 verification via certutil before extraction
rem    - Idempotent: installed bundle hash matched = skip download
rem    - Staged extraction + move, a broken run never leaves half a binary
rem    - Adds the install dir to the user PATH (dedup, HONE_NO_PATH=1 to skip)
rem    - Uninstall support: set HONE_UNINSTALL=1
rem
rem  Optional environment variables:
rem    HONE_VERSION     version tag to install (default: latest)
rem    HONE_MIRROR      mirror prefix (default: https://ghproxy.net/); off = official only
rem    HONE_PREFIX      install directory (default: %LOCALAPPDATA%\hone)
rem    HONE_UNINSTALL   1 = uninstall
rem    HONE_NO_PATH     1 = do not touch PATH
rem
rem  Requires: Windows 10 1803+ (curl.exe, tar.exe) - both ship with the OS.
rem ============================================================================

setlocal EnableExtensions EnableDelayedExpansion

set "REPO=shichencike/Hone"
set "ASSET=hone-windows-x86_64-full.zip"
if not defined HONE_VERSION set "HONE_VERSION=latest"
if not defined HONE_MIRROR set "HONE_MIRROR=https://ghproxy.net/"
if not defined HONE_PREFIX set "HONE_PREFIX=%LOCALAPPDATA%\hone"
set "HONE_HOME=%HONE_PREFIX%"

if "%HONE_VERSION%"=="latest" (
    set "BASE_URL=https://github.com/%REPO%/releases/latest/download"
) else (
    set "BASE_URL=https://github.com/%REPO%/releases/download/%HONE_VERSION%"
)
rem Mirror = prefix proxy + full official path (same scheme as install.sh)
set "MIRROR=%HONE_MIRROR%"
if "!MIRROR:~-1!"=="/" set "MIRROR=!MIRROR:~0,-1!"
set "MIRROR_BASE=!MIRROR!%BASE_URL%"

echo Installing Hone...
echo.

rem ---------- 0. prerequisite checks (tools shipped with Windows 10/11) ----------
where curl.exe >nul 2>nul
if errorlevel 1 (
    echo [ERROR] curl.exe not found. It ships with Windows 10 1803+.
    echo         Update Windows, or download the zip manually from:
    echo         %BASE_URL%/%ASSET%
    exit /b 1
)
where tar.exe >nul 2>nul
if errorlevel 1 (
    echo [ERROR] tar.exe not found. It ships with Windows 10+.
    echo         Update Windows, or extract the zip manually to: %HONE_HOME%
    exit /b 1
)
where certutil.exe >nul 2>nul
if errorlevel 1 (
    echo [ERROR] certutil.exe not found. It ships with every Windows install.
    exit /b 1
)

rem ---------- uninstall mode ----------
if not "%HONE_UNINSTALL%"=="1" goto :no_uninstall
    call :path_remove
    if exist "%HONE_HOME%\hone.exe" del /f /q "%HONE_HOME%\hone.exe" >nul 2>nul
    if exist "%HONE_HOME%\.installed_sha256" del /f /q "%HONE_HOME%\.installed_sha256" >nul 2>nul
    echo Uninstalled: %HONE_HOME%\hone.exe and its PATH entry.
    exit /b 0
:no_uninstall

set "WORK=%TEMP%\hone-install-%RANDOM%"
set "ZIP=%WORK%\%ASSET%"
mkdir "%WORK%" >nul 2>nul || (
    echo [ERROR] Cannot create temp dir %WORK%
    exit /b 1
)
mkdir "%HONE_HOME%" >nul 2>nul

echo ==^> Fetching checksums.txt (v%HONE_VERSION%)
call :fetch checksums.txt "%WORK%\checksums.txt"
if errorlevel 1 (
    echo [ERROR] Downloading checksums.txt failed. Check your network and retry.
    goto :fail
)
set "EXPECTED="
for /f "usebackq tokens=1,2" %%A in ("%WORK%\checksums.txt") do (
    if /i "%%B"=="%ASSET%" set "EXPECTED=%%A"
)
if not defined EXPECTED (
    echo [ERROR] %ASSET% not listed in checksums.txt. Release assets may be incomplete.
    goto :fail
)

rem ---------- idempotency: same bundle already installed = skip download ----------
if exist "%HONE_HOME%\.installed_sha256" (
    set "INSTALLED="
    set /p INSTALLED=<"%HONE_HOME%\.installed_sha256"
    if /i "!INSTALLED!"=="%EXPECTED%" (
        echo ==^> Same version already installed (sha256 match), skipping download.
        goto :path_step
    )
)

echo ==^> Downloading %ASSET% ...
call :fetch "%ASSET%" "%ZIP%"
if errorlevel 1 (
    echo [ERROR] Download failed. Check your network connection and try again.
    goto :fail
)

rem ---------- sha256 verification ----------
set "ZIP_SHA="
for /f "skip=1 tokens=1" %%H in ('certutil.exe -hashfile "%ZIP%" SHA256 2^>nul') do (
    if not defined ZIP_SHA set "ZIP_SHA=%%H"
)
if not defined ZIP_SHA (
    echo [ERROR] Could not compute sha256 of the downloaded file.
    goto :fail
)
if /i not "%ZIP_SHA%"=="%EXPECTED%" (
    echo [ERROR] sha256 mismatch: expected %EXPECTED%, got %ZIP_SHA%
    goto :fail
)
echo     sha256 OK: %ZIP_SHA%

rem ---------- staged extraction + atomic move ----------
echo ==^> Extracting to %HONE_HOME% ...
set "STAGING=%WORK%\unpacked"
mkdir "%STAGING%" >nul 2>nul
tar.exe -xf "%ZIP%" -C "%STAGING%"
if errorlevel 1 (
    echo [ERROR] Extraction failed. The downloaded file may be corrupted.
    goto :fail
)
set "HONE_SRC="
for /r "%STAGING%" %%F in (hone.exe) do (
    if exist "%%F" set "HONE_SRC=%%~fF"
)
if not defined HONE_SRC (
    echo [ERROR] hone.exe not found inside the archive. Layout may have changed.
    goto :fail
)
move /y "%HONE_SRC%" "%HONE_HOME%\hone.exe" >nul
if errorlevel 1 (
    echo [ERROR] Cannot replace %HONE_HOME%\hone.exe.
    echo         Close any running Hone processes and run this script again.
    goto :fail
)
rem Full bundle may also carry runtime data files beside hone.exe; copy the rest.
for /r "%STAGING%" %%F in (*.*) do (
    if /i not "%%~nxF"=="hone.exe" (
        if not exist "%HONE_HOME%\%%~nxF" copy /y "%%F" "%HONE_HOME%\" >nul 2>nul
    )
)
rem Record the installed bundle hash for idempotent reruns.
> "%HONE_HOME%\.installed_sha256" echo %EXPECTED%
del /f /q "%ZIP%" >nul 2>nul

:path_step
rem ---------- add to user PATH (skip if already present) ----------
if "%HONE_NO_PATH%"=="1" goto :verify
echo ==^> Adding %HONE_HOME% to user PATH ...
call :path_add

:verify
echo ==^> Verifying ...
"%HONE_HOME%\hone.exe" --version
if errorlevel 1 (
    echo [WARN] hone.exe did not report a version. Installation may be incomplete.
) else (
    echo     OK.
)

echo.
echo ============================================================
echo  Hone installed successfully!
echo   Install dir : %HONE_HOME%
echo.
echo  IMPORTANT: restart your terminal (or VS Code) so the
echo  updated PATH takes effect, then run:
echo     hone --version
echo ============================================================
rd /s /q "%WORK%" >nul 2>nul
exit /b 0

:fail
rd /s /q "%WORK%" >nul 2>nul
exit /b 1

rem ----------------------------------------------------------------------------
rem Subroutines
rem ----------------------------------------------------------------------------

rem :dl <url> <outfile> - single download with resume + retry
:dl
curl.exe -fL --retry 3 --retry-delay 2 --connect-timeout 15 -C - -o "%~2" "%~1"
exit /b %errorlevel%

rem :fetch <remote-file> <outfile> - mirror first, fallback to official
:fetch
if /i "%HONE_MIRROR%"=="off" (
    call :dl "%BASE_URL%/%~1" "%~2"
    exit /b %errorlevel%
)
call :dl "%MIRROR_BASE%/%~1" "%~2"
if not errorlevel 1 (
    echo     [mirror] %MIRROR_BASE%/%~1
    exit /b 0
)
echo     [mirror failed, falling back to official] %BASE_URL%/%~1
del /f /q "%~2" >nul 2>nul
call :dl "%BASE_URL%/%~1" "%~2"
exit /b %errorlevel%

rem :path_add - append HONE_HOME to the user PATH unless already present
:path_add
set "USER_PATH="
for /f "usebackq skip=2 tokens=1,2*" %%A in (`reg.exe query "HKCU\Environment" /v Path 2^>nul`) do (
    if /i "%%A"=="Path" set "USER_PATH=%%C"
)
if not defined USER_PATH (
    reg.exe add "HKCU\Environment" /v Path /t REG_EXPAND_SZ /d "%HONE_HOME%" /f >nul
    setx PATH "%HONE_HOME%" >nul
    echo     PATH updated.
    exit /b 0
)
echo %USER_PATH% | findstr /i /c:"%HONE_HOME%" >nul
if errorlevel 1 (
    set "NEW_PATH=%USER_PATH%;%HONE_HOME%"
    reg.exe add "HKCU\Environment" /v Path /t REG_EXPAND_SZ /d "!NEW_PATH!" /f >nul
    setx PATH "!NEW_PATH!" >nul
    echo     PATH updated.
) else (
    echo     Already in PATH, skipped.
)
exit /b 0

rem :path_remove - rebuild the user PATH without the Hone entry
:path_remove
set "USER_PATH="
for /f "usebackq skip=2 tokens=1,2*" %%A in (`reg.exe query "HKCU\Environment" /v Path 2^>nul`) do (
    if /i "%%A"=="Path" set "USER_PATH=%%C"
)
if not defined USER_PATH exit /b 0
set "NEW_PATH="
for %%E in ("!USER_PATH:;=" "!") do (
    if /i not "%%~E"=="%HONE_HOME%" if not "%%~E"=="" (
        if defined NEW_PATH (set "NEW_PATH=!NEW_PATH!;%%~E") else (set "NEW_PATH=%%~E")
    )
)
if defined NEW_PATH (
    reg.exe add "HKCU\Environment" /v Path /t REG_EXPAND_SZ /d "!NEW_PATH!" /f >nul
    setx PATH "!NEW_PATH!" >nul
)
exit /b 0
