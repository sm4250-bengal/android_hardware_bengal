#!/usr/bin/env python3
"""Exercise production loc.c through the existing AF_UNIX peer, never QRTR."""
import os
import signal
import socket
import struct
import subprocess
import sys


def tlv(tag, data):
    return struct.pack("<BH", tag, len(data)) + data


def packet(kind, txn, msg, body):
    return struct.pack("<BHHH", kind, txn, msg, len(body)) + body


def run(binary, case):
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    env = os.environ.copy()
    if case == "cancel-before-send":
        env["LOC_TEST_CANCEL_BEFORE_DELETE"] = "1"
    if case == "log-fail":
        env["LOC_TEST_LOG_FAIL"] = "LOC INDICATION"
    process = subprocess.Popen([binary, str(child.fileno()), "--delete-aiding-all"],
        pass_fds=[child.fileno()], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env)
    child.close()
    parent.settimeout(2)
    sent = []

    def send(data, node=7, port=4107, client=55, kind=1):
        parent.send(struct.pack("<6I", kind, node, port, 16, 9, client) + data)

    try:
        lookup = parent.recv(8192)
        assert struct.unpack("<6I", lookup) == (10, 0, 0xfffffffe, 16, 0, 55)
        if case != "discovery-timeout":
            send(b"", kind=4)
        data = parent.recv(8192)
        if case not in ("cancel-before-send", "discovery-timeout"):
            assert struct.unpack_from("<6I", data) == (1, 7, 4107, 16, 0, 55)
            # Independent exact wire oracle: one request, transaction1, ID44,
            # one mandatory01/u8/1; no optional masks or other commands.
            assert data[24:] == bytes.fromhex("0001004400040001010001"), data.hex()
            sent.append(0x44)
            response = packet(2, 1, 0x44, tlv(2, bytes(4)))
            indication = packet(4, 73, 0x44, tlv(1, bytes(4)))
            if case == "response-first":
                send(response); send(indication)
            elif case == "indication-first":
                send(indication); send(response)
            elif case == "noise-then-success":
                send(response, node=8); send(indication, port=4108)
                send(response, client=56)
                send(packet(2, 2, 0x44, tlv(2, bytes(4))))
                send(packet(4, 90, 0x43, tlv(1, bytes(4))))
                send(indication); send(response)
            elif case == "wrong-peer-only":
                send(response, node=8); send(indication, node=8)
            elif case == "wrong-client-only":
                send(response, client=56); send(indication, client=56)
            elif case == "wrong-txn-only":
                send(indication); send(packet(2, 2, 0x44, tlv(2, bytes(4))))
            elif case == "response-only":
                send(response)
            elif case == "indication-only":
                send(indication)
            elif case == "timeout":
                pass
            elif case == "cancel":
                os.kill(process.pid, signal.SIGTERM)
            elif case == "cancel-after-response":
                send(response); os.kill(process.pid, signal.SIGINT)
            elif case == "service-loss":
                send(b"", kind=5)
            elif case == "transport-loss":
                parent.shutdown(socket.SHUT_WR)
            elif case == "response-rejected":
                send(packet(2, 1, 0x44, tlv(2, struct.pack("<HH", 1, 94))))
            elif case == "response-error-with-success":
                send(packet(2, 1, 0x44, tlv(2, struct.pack("<HH", 0, 42))))
            elif case == "indication-rejected":
                send(packet(4, 73, 0x44, tlv(1, struct.pack("<I", 2))))
            elif case == "short-response":
                send(packet(2, 1, 0x44, tlv(2, bytes(3))))
            elif case == "missing-response":
                send(packet(2, 1, 0x44, b""))
            elif case == "duplicate-response-tlv":
                send(packet(2, 1, 0x44, tlv(2, bytes(4)) * 2))
            elif case == "short-indication":
                send(packet(4, 73, 0x44, tlv(1, bytes(3))))
            elif case == "missing-indication":
                send(packet(4, 73, 0x44, b""))
            elif case == "duplicate-indication-tlv":
                send(packet(4, 73, 0x44, tlv(1, bytes(4)) * 2))
            elif case == "bad-framing":
                send(indication[:-1])
            elif case == "wrong-kind":
                send(packet(0, 1, 0x44, tlv(1, bytes(4))))
            elif case == "log-fail":
                send(indication)
            else:
                raise AssertionError(case)
            data = parent.recv(8192)
        assert data == b"", (case, "extra command/retry", data.hex())
        out, err = process.communicate(timeout=2)
        success = case in ("response-first", "indication-first", "noise-then-success")
        assert process.returncode == (0 if success else 1), (case, out, err)
        if case != "log-fail":
            expected = "ACCEPTED" if success else "UNKNOWN" if sent else "NOT_SENT"
            assert "LOC DELETE_AIDING_ALL reset=" + expected in out, (case, out)
            assert "cleanup=NOT_REQUIRED" in out, (case, out)
        assert "RESTORED" not in out and "cleanup=CONFIRMED" not in out, (case, out)
        if success:
            assert "LOC RESPONSE id=0044 txn=1 result=0 error=0" in out
            assert "LOC INDICATION id=0044 sequence=73 status=0" in out
        assert len(sent) == (0 if case in ("cancel-before-send", "discovery-timeout") else 1)
        print("PASS delete-aiding", case)
    finally:
        parent.close()
        if process.poll() is None:
            process.kill(); process.wait()


for case in ("response-first", "indication-first", "noise-then-success", "wrong-peer-only",
             "wrong-client-only", "wrong-txn-only", "response-only", "indication-only", "timeout",
             "cancel", "cancel-after-response", "cancel-before-send", "discovery-timeout",
             "service-loss", "transport-loss", "response-rejected", "response-error-with-success",
             "indication-rejected", "short-response", "missing-response", "duplicate-response-tlv",
             "short-indication", "missing-indication", "duplicate-indication-tlv", "bad-framing",
             "wrong-kind", "log-fail"):
    run(sys.argv[1], case)
