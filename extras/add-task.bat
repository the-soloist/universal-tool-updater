@ECHO OFF

echo Add auto updater custom task
echo read more in README.md
echo.
echo Execute in elevated command prompt!
echo -------------------------------------------------
echo.

cd /D "%~dp0"
SCHTASKS /CREATE /SC WEEKLY /TN "UniversalToolUpdater" /TR "%cd%\custom-task-loader.bat"
pause
