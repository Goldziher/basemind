: << 'CMDBLOCK'
@echo off
REM Cross-platform polyglot wrapper for basemind hook scripts.
REM   Windows: cmd.exe runs this batch section, locates bash, and calls the hook.
REM   Unix:    the leading `:` is a bash no-op, so execution falls through to the
REM            shell section below.
REM Hook scripts are extensionless (e.g. "session-start") so Claude Code's Windows
REM auto-detection -- which prepends "bash" to any command containing .sh -- does
REM not interfere.
REM Usage: run-hook.cmd <script-name> [args...]

if "%~1"=="" (
    echo run-hook.cmd: missing script name >&2
    exit /b 1
)

set "HOOK_DIR=%~dp0"

if exist "C:\Program Files\Git\bin\bash.exe" (
    "C:\Program Files\Git\bin\bash.exe" "%HOOK_DIR%%~1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)
if exist "C:\Program Files (x86)\Git\bin\bash.exe" (
    "C:\Program Files (x86)\Git\bin\bash.exe" "%HOOK_DIR%%~1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)
where bash >nul 2>nul
if %ERRORLEVEL% equ 0 (
    bash "%HOOK_DIR%%~1" %2 %3 %4 %5 %6 %7 %8 %9
    exit /b %ERRORLEVEL%
)

REM No bash found: exit silently. The plugin still works; only the SessionStart
REM pre-warm + status-line nudge are skipped.
exit /b 0
CMDBLOCK

# Unix: run the named hook script directly.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRIPT_NAME="$1"
shift
exec bash "${SCRIPT_DIR}/${SCRIPT_NAME}" "$@"
