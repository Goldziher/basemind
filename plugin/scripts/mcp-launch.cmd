#!/usr/bin/env bash
: << 'CMDBLOCK'
@echo off
REM Polyglot MCP entry point for every plugin manifest.
REM   Windows: cmd.exe runs this section, finds Git Bash and runs mcp-launch.sh through it, since
REM            Windows cannot start a shell script itself.
REM   Unix:    the shebang runs bash, the leading `:` heredoc skips this section, and the shell
REM            section below runs mcp-launch.sh.
REM cmd.exe echoes the shebang line before `@echo off` takes effect; MCP clients skip a stdout line
REM that is not JSON-RPC.
REM Usage: mcp-launch.cmd [basemind args...]

setlocal
set "GIT_BASH="
if defined CLAUDE_CODE_GIT_BASH_PATH if exist "%CLAUDE_CODE_GIT_BASH_PATH%" set "GIT_BASH=%CLAUDE_CODE_GIT_BASH_PATH%"
if not defined GIT_BASH if exist "%ProgramFiles%\Git\bin\bash.exe" set "GIT_BASH=%ProgramFiles%\Git\bin\bash.exe"
if not defined GIT_BASH if exist "%ProgramFiles(x86)%\Git\bin\bash.exe" set "GIT_BASH=%ProgramFiles(x86)%\Git\bin\bash.exe"
if not defined GIT_BASH if exist "%LOCALAPPDATA%\Programs\Git\bin\bash.exe" set "GIT_BASH=%LOCALAPPDATA%\Programs\Git\bin\bash.exe"
REM A bash on PATH counts unless it is the WSL launcher, which runs Linux and cannot read this
REM plugin's Windows paths.
for /f "delims=" %%B in ('where bash 2^>nul') do if not defined GIT_BASH if /i not "%%~dpB"=="%SystemRoot%\System32\" if /i not "%%~dpB"=="%LOCALAPPDATA%\Microsoft\WindowsApps\" set "GIT_BASH=%%B"
if not defined GIT_BASH (
    >&2 echo basemind-launch: error: the basemind MCP server needs bash from Git for Windows, and none was found. Install Git for Windows from https://git-scm.com/download/win, or set CLAUDE_CODE_GIT_BASH_PATH to its bash.exe, then restart the session.
    exit /b 1
)
"%GIT_BASH%" "%~dp0mcp-launch.sh" %*
exit /b %ERRORLEVEL%
CMDBLOCK

# Unix: hand every argument to the launcher beside this file. ~keep
exec bash "$(cd "$(dirname "$0")" && pwd)/mcp-launch.sh" "$@"
