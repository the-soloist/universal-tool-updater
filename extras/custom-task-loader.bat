@echo off

cd /D "%~dp0"

set "UPDATER=%~dp0updater.exe"
if not exist "%UPDATER%" set "UPDATER=%~dp0..\updater.exe"

echo Executing Universal Tool Updater
"%UPDATER%" --profiles "%~dp0..\profiles" update > last-run.log 2>&1
