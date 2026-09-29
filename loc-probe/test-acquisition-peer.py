#!/usr/bin/env python3
"""Actual native query/socket seam, independent wire oracle; never QRTR."""
import os
import signal
import socket
import struct
import subprocess
import sys
import threading

IDS = (0xd5, 0xb7, 0xd8)
def tlv(tag, data):
    return struct.pack("<BH", tag, len(data)) + data
def u32(x):
    return struct.pack("<I", x)
def packet(kind, txn, msg, body):
    return struct.pack("<BHHH", kind, txn, msg, len(body)) + body

def run(binary, case, target=0xd5):
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    process = subprocess.Popen([binary, str(child.fileno()), "--acquisition-config"],
        pass_fds=[child.fileno()], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    child.close()
    parent.settimeout(2)
    sent, errors = [], []
    def send(p, node=7, client=55):
        parent.send(struct.pack("<6I", 1, node, 4107, 16, 9, client) + p)
    def peer():
        try:
            lookup = parent.recv(8192)
            assert struct.unpack("<6I", lookup) == (10, 0, 0xfffffffe, 16, 0, 55)
            parent.send(struct.pack("<6I", 4, 7, 4107, 16, 9, 55))
            while True:
                data = parent.recv(8192)
                if not data: break
                assert struct.unpack_from("<6I", data) == (1, 7, 4107, 16, 0, 55)
                kind, txn, msg, length = struct.unpack_from("<BHHH", data, 24)
                assert kind == 0 and txn == len(sent)+1 and len(data) == 31+length
                assert msg == IDS[len(sent)], (case, sent, msg) # Exact whitelist/order.
                assert data[31:] == (tlv(1, u32(1)) if msg == 0xd8 else b"")
                sent.append(msg)
                status = tlv(1, u32(0))
                value = (tlv(0x10, struct.pack("<H", 2400)) if msg == 0xd5 else
                    b"".join(tlv(0x10+i, struct.pack("<Q", (1 << (63-i)) | i)) for i in range(6))
                    if msg == 0xb7 else tlv(2, u32(1)) + tlv(0x10, b"\x5a"))
                response = packet(2, txn, msg, tlv(2, bytes(4)))
                if msg == target:
                    if case == "cancel":
                        os.kill(process.pid, signal.SIGTERM); continue
                    if case == "refused-response":
                        send(packet(2, txn, msg, tlv(2, struct.pack("<HH", 1, 42)))); continue
                    if case == "unsupported-response":
                        send(packet(2, txn, msg, tlv(2, struct.pack("<HH", 1, 94)))); continue
                    if case == "unsupported-indication":
                        status, value = tlv(1, u32(2)), tlv(2, u32(1)) if msg == 0xd8 else b""
                    if case == "failed-status": status, value = tlv(1, u32(5)), b""
                    if case == "short-status": status = tlv(1, b"\x00")
                    if case == "duplicate": value += value
                    if case == "absent": value = tlv(2, u32(1)) if msg == 0xd8 else b""
                    if case == "zero":
                        value = (tlv(0x10, bytes(2)) if msg == 0xd5 else
                            b"".join(tlv(0x10+i, bytes(8)) for i in range(6)) if msg == 0xb7 else
                            tlv(2, u32(1)) + tlv(0x10, b"\x00"))
                    if case == "one-absent": value = value[:-11]
                    if case == "unsupported-malformed": status, value = tlv(1, u32(2)), tlv(0x10, b"\x00")
                    if case == "long-value":
                        value = (tlv(2, u32(1)) if msg == 0xd8 else b"") + tlv(0x10, bytes(9))
                    if case == "short-response": response = packet(2, txn, msg, tlv(2, b"\x00"))
                    if case == "short-value":
                        value = (tlv(2, u32(1)) if msg == 0xd8 else b"") + tlv(0x10, b"")
                    if case == "week-max": value = tlv(0x10, b"\xff\xff")
                    if case == "wrong-param": value = tlv(2, u32(2)) + tlv(0x10, b"\x00")
                    if case == "missing-param": value = tlv(0x10, b"\x00")
                    if case == "short-param": value = tlv(2, b"\x01")
                    if case == "bad-elevation": value = tlv(2, u32(1)) + tlv(0x10, b"\x5b")
                    if case == "unrelated":
                        send(b"bad", node=99); send(b"bad", client=99)
                        send(packet(4, 77, 0xee, status))
                        send(packet(2, txn+20, msg, tlv(2, bytes(4))))
                indication = packet(4, 100+len(sent), msg, status+value+tlv(0xee, b"optional-extension"))
                if msg == target and case == "response-only": send(response)
                elif msg == target and case == "indication-only": send(indication)
                elif msg == target and case == "wrong-transaction":
                    send(packet(2, txn+1, msg, tlv(2, bytes(4)))); send(indication)
                elif msg == target and case == "wrong-indication":
                    send(response); send(packet(4, 101, 0xee, status+value))
                elif msg == target and case == "malformed-frame": send(response); send(indication[:-1])
                elif case == "indication-first": send(indication); send(response)
                else: send(response); send(indication)
        except (BrokenPipeError, ConnectionResetError): pass
        except Exception as error: errors.append(repr(error))
        finally: parent.close()
    thread = threading.Thread(target=peer)
    thread.start()
    stdout, stderr = process.communicate(timeout=3)
    thread.join(3)
    assert not thread.is_alive() and not errors, (case, errors, stdout, stderr)
    partial = case in ("one-absent", "absent", "unsupported-response", "unsupported-indication")
    success = case in ("success", "zero", "week-max", "indication-first", "unrelated")
    assert process.returncode == (0 if success else 1), (case, target, process.returncode, stdout, stderr)
    assert sent == list(IDS[:3 if success or partial else IDS.index(target)+1]), (case, sent)
    assert "cleanup=CONFIRMED" in stdout and "LOC START" not in stdout
    if success or partial:
        assert "outcome=ACQUISITION_CONFIG" + ("_PARTIAL" if partial else " ") in stdout
    else:
        assert "outcome=FAIL" in stdout
    if case in ("absent", "one-absent"): assert "unavailable" in stdout
    if case in ("unsupported-response", "unsupported-indication"): assert "unsupported=" in stdout
    if case == "zero":
        needle = {0xd5:"minimum-gps-week=0",0xb7:"blacklist GLONASS=0x0000000000000000",0xd8:"minimum-sv-elevation-deg=0"}[target]
        assert needle in stdout
    if case == "success":
        for needle in ("minimum-gps-week=2400", "blacklist GLONASS=0x8000000000000000",
                       "minimum-sv-elevation-deg=90"):
            assert needle in stdout, stdout
    if case == "week-max": assert "minimum-gps-week=65535" in stdout
    print(f"PASS acquisition {case} target={target:04x} TX={sent}")

cases = [(case, target) for target in IDS for case in
    ("success", "zero", "absent", "unsupported-response", "unsupported-indication",
     "refused-response", "failed-status", "short-status", "short-value", "duplicate",
     "long-value", "short-response", "unsupported-malformed", "cancel", "response-only", "indication-only", "wrong-transaction", "wrong-indication", "malformed-frame")]
cases += [("one-absent", 0xb7), ("indication-first", 0xd5), ("unrelated", 0xd5), ("week-max", 0xd5)]
cases += [(case, 0xd8) for case in ("wrong-param", "missing-param", "short-param", "bad-elevation")]
for case, target in cases: run(sys.argv[1], case, target)
print(f"PASS {len(cases)} acquisition-config peer scenarios; no hardware")
