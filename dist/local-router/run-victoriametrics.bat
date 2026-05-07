@echo off
rem Wrapper used by the "VictoriaMetrics" Scheduled Task.
cd /d C:\victoria-metrics
victoria-metrics.exe -storageDataPath=C:\victoria-metrics\data -httpListenAddr=:8428 > C:\victoria-metrics\vm-stdout.log 2> C:\victoria-metrics\vm-stderr.log
