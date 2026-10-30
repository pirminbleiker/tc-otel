"""
Full re-deploy after the protobuf-metrics rebuild:
1. Stop tc-otel + Victoria-{Logs,Metrics,Traces} on the target.
2. Wipe and re-create C:\\victoria-{logs,metrics,traces}\\data so the
   protobuf test starts from a clean ingest.
3. Upload the freshly-built tc-otel.exe + dist/local-router/config.json
   (which now sets metrics.export_format = "protobuf").
4. Restart all four Scheduled Tasks.
5. Verify each pillar:
   - Logs   → POST /select/logsql/query?query=*&start=2m
   - Metrics → /api/v1/series?match[]={__name__=~".+"} (this fails in
     JSON mode, succeeds with protobuf)
   - Traces → /select/jaeger/api/services

Usage:

    py redeploy_protobuf.py --host 192.0.2.10 [--user Administrator] [--password XXX]

Every option falls back to a TC_OTEL_* environment variable (CLI > env >
default). Without a password, paramiko uses key / agent authentication.
Requires `paramiko` (`py -m pip install paramiko`).
"""
from __future__ import annotations
import argparse
import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

REPO_ROOT = Path(__file__).resolve().parents[1]


def parse_args():
    p = argparse.ArgumentParser(
        description="Full re-deploy of tc-otel + Victoria backends to a Windows target.")
    p.add_argument("--host", default=os.environ.get("TC_OTEL_HOST"),
                   help="Target IP or hostname (env TC_OTEL_HOST)")
    p.add_argument("--user", default=os.environ.get("TC_OTEL_USER", "Administrator"),
                   help="SSH user (env TC_OTEL_USER, default Administrator)")
    p.add_argument("--password", default=os.environ.get("TC_OTEL_PASSWORD"),
                   help="SSH password (env TC_OTEL_PASSWORD; omit to use key-based auth)")
    p.add_argument("--exe",
                   default=os.environ.get(
                       "TC_OTEL_EXE", str(REPO_ROOT / "target" / "release" / "tc-otel.exe")),
                   help="Local tc-otel.exe to upload (env TC_OTEL_EXE, "
                        "default <repo>/target/release/tc-otel.exe)")
    p.add_argument("--dist-dir",
                   default=os.environ.get(
                       "TC_OTEL_DIST_DIR", str(REPO_ROOT / "dist" / "local-router")),
                   help="Local dir holding config.json (env TC_OTEL_DIST_DIR, "
                        "default <repo>/dist/local-router)")
    args = p.parse_args()
    if not args.host:
        p.error("--host or TC_OTEL_HOST is required")
    return args


def ssh_run(c, cmd, timeout=30):
    chan = c.get_transport().open_session()
    chan.set_combine_stderr(True)
    chan.exec_command(cmd)
    chan.settimeout(timeout)
    out = b""
    while True:
        try:
            chunk = chan.recv(8192)
            if not chunk:
                if chan.exit_status_ready():
                    break
                time.sleep(0.1); continue
            out += chunk
        except Exception:
            break
    chan.close()
    return out.decode("utf-8", errors="replace")


def main():
    args = parse_args()
    host = args.host

    import paramiko

    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(host, username=args.user, password=args.password, timeout=10)

    print(">>> Stopping all four Scheduled Tasks ...")
    for t in ("tc-otel", "VictoriaLogs", "VictoriaMetrics", "VictoriaTraces", "otelcol"):
        ssh_run(c, f'schtasks /End /TN {t} 2>nul')
    for p in ("tc-otel.exe", "victoria-logs.exe", "victoria-metrics.exe",
              "victoria-traces.exe", "otelcol-contrib.exe"):
        ssh_run(c, f"taskkill /F /IM {p} 2>nul")
    time.sleep(2)

    print(">>> Wiping Victoria data dirs (fresh ingest from this point) ...")
    for d in ("C:\\victoria-logs\\data", "C:\\victoria-metrics\\data",
              "C:\\victoria-traces\\data"):
        ssh_run(c, f'rmdir /S /Q "{d}" 2>nul')
        ssh_run(c, f'mkdir "{d}" 2>nul')

    print(">>> Uploading new tc-otel.exe + config.json ...")
    sftp = c.open_sftp()
    sftp.put(args.exe, "/C:/tc-otel/tc-otel.exe")
    sftp.put(os.path.join(args.dist_dir, "config.json"), "/C:/tc-otel/config.json")
    sftp.close()

    # Show config so we know what's live
    print(">>> Active config on target:")
    print(ssh_run(c, "type C:\\tc-otel\\config.json"))

    print(">>> Starting Victoria backends ...")
    for t in ("VictoriaLogs", "VictoriaMetrics", "VictoriaTraces"):
        ssh_run(c, f'schtasks /Run /TN {t}')
    time.sleep(4)
    print(ssh_run(c, 'netstat -an | findstr ":9428 :8428 :10428"'))

    print(">>> Starting tc-otel ...")
    ssh_run(c, 'schtasks /Run /TN tc-otel')
    time.sleep(6)

    print(">>> tc-otel stderr (last 20 lines):")
    log = ssh_run(c, "type C:\\tc-otel\\tcotel-stderr.log 2>nul")
    log = re.sub(r"\x1b\[[0-9;]*m", "", log)
    for line in log.splitlines()[-25:]:
        if line.strip():
            print(f"   {line}")

    print()
    print(">>> Waiting 12s for first batch flush from PLC ...")
    time.sleep(12)

    print(">>> [Logs] VL count, last 2 min:")
    try:
        req = urllib.request.Request(
            f"http://{host}:9428/select/logsql/query",
            data=b"query=*&start=2m", method="POST",
        )
        body = urllib.request.urlopen(req, timeout=8).read().decode()
        n = len([l for l in body.splitlines() if l.strip()])
        print(f"   ingested log lines: {n}")
        if n:
            print(f"   sample: {body.splitlines()[0][:200]}")
    except Exception as e:
        print(f"   {e}")

    print(">>> [Metrics] VM series count:")
    try:
        url = (f"http://{host}:8428/api/v1/series?match%5B%5D="
               + urllib.parse.quote('{__name__=~".+"}'))
        body = urllib.request.urlopen(url, timeout=8).read().decode()
        data = json.loads(body)
        series = data.get("data", [])
        print(f"   total series: {len(series)}")
        for s in series[:8]:
            print(f"      {s}")
    except Exception as e:
        print(f"   {e}")

    print(">>> [Traces] VT services + traces:")
    try:
        body = urllib.request.urlopen(
            f"http://{host}:10428/select/jaeger/api/services",
            timeout=5).read().decode()
        print(f"   services: {body}")
        d = json.loads(body)
        for svc in d.get("data", []):
            traces = urllib.request.urlopen(
                f"http://{host}:10428/select/jaeger/api/traces?service={svc}&limit=5",
                timeout=5).read()
            td = json.loads(traces)
            print(f"   {svc}: {len(td.get('data', []))} traces")
    except Exception as e:
        print(f"   {e}")

    print()
    print(f"   VL UI:    http://{host}:9428/select/vmui/")
    print(f"   VM VMUI:  http://{host}:8428/vmui/")
    print(f"   VT API:   http://{host}:10428/select/jaeger/api/services")

    c.close()


if __name__ == "__main__":
    main()
