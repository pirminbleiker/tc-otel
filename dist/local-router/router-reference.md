# TwinCAT AMS Router — protocol reference & performance notes

Deep dive into the TCP/48898 protocol, what the local router exposes,
how it dispatches frames internally, and what performance you can
expect on the loopback path. Written for someone who already knows the
basics from [`architecture.md`](architecture.md).

## Contents

1. [Port-registration handshake](#1-port-registration-handshake)
2. [AMS/TCP control commands](#2-amstcp-control-commands)
3. [ADS commands](#3-ads-commands-in-the-ams-header)
4. [Useful AMS ports tc-otel could talk to](#4-useful-ams-ports-tc-otel-could-talk-to)
5. [Important Index Groups](#5-important-index-groups)
6. [Performance — local-router vs. direct TCP](#6-performance--local-router-vs-direct-tcp)
7. [How the TwinCAT router dispatches frames internally](#7-how-the-twincat-router-dispatches-frames-internally)
8. [What this means for tc-otel](#8-what-this-means-for-tc-otel)

---

## 1. Port-registration handshake

Every AMS/TCP frame is prefixed by **6 bytes**: `u16 commandId` +
`u32 dataLength` (both little-endian). The `commandId` decides whether
the router consumes the frame itself or forwards it to a destination
port.

```
TCP connect → 127.0.0.1:48898            (TwinCAT router listener)
↓
Send:  [00 10] [02 00 00 00] [16 3F]
       └─cmd──┘ └──length──┘ └port─┘
       0x1000   2 bytes      16150 (LE)
       PortConnect
↓
Recv:  [00 10] [08 00 00 00] [AC 1C 29 25 01 01] [16 3F]
       └─cmd──┘ └──length──┘ └────NetId 6B────┘ └port─┘
       0x1000   8 bytes      172.28.41.37.1.1   16150
↓
Frame loop: every subsequent inbound packet is cmd 0x0000 with a
            32-byte AMS header + ADS payload, addressed to the
            registered port.
```

The router stores the registration as **`(srcNetId, port) → socket fd`**
in its in-memory port table. From then on, any frame whose AMS
target field matches `<localNetId>:<port>` is written to that fd. No
authentication; the router trusts the loopback peer.

`PortClose` (`cmd 0x0001`, empty payload) cleanly removes the entry.

## 2. AMS/TCP control commands

Only **five** values are publicly defined. They live in the 2-byte
`commandId` field of the AMS/TCP header (i.e. *outside* the 32-byte
AMS header).

| Hex | Name | Direction | Payload | Purpose |
|---|---|---|---|---|
| `0x0000` | `AMS_CMD` | both | 32-byte AMS header + ADS data | regular ADS frame |
| `0x0001` | `PORT_CLOSE` | client → router | empty | clean port deregistration |
| `0x1000` | `PORT_CONNECT` | both | req: `u16` port (0 = any). resp: 6 B NetId + `u16` port | register an AMS port |
| `0x1001` | `ROUTER_NOTE` | router → client | `u32` state (1 = RUNNING, 0 = STOP) | TwinCAT lifecycle events |
| `0x1002` | `GET_LOCAL_NETID` | both | resp: 6 B NetId | runtime NetId query |

There is **no auth/login at the AMS/TCP layer**. Secure ADS is just
AMS-over-TLS 1.2 on TCP/8016 (cert-based mTLS at the TLS layer); it
adds no opcodes here.

The router *terminates* `0x0001`/`0x1000`/`0x1001`/`0x1002`. Anything
inside `0x0000` is dispatched by `target NetId : port` to either a
locally-registered port (this is what `local_router` consumes) or to
another router on a remote machine (via the static-routes table).

## 3. ADS commands (in the AMS header)

Inside a `cmd 0x0000` frame, the 32-byte AMS header carries a 16-bit
`commandId` selecting one of nine ADS operations.

| ID | ADS command | Used by tc-otel |
|---|---|---|
| 1 | `ReadDeviceInfo` | optional heartbeat |
| 2 | `Read` | symbol read (polling) |
| 3 | `Write` | **PLC writes log/metric/trace frames here** |
| 4 | `ReadState` | health check |
| 5 | `WriteControl` | (start/stop PLC — not used) |
| 6 | `AddDeviceNotification` | subscribe |
| 7 | `DeleteDeviceNotification` | unsubscribe |
| 8 | `Notification` | unsolicited push from PLC |
| 9 | `ReadWrite` | symbol-handle lookup (`IG=0xF003`), bulk SumUp ops |

Higher numbers don't exist in any public source. The 16-bit field
allows more, but Beckhoff has never shipped one.

## 4. Useful AMS ports tc-otel could talk to

A single registered TCP connection lets tc-otel address **any** port
on the local NetId (and any reachable remote NetId). Useful targets:

| Port | Service | What you can do |
|---|---|---|
| `1` | Router | route table, route notifications |
| `30` | License Server | license status |
| `100` | Logger | TwinCAT system logger |
| `110` | EventLogger | structured TwinCAT events |
| `200` | R0_Realtime | real-time CPU/latency stats |
| `300` | R0_IO | IO subsystem |
| `500` (501 SAF, 511 SVB) | R0_NC | motion control |
| `851`-`854` | TC3 PLC Runtime 1-4 | symbols, variable values, notifications |
| `10000` | SystemService | TwinCAT state, license info, route list (`IG=0x322`), CPU stats |
| `10500` | AmsLogger | AMS-internal logger |
| `14000` | Scope | TwinCAT Scope feed |

User-allocated dynamic ports come from the standalone router as
`PORT_BASE = 30000` upwards (`PORT_BASE + 128`). Beckhoff's TwinCAT
router uses similar dynamic ranges.

So one PortConnect connection is enough to (for example) enumerate all
PLC runtimes, fetch their symbol tables, and subscribe to
notifications — without a second socket. That's the upgrade path from
"passive receiver" to "active symbol browser".

## 5. Important Index Groups

Sent inside an `ADS_CMD_READ_WRITE` (cmd 9) request to a runtime port
(typically 851):

| IG | Purpose |
|---|---|
| `0xF003` | `GET_SYMHANDLE_BYNAME` — resolve symbol name → handle |
| `0xF004` | `GET_SYMVAL_BYNAME` — read value by name |
| `0xF005` | RW by handle |
| `0xF00B` | `SYM_UPLOAD` — full symbol list |
| `0xF00F` | `SYM_UPLOAD_INFO2` — size of the symbol list |
| `0xF010` | `SYM_NOTE` — notification by symbol name |
| `0xF080`-`0xF086` | SumUp (bulk read / write / RW / addNote / delNote in one round-trip) |
| `0x4020` | `%M` PLC marker area |
| `0x4040` | PLC data area |
| `0x4000`/`0x4030` | input bytes / retain |
| `0x322` | system-service route table |

## 6. Performance — comparing the three tc-otel transports

The realistic deployment for each transport mode and what it costs per
frame on a typical Windows x86_64 box:

| Transport | Realistic deployment | End-to-end path | Per-frame latency | Notes |
|---|---|---|---|---|
| **`local_router`** | tc-otel **on the TwinCAT box** | PLC kernel → router IPC → loopback TCP → tc-otel | **~30-80 µs** | One TCP hop, no NIC, no wire |
| `tcp` | tc-otel on a **separate** box (TwinCAT-free) | PLC kernel → router IPC → NIC → wire → NIC → tc-otel TCP server | **NIC + LAN** (sub-ms typical, 100 µs-few ms) | Two NIC traversals + medium |
| `mqtt` | tc-otel anywhere + MQTT broker | PLC → router → MQTT publish → broker → MQTT subscribe → tc-otel | **broker round-trip + MQTT framing**, even if broker is local | Adds the broker as another process hop |

**`local_router` is the fastest of the three** in any realistic
single-box scenario. It's the only mode that stays entirely on
loopback. `tcp` always carries network cost (it's only chooseable on
a separate machine), and `mqtt` adds a broker hop on top of whatever
transport the broker uses. Earlier drafts of this document compared
`local_router` with a hypothetical "direct AMS/TCP server on the same
machine" — that scenario doesn't exist for tc-otel because the only
producer of AMS frames *is* the TwinCAT router that already owns
:48898.

Cost breakdown for the local-router path (the dominant case):

- Header parse + port-table lookup in the router: **sub-µs**
- Two kernel transitions on the loopback TCP hop (`send` in router,
  `recv` in tc-otel): dominant share, ~30-50 µs
- One `memcpy` into the router's heap buffer + one back out: minor on
  small frames, halves throughput on big ones
- **No zero-copy.** Both Beckhoff's TwinCAT router and the standalone
  open-source code use plain `recv`/`send`. No `TransmitFile`, no
  `WSASend MSG_PARTIAL`, no shared memory. TwinCAT also does not
  enable `SIO_LOOPBACK_FAST_PATH` (~10-20 µs would be possible if it
  did), so we sit on the slow path.

What this means in practice for tc-otel:

- A log WRITE frame is 32 B AMS header + ~50-2000 B payload. At ~50 µs
  overhead per frame, a 100 Hz PLC task spends ~5 ms of CPU per second
  on the router hop — negligible.
- For batched 2480-byte push-diag frames the picture is the same:
  ~50 µs router overhead, bandwidth is nowhere near the bottleneck.
- **The bottleneck is tc-otel's dispatcher, not the router.** Channel
  fullness (`trace-event channel full, dropping`) shows up long before
  the router becomes a constraint. Bump `service.channel_capacity` if
  you see drops.

## 7. How the TwinCAT router dispatches frames internally

From the open-source standalone implementation in
[`Beckhoff/ADS/AdsLib/standalone/`](https://github.com/Beckhoff/ADS/tree/master/AdsLib/standalone):

**Port table.** `AmsPort ports[NUM_PORTS_MAX = 128]`, indexed by
`port - PORT_BASE` (`PORT_BASE = 30000`). Each `AmsPort` carries a
`tmms` timeout and an `IsOpen()` flag — it does *not* hold a NetId.
NetId resolution happens separately through
`mapping: AmsNetId → AmsConnection*` (one entry per known peer
router).

**Receive flow** (`AmsConnection::Recv`, one receiver thread per
outbound TCP socket):

1. Read the 6-byte AMS/TCP header.
2. If `commandId != 0x0000`: handle in-router (`ROUTER_NOTE`,
   `GET_LOCAL_NETID`, etc.). Done.
3. If `commandId == 0x0000`: read the 32-byte AMS header and switch on
   `cmdId`:
   - `cmd 8 (Notification)`: lookup
     `dispatcherList: map<{srcAddr, dstPort}, SharedDispatcher>`,
     queue the frame to that dispatcher's worker thread (bypasses the
     normal request/response queue).
   - all other cmds: append to `queue[port - PORT_BASE]`, matched by
     `invokeId` via `GetPending()` for request/response correlation.
4. Unknown `cmdId` → `ReceiveJunk()` drains the bytes and logs
   *"Unkown AMS command id"*.

**Loopback is not in-process.** Even when PLC and tc-otel run on the
same physical box, every frame still travels:

```
PLC kernel driver  →  TCP (127.0.0.1)  →  TwinCAT router service  →  TCP (127.0.0.1)  →  tc-otel
```

There is **no** shared-memory shortcut on Windows. Each hop is a real
`send`/`recv` pair against the kernel TCP stack.

**Multi-process.** Multiple applications all connect to
`127.0.0.1:48898`, send their own `PortConnect`, and get distinct
ports. The router multiplexes by `(srcNetId, port) → fd`. Two
processes cannot share a port; the router rejects a duplicate
registration.

## 8. What this means for tc-otel

- On a **TwinCAT-equipped machine**, `local_router` is the *only*
  option (TwinCAT owns :48898) **and also the fastest of the three
  transports** — it's the only one without either a network hop or a
  broker hop.
- `tcp` makes sense when tc-otel runs on a **separate** box collecting
  from one or several remote TwinCAT systems. You pay LAN latency,
  but `tcp` doesn't compete with a same-machine `local_router` for
  the same scenario.
- `mqtt` is for fan-out / cloud-style aggregation: trade per-frame
  performance for an addressable bus that many subscribers can attach
  to. Don't pick it when you only need a single tc-otel.
- For high-volume PLCs the bottleneck is tc-otel's dispatcher, not
  the router — bump `service.channel_capacity` first if you see drops.
- **Future work.** The PortConnect socket is bidirectional. tc-otel
  could become an *active* client too — issuing
  `cmd 9 ReadWrite` with `IG=0xF00B SYM_UPLOAD` to enumerate PLC
  symbols, or `cmd 6 AddDeviceNotification` to subscribe to PLC
  variables, all over the *same* connection that already handles
  passive log ingest. Implementing that means filling in
  `AmsTransport::send()` (currently a no-op for `LocalRouterAmsTransport`)
  with proper `invokeId` correlation against the ports table the
  router maintains.

## Sources

- [Beckhoff/ADS](https://github.com/Beckhoff/ADS) — `AmsHeader.h`,
  `AdsDef.h`, `standalone/AmsRouter.{h,cpp}`,
  `standalone/AmsConnection.{h,cpp}`
- [jisotalo/ads-server `ads-commons.ts`](https://github.com/jisotalo/ads-server/blob/master/src/ads-commons.ts)
- [birkenfeld/ads-rs `ports.rs` and `index.rs`](https://github.com/birkenfeld/ads-rs)
- [runZero TwinCAT 3 ADS protocol writeup](https://www.runzero.com/blog/twincat-3-ads-protocol/)
- [Beckhoff infosys: TwinCAT ADS specification](https://infosys.beckhoff.com/content/1033/tcadscommon/12440276875.html)
- [MS WinCAT — Fast TCP loopback on Windows Server 2012+](https://learn.microsoft.com/en-us/archive/blogs/wincat/fast-tcp-loopback-performance-and-low-latency-with-windows-server-2012-tcp-loopback-fast-path)
- [`Beckhoff.TwinCAT.Ads.TcpRouter` NuGet](https://www.nuget.org/packages/Beckhoff.TwinCAT.Ads.TcpRouter)
