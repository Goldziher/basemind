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
set "HOOK_SCRIPT=%~1"

REM Collect every argument after the script name, not just %2..%9 — `shift` past
REM the first token then accumulate the rest so hooks invoked with 9+ args don't
REM silently lose the tail.
set "HOOK_ARGS="
shift
:collect_args
if "%~1"=="" goto run_hook
set "HOOK_ARGS=%HOOK_ARGS% %1"
shift
goto collect_args

:run_hook
if exist "C:\Program Files\Git\bin\bash.exe" (
    "C:\Program Files\Git\bin\bash.exe" "%HOOK_DIR%%HOOK_SCRIPT%"%HOOK_ARGS%
    exit /b %ERRORLEVEL%
)
if exist "C:\Program Files (x86)\Git\bin\bash.exe" (
    "C:\Program Files (x86)\Git\bin\bash.exe" "%HOOK_DIR%%HOOK_SCRIPT%"%HOOK_ARGS%
    exit /b %ERRORLEVEL%
)
where bash >nul 2>nul
if %ERRORLEVEL% equ 0 (
    bash "%HOOK_DIR%%HOOK_SCRIPT%"%HOOK_ARGS%
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
