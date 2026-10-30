"""
Optional: deploy this dist to a TwinCAT target over SSH from a developer
machine. Run as:

    py remote_deploy.py --host 192.0.2.10 --user Administrator [--password XXX]

Host, user and password also read TC_OTEL_HOST, TC_OTEL_USER and
TC_OTEL_PASSWORD (CLI > env > default).

Requires `paramiko` (`py -m pip install paramiko`). The target needs
OpenSSH server enabled (Windows Optional Feature). The script:

  1. SFTPs every file in this directory to C:\\deploy\\local-router on the target
  2. Runs `install.ps1` on the target via SSH (admin elevation required)
  3. Streams the installer output back

If the target already has VictoriaLogs available locally, you can pass it
through with `--vl-exe C:\\path\\to\\victoria-logs.exe` so the installer
skips the GitHub download.
"""
from __future__ import annotations
import argparse
import os
import sys
import time

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

DEFAULT_REMOTE_DIR = "/C:/deploy/local-router"


def parse_args():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default=os.environ.get("TC_OTEL_HOST"),
                   help="Target IP or hostname (env TC_OTEL_HOST)")
    p.add_argument("--user", default=os.environ.get("TC_OTEL_USER", "Administrator"),
                   help="SSH user (env TC_OTEL_USER, default Administrator)")
    p.add_argument("--password", default=os.environ.get("TC_OTEL_PASSWORD"),
                   help="SSH password (env TC_OTEL_PASSWORD; omit to use key-based auth)")
    p.add_argument("--remote-dir", default=DEFAULT_REMOTE_DIR,
                   help=f"Where to stage files on the target (default {DEFAULT_REMOTE_DIR})")
    p.add_argument("--vl-exe", default=None,
                   help="If set, pass to install.ps1 -VlExe to skip the GitHub download")
    p.add_argument("--skip-firewall", action="store_true")
    args = p.parse_args()
    if not args.host:
        p.error("--host or TC_OTEL_HOST is required")
    return args


def ssh_stream(client, cmd, label="install"):
    chan = client.get_transport().open_session()
    chan.set_combine_stderr(True)
    chan.exec_command(cmd)
    chan.settimeout(2)
    while not chan.exit_status_ready() or chan.recv_ready():
        try:
            chunk = chan.recv(8192)
            if chunk:
                for line in chunk.decode("utf-8", errors="replace").splitlines():
                    print(f"[{label}] {line}", flush=True)
        except Exception:
            time.sleep(0.2)
    return chan.recv_exit_status()


def main():
    args = parse_args()
    import paramiko

    here = os.path.dirname(os.path.abspath(__file__))

    print(f">>> Connecting to {args.host} as {args.user} ...")
    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(args.host, username=args.user, password=args.password, timeout=10)

    print(f">>> Uploading dist to {args.remote_dir} ...")
    sftp = c.open_sftp()

    def mkdir_p(path):
        parts = []
        head = path
        while head and head not in ("/", ""):
            parts.append(head)
            head, _ = os.path.split(head.rstrip("/"))
        for p in reversed(parts):
            try:
                sftp.mkdir(p)
            except IOError:
                pass

    mkdir_p(args.remote_dir)

    for fname in sorted(os.listdir(here)):
        full = os.path.join(here, fname)
        if not os.path.isfile(full):
            continue
        if fname.endswith((".pyc", ".swp")) or fname == "remote_deploy.py":
            continue
        size_kb = max(1, os.path.getsize(full) // 1024)
        print(f"    {fname} ({size_kb} KB)")
        sftp.put(full, f"{args.remote_dir}/{fname}")
    sftp.close()

    # Strip leading /C: for the cmd-line path
    win_dir = args.remote_dir.lstrip("/").replace("/", "\\")

    install_args = ""
    if args.vl_exe:
        install_args += f' -VlExe "{args.vl_exe}"'
    if args.skip_firewall:
        install_args += " -SkipFirewall"

    cmd = (
        f'powershell -NonInteractive -ExecutionPolicy Bypass '
        f'-File {win_dir}\\install.ps1{install_args}'
    )
    print(f">>> Running: {cmd}")
    rc = ssh_stream(c, cmd, label="install")
    print(f">>> install.ps1 exit code: {rc}")

    c.close()


if __name__ == "__main__":
    main()
