# Helper scripts

Developer helpers. None of them embed site-specific values: targets are
passed as arguments or read from environment variables. Precedence is
CLI argument, then environment variable, then the default.

| Script | Purpose | Required | Optional |
| --- | --- | --- | --- |
| `redeploy_protobuf.py` | Stop, wipe, re-upload and restart tc-otel + Victoria backends on a Windows target over SSH, then check each pillar | `--host` / `TC_OTEL_HOST` | `--user`, `--password`, `--exe`, `--dist-dir` |
| `scope_resolver_poc.py` | Download a PLC symbol table over ADS and resolve instance paths to FB type names | none | `--net-id`, `--port`, `--query` (repeatable) |
| `run-runtime-tests.sh` | Run the runtime tests in a container | none | `TC_RUNTIME_IMAGE` |
| `../dist/local-router/remote_deploy.py` | Push `dist/local-router` to a target over SSH and run `install.ps1` | `--host` / `TC_OTEL_HOST` | `--user`, `--password`, `--remote-dir`, `--vl-exe`, `--skip-firewall` |
| `../dist/local-router/probe_port_connect.py` | Probe the local AMS router with PortConnect | none | positional `listen_sec` (default 5) |

## Environment variables

| Variable | Used by | Meaning |
| --- | --- | --- |
| `TC_OTEL_HOST` | `redeploy_protobuf.py`, `remote_deploy.py` | Target IP or hostname (e.g. `192.0.2.10`) |
| `TC_OTEL_USER` | same | SSH user, default `Administrator` |
| `TC_OTEL_PASSWORD` | same | SSH password; unset means key / agent auth |
| `TC_OTEL_EXE` | `redeploy_protobuf.py` | Local `tc-otel.exe`, default `<repo>/target/release/tc-otel.exe` |
| `TC_OTEL_DIST_DIR` | `redeploy_protobuf.py` | Local dir with `config.json`, default `<repo>/dist/local-router` |
| `TC_OTEL_NET_ID` | `scope_resolver_poc.py` | AMS Net ID, default `127.0.0.1.1.1` |
| `TC_RUNTIME_IMAGE` | `run-runtime-tests.sh` | Container image override |

## Dependencies

`--help` works without extra packages. Actually running the scripts
needs `paramiko` (`redeploy_protobuf.py`, `remote_deploy.py`) or `pyads`
(`scope_resolver_poc.py`, plus the TwinCAT ADS library).
