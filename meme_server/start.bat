@echo off
rem Start the meme-generator HTTP service for qqbot (default http://127.0.0.1:3233).
rem Port 2233 (upstream default) is inside a Windows reserved port range on this machine, so 3233 is used.
rem Keep this file ASCII-only: cmd.exe reads .bat files in the ANSI code page (GBK), UTF-8 text breaks parsing.
rem First-time setup (venv, image templates, fonts): see the meme-generator section of README.md.
cd /d %~dp0
set PYTHONIOENCODING=utf-8
if "%MEME_PORT%"=="" set MEME_PORT=3233
".venv\Scripts\python.exe" serve.py
