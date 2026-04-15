#!/usr/bin/env python3
"""Decode AMS/ADS frames from mosquitto_sub hex captures.

Input line format: <ISO-timestamp> <topic> <payload-hex>
Topic: AdsOverMqtt/<net-id>/ams | .../ams/res | .../info
"""
import sys
import struct
from collections import Counter, defaultdict

ADS_CMD = {
    0: "Invalid", 1: "ReadDeviceInfo", 2: "Read", 3: "Write",
    4: "ReadState", 5: "WriteControl", 6: "AddNotification",
    7: "DelNotification", 8: "Notification", 9: "ReadWrite",
}

IDX_GROUP = {
    0x0000_4020: "PLC_RW_M (M-memory)",
    0x0000_4021: "PLC_W_M",
    0x0000_F003: "SYM_HNDBYNAME",
    0x0000_F004: "SYM_VALBYNAME",
    0x0000_F005: "SYM_VALBYHND",
    0x0000_F006: "SYM_RELEASEHND",
    0x0000_F008: "SYM_INFOBYNAME",
    0x0000_F009: "SYM_VERSION",
    0x0000_F00A: "SYM_INFOBYNAMEEX",
    0x0000_F00B: "SYM_DOWNLOAD",
    0x0000_F00C: "SYM_UPLOAD",
    0x0000_F00D: "SYM_UPLOADINFO",
    0x0000_F00E: "SYM_UPLOADINFO2",
    0x0000_F00F: "SYM_DT_UPLOAD",
    0x0000_F010: "SYM_DT_UPLOADINFO",
    0x0000_F020: "SUMUP_READ",
    0x0000_F021: "SUMUP_WRITE",
    0x0000_F022: "SUMUP_READWRITE",
    0x0000_F025: "SUMUP_READEX",
    0x0000_F040: "SUMUP_ADDDEVNOTE",
    0x0000_F041: "SUMUP_DELDEVNOTE",
    0x0000_F070: "SYM_INFO",
    0x0000_0500: "SYSTEM_SVC",
    0x0000_0501: "SYSTEM_REGHKEY",
    0x0000_0502: "SYSTEM_REGVAL",
    0x0000_0508: "SYSTEM_SVCCOMMAND",
    0x0000_0100: "PLC_R_I",
    0x0000_0101: "PLC_W_I",
    0x0000_0102: "PLC_RW_I",
    0x0000_0110: "PLC_R_Q",
    0x0000_0111: "PLC_W_Q",
    0x0000_0120: "PLC_RW_Q",
    0x0000_0130: "PLC_R_MEM",
    0x0000_0131: "PLC_W_MEM",
    0x0000_0132: "PLC_RW_MEM",
    0x0000_0140: "PLC_R_DATA",
    0x0000_2710: "SPS_STATUS",
}


def net_id(b: bytes) -> str:
    return ".".join(str(x) for x in b)


def decode_frame(payload: bytes):
    if len(payload) < 32:
        return {"short": True, "len": len(payload)}
    dst = net_id(payload[0:6])
    dst_port = struct.unpack_from("<H", payload, 6)[0]
    src = net_id(payload[8:14])
    src_port = struct.unpack_from("<H", payload, 14)[0]
    cmd, flags, dlen, err, invoke = struct.unpack_from("<HHIII", payload, 16)
    data = payload[32:32 + dlen]
    is_resp = bool(flags & 0x0001)
    frame = {
        "dst": f"{dst}:{dst_port}",
        "src": f"{src}:{src_port}",
        "cmd": ADS_CMD.get(cmd, f"0x{cmd:02x}"),
        "cmd_id": cmd,
        "flags": f"0x{flags:04x}",
        "is_resp": is_resp,
        "dlen": dlen,
        "err": err,
        "invoke": invoke,
        "data": data,
    }
    decode_cmd_payload(frame, cmd, is_resp, data)
    return frame


def decode_cmd_payload(frame, cmd, is_resp, data):
    if cmd == 2:  # Read
        if not is_resp and len(data) >= 12:
            ig, io, sz = struct.unpack_from("<III", data, 0)
            frame["ig"] = f"0x{ig:08x}"
            frame["ig_name"] = IDX_GROUP.get(ig, "?")
            frame["io"] = f"0x{io:08x}"
            frame["rlen"] = sz
        elif is_resp and len(data) >= 8:
            res, rlen = struct.unpack_from("<II", data, 0)
            frame["result"] = res
            frame["rlen"] = rlen
            frame["payload"] = data[8:8 + rlen].hex()
    elif cmd == 3:  # Write
        if not is_resp and len(data) >= 12:
            ig, io, sz = struct.unpack_from("<III", data, 0)
            frame["ig"] = f"0x{ig:08x}"
            frame["ig_name"] = IDX_GROUP.get(ig, "?")
            frame["io"] = f"0x{io:08x}"
            frame["wlen"] = sz
            frame["payload"] = data[12:12 + sz].hex()
    elif cmd == 9:  # ReadWrite
        if not is_resp and len(data) >= 16:
            ig, io, rsz, wsz = struct.unpack_from("<IIII", data, 0)
            frame["ig"] = f"0x{ig:08x}"
            frame["ig_name"] = IDX_GROUP.get(ig, "?")
            frame["io"] = f"0x{io:08x}"
            frame["rlen"] = rsz
            frame["wlen"] = wsz
            wdata = data[16:16 + wsz]
            frame["payload"] = wdata.hex()
            if wsz <= 256:
                try:
                    frame["payload_ascii"] = wdata.rstrip(b"\x00").decode("ascii", errors="replace")
                except Exception:
                    pass
        elif is_resp and len(data) >= 8:
            res, rlen = struct.unpack_from("<II", data, 0)
            frame["result"] = res
            frame["rlen"] = rlen
            frame["payload"] = data[8:8 + rlen].hex()
    elif cmd == 6:  # AddNotification
        if not is_resp and len(data) >= 40:
            ig, io, sz, trans, maxd, cycle = struct.unpack_from("<IIIIII", data, 0)
            frame["ig"] = f"0x{ig:08x}"
            frame["ig_name"] = IDX_GROUP.get(ig, "?")
            frame["io"] = f"0x{io:08x}"
            frame["notify_len"] = sz
            frame["trans_mode"] = trans
            frame["max_delay"] = maxd
            frame["cycle_time"] = cycle
        elif is_resp and len(data) >= 8:
            res, handle = struct.unpack_from("<II", data, 0)
            frame["result"] = res
            frame["handle"] = f"0x{handle:08x}"
    elif cmd == 7:  # DelNotification
        if not is_resp and len(data) >= 4:
            h, = struct.unpack_from("<I", data, 0)
            frame["handle"] = f"0x{h:08x}"
    elif cmd == 8:  # Notification (device → client)
        if len(data) >= 8:
            dlen_inner, stamps = struct.unpack_from("<II", data, 0)
            frame["stamps"] = stamps
            frame["notify_dlen"] = dlen_inner
            off = 8
            sample_list = []
            for _ in range(min(stamps, 4)):
                if off + 12 > len(data):
                    break
                ts, nsamp = struct.unpack_from("<QI", data, off)
                off += 12
                samples = []
                for _ in range(min(nsamp, 8)):
                    if off + 8 > len(data):
                        break
                    h, sz = struct.unpack_from("<II", data, off)
                    off += 8
                    val = data[off:off + sz]
                    off += sz
                    samples.append({"h": f"0x{h:08x}", "sz": sz, "val": val.hex()})
                sample_list.append({"ts": ts, "nsamp": nsamp, "samples": samples})
            frame["stamp_data"] = sample_list


def parse_line(line):
    parts = line.rstrip().split(" ", 2)
    if len(parts) != 3:
        return None
    ts, topic, hex_data = parts
    try:
        payload = bytes.fromhex(hex_data)
    except ValueError:
        return None
    return ts, topic, payload


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "-"
    mode = sys.argv[2] if len(sys.argv) > 2 else "summary"

    f = sys.stdin if path == "-" else open(path, "r", encoding="utf-8", errors="replace")

    cmd_counter = Counter()
    topic_counter = Counter()
    ig_counter = Counter()
    notif_handles = defaultdict(int)
    add_notifications = []
    reads = []
    readwrites = []
    frames = []

    for ln in f:
        parsed = parse_line(ln)
        if not parsed:
            continue
        ts, topic, payload = parsed
        topic_counter[topic] += 1
        if topic.endswith("/info"):
            try:
                frames.append({"ts": ts, "topic": topic, "info": payload.decode("utf-8", errors="replace")})
            except Exception:
                pass
            continue
        fr = decode_frame(payload)
        fr["ts"] = ts
        fr["topic"] = topic
        frames.append(fr)
        if "cmd" in fr:
            key = fr["cmd"] + (" REQ" if not fr.get("is_resp") else " RES")
            cmd_counter[key] += 1
            if "ig_name" in fr:
                ig_counter[f"{fr['ig']} {fr['ig_name']}"] += 1
            if fr["cmd_id"] == 6 and not fr["is_resp"]:
                add_notifications.append(fr)
            if fr["cmd_id"] == 8:
                for sd in fr.get("stamp_data", []):
                    for s in sd.get("samples", []):
                        notif_handles[s["h"]] += 1
            if fr["cmd_id"] == 2 and not fr["is_resp"]:
                reads.append(fr)
            if fr["cmd_id"] == 9 and not fr["is_resp"]:
                readwrites.append(fr)

    if mode == "summary":
        print("=== TOPICS ===")
        for t, c in topic_counter.most_common():
            print(f"  {c:6d}  {t}")
        print("\n=== ADS COMMANDS ===")
        for k, c in cmd_counter.most_common():
            print(f"  {c:6d}  {k}")
        print("\n=== INDEX GROUPS (requests) ===")
        for k, c in ig_counter.most_common():
            print(f"  {c:6d}  {k}")
        print("\n=== ADD NOTIFICATION REQUESTS ===")
        for n in add_notifications[:50]:
            print(f"  {n['ts']} {n.get('ig')} {n.get('ig_name','?')} io={n.get('io')} sz={n.get('notify_len')} mode={n.get('trans_mode')} cycle={n.get('cycle_time')}us")
        print(f"  (total: {len(add_notifications)})")
        print("\n=== ACTIVE NOTIFICATION HANDLES (in Notification frames) ===")
        for h, c in sorted(notif_handles.items(), key=lambda x: -x[1])[:30]:
            print(f"  {c:6d}  {h}")
        print(f"\n=== READWRITE (symbol lookups) samples ===")
        ascii_seen = set()
        for rw in readwrites[:200]:
            a = rw.get("payload_ascii", "")
            if a and a not in ascii_seen:
                ascii_seen.add(a)
                print(f"  {rw['ts']} ig={rw.get('ig_name','?')} wlen={rw.get('wlen')} ascii={a!r}")
    elif mode == "full":
        for fr in frames:
            print(fr)


if __name__ == "__main__":
    main()
