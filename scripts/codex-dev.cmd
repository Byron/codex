@echo off
setlocal DisableDelayedExpansion
if not defined CODEX_HOME set "CODEX_HOME=%LOCALAPPDATA%\CodexDev"
if not exist "%CODEX_HOME%" mkdir "%CODEX_HOME%"
if not exist "%CODEX_HOME%\" exit /b 1
"%~dp0bin\codex.exe" %*
exit /b %errorlevel%
