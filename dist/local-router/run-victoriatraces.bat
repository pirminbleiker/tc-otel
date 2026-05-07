@echo off
rem Wrapper used by the "VictoriaTraces" Scheduled Task.
cd /d C:\victoria-traces
victoria-traces.exe -storageDataPath=C:\victoria-traces\data -httpListenAddr=:10428 > C:\victoria-traces\vt-stdout.log 2> C:\victoria-traces\vt-stderr.log
