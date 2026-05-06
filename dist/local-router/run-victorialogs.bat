@echo off
rem Wrapper used by the "VictoriaLogs" Scheduled Task.
cd /d C:\victoria-logs
victoria-logs.exe -storageDataPath=C:\victoria-logs\data -httpListenAddr=:9428 > C:\victoria-logs\vl-stdout.log 2> C:\victoria-logs\vl-stderr.log
