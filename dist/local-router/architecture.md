# local-router transport — architecture

## Why this exists

When tc-otel runs **on the same machine as TwinCAT**, it cannot bind
`TCP/48898`: TwinCAT's `TcAmsRouter` already owns it. The previous workaround
was to run tc-otel on a separate machine with its own AMS NetId and a
`StaticRoutes.xml` entry on the TwinCAT side. That requires:

- Editing `StaticRoutes.xml` (and a TwinCAT restart to pick it up)
- Reserving an AMS NetId for tc-otel
- A custom port wouldn't help anyway: TwinCAT's `TCP_IP` route type always
  dials the destination on `48898` — there is no `Port` attribute for it
  (only `<Mqtt>` routes accept `Port=`).

The local-router transport eliminates all of that. tc-otel becomes a
**client** of the running TwinCAT router.

## How it works

```
+--------------+        TCP                  +---------------+
| TwinCAT PLC  |   ADSWRITE                  | tc-otel       |
| (task code)  |---->>>------+               |  (this binary)|
+--------------+             |               +-------+-------+
                             v                       ^
                  +---------------------+            |
                  |  TwinCAT AMS Router |            |
                  |  bound to :48898    |            |
                  |  (TcAmsRouter)      |            |
                  +----+----------------+            |
                       |                             |
   on PortConnect: registers tc-otel as              |
   <localNetId>:16150 in the router's                |
   in-process port table; from then on              |
   any frame with that target is fan-               |
   ned to tc-otel's open socket --------------------+
```

1. tc-otel opens an outbound TCP connection to `127.0.0.1:48898`.
2. It sends an AMS/TCP frame with `commandId = 0x1000` (`PortConnect`)
   and a 2-byte little-endian payload = the requested AMS port (`16150`,
   the well-known `ADS_LOG_PORT` used by `FB_TcOtelTask`).
3. The router replies with the same `commandId`, an 8-byte body =
   6 bytes local AmsNetId + 2 bytes assigned port.
4. From then on, every ADS frame the PLC writes to
   `<localNetId>:16150` is delivered over the same socket as a
   `commandId = 0x0000` AMS/TCP packet.
5. tc-otel's normal `AdsRouter::dispatch()` consumes the frame and
   pushes it through the existing log/metric/trace/diag pipelines.

This is exactly the same pattern that `TcAdsDll.dll`, `TwinCAT Scope`,
and any open-source ADS client uses (see `Beckhoff/ADS`, `jisotalo/ads-server`,
`birkenfeld/ads-rs`). There is no separate Windows named pipe — on
Windows the local AMS path is just TCP loopback.

## AMS/TCP control commands used

The first 2 bytes of every AMS/TCP packet hold the command ID. For
ordinary ADS commands this field is `0x0000`. For router-control frames
it carries one of:

| ID       | Name              | Direction      | Payload |
|----------|-------------------|----------------|---------|
| `0x0000` | ADS command       | both           | 32-byte AMS header + ADS payload |
| `0x0001` | Port close        | client → router| (none) |
| `0x1000` | **Port connect**  | client → router| `u16 LE` requested port (0 = any) |
| `0x1001` | Router note       | router → client| router state |
| `0x1002` | Get local NetId   | client → router| (none) |

tc-otel only sends `0x1000` (on connect) and `0x0001` (on shutdown);
it consumes `0x0000` (frames) and `0x1001` (router state).

## Why dynamic route learning *doesn't* work

It's tempting to hope that simply opening a TCP connection to `:48898`
and sending an arbitrary AMS frame with `source_net_id = X` would cause
the router to learn the route to `X` over that socket. **It does not.**

The router only honours static route entries (`StaticRoutes.xml`) and
internally registered local ports (the `PortConnect` flow above).
A frame with an unknown source NetId is rejected with `adsErrCode 0x12`
(see `probe_local_ams_router.py` in the project's `scripts/` folder for a
reproducer).

## What the PLC does

Unchanged. `FB_TcOtelTask` calls
`Tc2_System.ADSWRITE` with `PORT := 16150` and `NETID := sAmsNetId`.
With `PRG_TaskLog.Init('')` the empty string means "local NetId" — the
TwinCAT runtime resolves it to the running system's own NetId, hands the
frame to its own router, the router looks at port 16150 in its port
table, finds tc-otel's registered socket, and writes the frame there.

## Implementation pointer

- **Rust:** `crates/tc-otel-ads/src/transport/local_router.rs`
- **Config variant:** `TransportConfig::LocalRouter` in
  `crates/tc-otel-core/src/config.rs`
- **Probe / reproducer:** `dist/local-router/probe_port_connect.py`

## References

- [Beckhoff AMS/TCP packet spec](https://infosys.beckhoff.com/content/1033/tcadscommon/12440280843.html)
- [AMS/TCP header](https://infosys.beckhoff.com/content/1033/tcadscommon/12440282379.html)
- [AMS header](https://infosys.beckhoff.com/content/1033/tcadscommon/12440283915.html)
- [jisotalo/ads-server `AMS_HEADER_FLAG` constants](https://github.com/jisotalo/ads-server/blob/master/src/ads-commons.ts)
- [Beckhoff/ADS standalone router C++](https://github.com/Beckhoff/ADS/tree/master/AdsLib/standalone)
