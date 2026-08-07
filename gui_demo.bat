@echo off
rem Zap GUI demo launcher - double-click to start
rem Uses the local zap_lib/gui.zp + server.* builtins (v0.3.0+)
cd /d D:\shichencike\Desktop\zap
echo Starting Zap GUI demo... browser will open automatically.
echo Press Ctrl+C to exit.
bin\zap-windows-x86_64.exe examples\gui_demo.zp
pause