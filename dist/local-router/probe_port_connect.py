"""
Local AMS port registration test:
- Connect 1: register port 16150, listen for inbound frames
- Connect 2: register any port, send ADS WRITE to <localNetId>:16150
- Connect 1 should receive the frame via the local router
"""
import socket
import struct
import sys
import threading
import time

ROUTER_HOST = '127.0.0.1'
ROUTER_PORT = 48898
TARGET_PORT = 16150  # ADS_LOG_PORT — what we want to register

CMD_ADS          = 0x0000
CMD_PORT_CONNECT = 0x1000


def send_pkt(sock, cmd_id, payload=b''):
    sock.sendall(struct.pack('<HI', cmd_id, len(payload)) + payload)


def recv_pkt(sock):
    hdr = b''
    while len(hdr) < 6:
        chunk = sock.recv(6 - len(hdr))
        if not chunk:
            raise ConnectionError("closed")
        hdr += chunk
    cmd_id, data_len = struct.unpack('<HI', hdr)
    data = b''
    while len(data) < data_len:
        chunk = sock.recv(data_len - len(data))
        if not chunk:
            raise ConnectionError("closed")
        data += chunk
    return cmd_id, data


def port_connect(sock, requested):
    send_pkt(sock, CMD_PORT_CONNECT, struct.pack('<H', requested))
    cmd, resp = recv_pkt(sock)
    assert cmd == CMD_PORT_CONNECT and len(resp) >= 8, f"bad: cmd=0x{cmd:x} resp={resp.hex()}"
    netid = bytes(resp[0:6])
    port  = struct.unpack_from('<H', resp, 6)[0]
    return netid, port


def parse_ams_frame(payload):
    if len(payload) < 32:
        return f"too short ({len(payload)}B)"
    tnet  = '.'.join(str(b) for b in payload[0:6])
    tport = struct.unpack_from('<H', payload, 6)[0]
    snet  = '.'.join(str(b) for b in payload[8:14])
    sport = struct.unpack_from('<H', payload, 14)[0]
    cmd   = struct.unpack_from('<H', payload, 16)[0]
    flags = struct.unpack_from('<H', payload, 18)[0]
    err   = struct.unpack_from('<I', payload, 24)[0]
    return f"target={tnet}:{tport} src={snet}:{sport} cmd=0x{cmd:04x} flags=0x{flags:04x} err=0x{err:x}"


def make_ams_frame(target_netid, target_port, source_netid, source_port,
                   cmd_id, state_flags, ads_payload):
    ams = struct.pack('<6sH6sHHHIII',
        target_netid, target_port,
        source_netid, source_port,
        cmd_id, state_flags,
        len(ads_payload), 0, 1)
    return ams + ads_payload


def main():
    listen_sec = int(sys.argv[1]) if len(sys.argv) > 1 else 5

    # === Connection 1: register port 16150, listen ===
    print(f"[L] Connecting + registering port {TARGET_PORT}...", flush=True)
    s1 = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s1.settimeout(5.0)
    s1.connect((ROUTER_HOST, ROUTER_PORT))
    netid_l, port_l = port_connect(s1, TARGET_PORT)
    print(f"[L] Registered: netId={'.'.join(str(b) for b in netid_l)} port={port_l}", flush=True)

    # === Connection 2: register any port, used for sending ===
    print(f"[S] Connecting + registering any port...", flush=True)
    s2 = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s2.settimeout(5.0)
    s2.connect((ROUTER_HOST, ROUTER_PORT))
    netid_s, port_s = port_connect(s2, 0)  # 0 = any
    print(f"[S] Registered: netId={'.'.join(str(b) for b in netid_s)} port={port_s}", flush=True)

    # Listener thread
    received = []
    def listen():
        s1.settimeout(0.5)
        deadline = time.time() + listen_sec
        while time.time() < deadline:
            try:
                cmd, data = recv_pkt(s1)
                if cmd == CMD_ADS:
                    parsed = parse_ams_frame(data)
                    print(f"[L] >> FRAME: {parsed}", flush=True)
                    received.append(data)
                else:
                    print(f"[L] >> control cmd=0x{cmd:x} {data.hex()}", flush=True)
            except (socket.timeout, TimeoutError):
                continue
            except (ConnectionError, OSError) as e:
                print(f"[L] connection: {e}", flush=True)
                break

    t = threading.Thread(target=listen, daemon=True)
    t.start()
    time.sleep(0.5)  # let listener arm

    # === From s2: send ADS WRITE to <netid_l>:TARGET_PORT ===
    # ADS WRITE payload: ig(4) + io(4) + len(4) + data
    ig = struct.pack('<I', 1)
    io = struct.pack('<I', 1)
    payload_data = b'\x42' * 16  # 16 bytes of dummy
    ads_payload = ig + io + struct.pack('<I', len(payload_data)) + payload_data

    ams = make_ams_frame(
        target_netid=netid_l, target_port=TARGET_PORT,
        source_netid=netid_s, source_port=port_s,
        cmd_id=3, state_flags=4,  # WRITE, request
        ads_payload=ads_payload,
    )

    print(f"[S] Sending ADS WRITE -> {'.'.join(str(b) for b in netid_l)}:{TARGET_PORT}", flush=True)
    send_pkt(s2, CMD_ADS, ams)

    # Wait for s2's response (write confirmation)
    s2.settimeout(3.0)
    try:
        cmd, data = recv_pkt(s2)
        print(f"[S] response cmd=0x{cmd:x}: {parse_ams_frame(data)}", flush=True)
    except Exception as e:
        print(f"[S] no response: {e}", flush=True)

    t.join(timeout=listen_sec + 2)

    print()
    if received:
        print(f">> SUCCESS: listener received {len(received)} frame(s) — local routing WORKS", flush=True)
    else:
        print(">> FAIL: listener received no frames", flush=True)

    s1.close()
    s2.close()


if __name__ == '__main__':
    main()
