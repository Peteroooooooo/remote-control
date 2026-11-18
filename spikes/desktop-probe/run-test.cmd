@echo off
rem Double-click to run the secure-desktop test. Results go to measurements-raw\uac-<time>.
"%~dp0target\release\desktop-probe.exe" selftest --out "%~dp0..\..\measurements-raw"
if errorlevel 1 pause
