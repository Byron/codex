@echo off
setlocal DisableDelayedExpansion
pwsh.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0codex-dev.ps1" %*
exit /b %errorlevel%
