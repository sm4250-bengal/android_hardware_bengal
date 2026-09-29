#!/usr/bin/env python3
"""Independent little-endian wire oracle; only AF_UNIX, never QRTR."""
import os
import signal
import socket
import struct
import subprocess
import sys
import threading
import time


def tlv(tag, data):
    return struct.pack("<BH", tag, len(data)) + data


def qmi(kind, txn, msg, data):
    return struct.pack("<BHHH", kind, txn, msg, len(data)) + data


def u32(value):
    return struct.pack("<I", value)


def parse(data):
    kind, txn, msg, length = struct.unpack_from("<BHHH", data)
    assert kind == 0 and txn > 0 and len(data) == 7 + length
    fields = {}
    offset = 7
    while offset < len(data):
        tag, n = struct.unpack_from("<BH", data, offset)
        offset += 3
        assert tag not in fields and offset + n <= len(data)
        fields[tag] = data[offset:offset+n]
        offset += n
    return txn, msg, fields


def run_case(binary, case):
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    env = os.environ.copy()
    fail_markers = {"log-before-mutation": "LOC SAVED", "log-start-tx": "LOC TX raw=00080022",
                    "log-after-start": "LOC RX raw=02080022", "log-first-set-tx": "LOC TX raw=0004003a"}
    if case in fail_markers:
        env["LOC_TEST_LOG_FAIL"] = fail_markers[case]
    if case == "mask-fix-cancel":
        env["LOC_TEST_CANCEL_FIX"] = "1"
    follow = "follow-" in case
    if follow:
        env["LOC_TEST_FOLLOW"] = "1"
    if case in ("mask-follow-stop", "mask-follow-stop-refused"):
        env["LOC_TEST_CANCEL_FIX"] = "1"
    if case == "mask-follow-sv-cancel":
        env["LOC_TEST_CANCEL_SV"] = "1"
    query = case.endswith("query-only")
    info = case.startswith("info-")
    args = [binary, str(child.fileno())] + (["--receiver-info"] if info else ["--query-only"] if query else [])
    process = subprocess.Popen(args, pass_fds=[child.fileno()], env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    child.close()
    parent.settimeout(4)
    sent = []
    values = {0x3b: 4, 0x3f: 5, 0x4b: 2}
    modern = case.startswith("mask-")
    echo = modern or case == "echo-transaction"
    if modern:
        values[0x3b] = 2 if case == "mask-captured" else 7
    if follow and case != "mask-follow-config":
        values[0x3b] = 2 if modern else 3
        values[0x4b] = 4
    saved = values.copy()
    setters = {0x3a: 0x3b, 0x3e: 0x3f, 0x4a: 0x4b}
    desired = {0x3a: 3, 0x3e: 0xffff, 0x4a: 4}
    if modern:
        desired[0x3a] = (saved[0x3b] & 4) | 2
        desired[0x3e] = saved[0x3f]
    errors = []
    indication_seq = 0

    def position(stamp=1700000000123, status=0, session=1):
        return qmi(4, 77, 0x24, tlv(1, u32(status)) + tlv(2, bytes([session])) +
                   tlv(0x10, struct.pack("<d", 12.25)) + tlv(0x11, struct.pack("<d", -34.5)) +
                   tlv(0x25, struct.pack("<Q", stamp)) + tlv(0x23, u32(1)))

    def satellites(count=2, expanded=False):
        # Independent wire layout per IDL descriptor: mask:u32, system:i32,
        # id:u16, health:u8, status:i32, info:u8, elevation/azimuth/SNR:f32.
        entry = struct.pack("<IIHBIBfff", 0xff, 1, 7, 1, 3, 3, 45.0, 123.0, 37.5)
        if expanded:
            entry += b"\x00"
        return qmi(4, 88, 0x25, tlv(1, b"\x00") +
                   tlv(0x11 if expanded else 0x10, bytes([count]) + entry * count))

    def send(payload=b"", kind=1, node=7, port=4107, client=55):
        nonlocal indication_seq
        if (modern or case == "sequence-legacy") and payload and payload[0] == 4:
            indication_seq += 1
            payload = payload[:1] + struct.pack("<H", indication_seq) + payload[3:]
        parent.send(struct.pack("<6I", kind, node, port, 16, 9, client) + payload)

    def peer():
        try:
            listing = parent.recv(8192)
            assert struct.unpack("<6I", listing) == (10, 0, 0xfffffffe, 16, 0, 55)
            send(kind=4)
            while True:
                packet = parent.recv(8192)
                if not packet:
                    break
                assert struct.unpack_from("<6I", packet) == (1, 7, 4107, 16, 0, 55)
                txn, msg, fields = parse(packet[24:])
                itxn = txn if echo else 0
                sent.append(msg)
                first = sent.count(msg) == 1
                response = qmi(2, txn, msg, tlv(2, struct.pack("<HH", 0, 0)))
                if info:
                    assert msg in (0x32, 0xbf) and not fields, (case, msg, fields)
                    body = tlv(1, u32(0))
                    if msg == 0x32:
                        body += tlv(2, u32(134))
                        if case == "info-short-revision":
                            body = tlv(1, u32(0)) + tlv(2, b"\x86")
                        elif case == "info-missing-revision":
                            body = tlv(1, u32(0))
                        elif case == "info-duplicate":
                            body += tlv(2, u32(134))
                        elif case == "info-string-long":
                            body += tlv(0x10, b"x" * 128)
                        elif case == "info-string-control":
                            body += tlv(0x10, b"bad\nversion")
                        elif case == "info-max-strings":
                            body += tlv(0x10, b"a"*127) + tlv(0x11, b"b"*127) + tlv(0x12, b"c"*255)
                        elif case != "info-optional-absent":
                            body += tlv(0x12, b"MPSS-test-fixture")
                        if case == "info-refused-response":
                            send(qmi(2, txn, msg, tlv(2, struct.pack("<HH", 1, 42))))
                            continue
                        if case == "info-unsupported-response":
                            send(qmi(2, txn, msg, tlv(2, struct.pack("<HH", 1, 94))))
                            continue
                        if case == "info-unsupported-indication":
                            body = tlv(1, u32(2))
                        if case == "info-failed-status":
                            body = tlv(1, u32(5))
                        if case == "info-short-status":
                            body = tlv(1, b"\x00")
                    elif case != "info-optional-absent":
                        body += tlv(0x10, u32(0)) + tlv(0x11, u32(1)) + tlv(0x15, u32(100))
                        if case == "info-short-constellation":
                            body += tlv(0x12, b"\x01")
                        elif case == "info-unknown-enum":
                            body += tlv(0x12, u32(999))
                    body += tlv(0xee, b"unknown extension")  # Framed optional extensions survive.
                    indication = qmi(4, 77, msg, body)
                    if case == "info-response-only":
                        send(response)
                    elif case == "info-indication-only":
                        send(indication)
                    elif case == "info-wrong-transaction":
                        send(qmi(2, txn+1, msg, tlv(2, bytes(4))))
                        send(indication)
                    elif case == "info-indication-first":
                        send(indication); send(response)
                    else:
                        send(response); send(indication)
                    continue
                if case == "unsupported" and msg == 0x3f:
                    send(qmi(2, txn, msg, tlv(2, struct.pack("<HH", 1, 94))))
                    continue
                if case == "get-response-only" and msg == 0x3b:
                    send(response)
                    continue
                if case == "wrong-transaction-only" and msg == 0x3b:
                    send(qmi(2, txn+1, msg, tlv(2, struct.pack("<HH", 0, 0))))
                    continue
                if case == "duplicate-tlv" and msg == 0x3b:
                    send(qmi(2, txn, msg, tlv(2, bytes(4)) * 2))
                    continue
                if case == "short-result" and msg == 0x3b:
                    send(qmi(2, txn, msg, tlv(2, bytes(1))))
                    continue
                if case == "unrelated" and first:
                    send(b"bad", node=99)
                    send(b"bad", client=99)
                    send(qmi(2, txn+1, msg, tlv(2, struct.pack("<HH", 0, 0))))
                    send(qmi(4, 12, 0xee, tlv(1, u32(0))))
                if msg in values:
                    assert not fields
                    field = tlv(0x10, u32(values[msg]))
                    if modern and msg == 0x3b:
                        value = values[msg] | (1 << 40) if case == "mask-unknown-bits" else values[msg]
                        field = tlv(0x12, struct.pack("<Q", value))
                        if case == "mask-short":
                            field = tlv(0x12, u32(value))
                        if case == "mask-both":
                            field = tlv(0x10, u32(1)) + field
                        if case == "mask-wrong-subscription":
                            field += tlv(0x11, u32(1))
                        if case == "mask-restore-schema" and not first:
                            field = tlv(0x10, u32(4))
                    indication = qmi(4, itxn, msg, tlv(1, u32(0)) + field)
                    if case == "mask-captured" and msg == 0x3b:
                        # Exact device-3 session1 completion, not a generated surrogate.
                        assert txn == 1
                        indication = bytes.fromhex("0401003b001200010400000000001208000200000000000000")
                    if case == "wrong-indication-only" and msg == 0x3b:
                        indication = qmi(4, txn+1, 0xee, tlv(1, u32(0)) + field)
                    if case == "indication-first":
                        send(indication); send(response)
                    else:
                        send(response); send(indication)
                elif msg in setters:
                    assert not (modern and msg == 0x3e), "native positions must not change NMEA configuration"
                    if modern and msg == 0x3a:
                        assert fields.keys() == {1, 0x11} and fields[1] == u32(3)
                        value = struct.unpack("<Q", fields[0x11])[0]
                    else:
                        assert fields.keys() == {1} and len(fields[1]) == 4
                        value = struct.unpack("<I", fields[1])[0]
                    assert value == (desired[msg] if first else saved[setters[msg]])
                    if case == "locked" and msg == 0x3a and first:
                        send(response); send(qmi(4, 0, msg, tlv(1, u32(1))))
                        continue
                    if case == "restore-failure" and msg == 0x4a and not first:
                        send(response); send(qmi(4, 0, msg, tlv(1, u32(1))))
                        continue
                    if case == "restore-readback-mismatch" and msg == 0x4a and not first:
                        send(response); send(qmi(4, 0, msg, tlv(1, u32(0))))
                        continue
                    values[setters[msg]] = value
                    send(response)
                    if not (case == "set-response-only" and msg == 0x3e and first):
                        send(qmi(4, itxn, msg, tlv(1, u32(0))))
                elif msg == 0x21:
                    expected = {1: struct.pack("<Q", (0x83 if modern else 0x87) if first else 0)}
                    if modern:
                        expected[0x11] = u32(1)  # AFW, never privileged or default NFW.
                    assert fields == expected, (case, fields)
                    if case == "unregister-failure" and not first:
                        send(qmi(2, txn, msg, tlv(2, struct.pack("<HH", 1, 3))))
                    else:
                        send(response)
                elif msg == 0x22:
                    assert fields == {1: b"\x01", 0x10: u32(1), 0x11: u32(3), 0x13: u32(1500 if follow else 1000)}, (case, fields)
                    if "follow-sv" in case:
                        send(satellites(9))  # Before Start ack: never delivered.
                        send(qmi(4, 88, 0x2b, tlv(1, u32(99))))
                    if case.startswith("mask-fix-"):
                        send(position(stamp=1700000000900))  # Before Start acknowledgement: no callback.
                    if case == "start-timeout":
                        continue
                    if case == "signal":
                        os.kill(process.pid, signal.SIGTERM)
                        continue
                    send(response)
                    if "follow-sv" in case:
                        send(qmi(4, 88, 0x2b, tlv(1, u32(1))))
                        if case.endswith("bad-engine"):
                            send(qmi(4, 88, 0x2b, tlv(1, b"\x01")))
                            continue
                        if case.endswith("bad-list"):
                            send(qmi(4, 88, 0x25, tlv(1, b"\x00") + tlv(0x10, b"\x01")))
                            continue
                        send(satellites())
                        send(qmi(4, 88, 0x2b, tlv(1, u32(0xffffffff))))
                        send(satellites(176, expanded=True))  # >4096 bytes, real transport seam.
                        send(qmi(4, 88, 0x25, tlv(1, b"\x01")))  # absent != empty
                        send(satellites(0))
                        time.sleep(0.1)
                        os.kill(process.pid, signal.SIGTERM)
                        continue
                    if follow:
                        if case == "mask-follow-disappear":
                            send(kind=5)
                        elif case == "mask-follow-idle":
                            time.sleep(0.25)  # No 70ms diagnostic timeout in follow.
                            os.kill(process.pid, signal.SIGTERM)
                        else:
                            send(position())
                        continue
                    if case.startswith("mask-fix-"):
                        send(position(status=1))
                        send(position(session=2))
                        send(position())
                        send(position())  # Duplicate timestamp.
                        send(position(stamp=1700000000122))  # Out of order.
                        send(position(stamp=1700000000124))
                        continue
                    if case == "disappearance":
                        send(kind=5)
                        continue
                    sentence = b"GPGGA,0,0,N,0,E,0,00"
                    checksum = 0
                    for c in sentence:
                        checksum ^= c
                    nmea = b"$" + sentence + b"*%02X\r\n" % checksum
                    if case == "malformed-nmea":
                        nmea = b"$invalid*00\r\n"
                    event = qmi(4, 0, 0x26, tlv(1, nmea))
                    if modern:
                        event = qmi(4, 0, 0x24, tlv(1, u32(1)) + tlv(2, b"\x01"))
                    if case == "tagged-event":
                        # A modem event sequence is not a request transaction.
                        event = qmi(4, txn, 0x26, tlv(1, nmea))
                    if case == "malformed-qmi":
                        event = event[:-1]
                    if case == "truncated-header":
                        event = b"\x04\x00"
                    send(event)
                elif msg == 0x23:
                    assert fields == {1: b"\x01"}
                    if "follow-sv" in case:
                        send(satellites(8))  # During Stop: no callback.
                        send(qmi(4, 88, 0x25, tlv(1, b"\x00")))
                        send(qmi(4, 88, 0x2b, tlv(1, u32(98))))
                    if case.startswith("mask-fix-"):
                        send(position(stamp=1700000000125))  # Draining Stop: no callback.
                    if case in ("stop-failure", "mask-follow-stop-refused"):
                        send(qmi(2, txn, msg, tlv(2, struct.pack("<HH", 1, 3))))
                    else:
                        send(response)
                else:
                    raise AssertionError(f"unexpected request {msg:x}")
        except (BrokenPipeError, ConnectionResetError):
            pass  # client may close after rejecting injected packets
        except Exception as exc:
            errors.append(exc)
        finally:
            parent.close()

    thread = threading.Thread(target=peer)
    thread.start()
    try:
        stdout, stderr = process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate()
        raise AssertionError(f"{case}: unbounded client")
    thread.join(5)
    assert not thread.is_alive() and not errors, (case, errors)
    if info:
        success = case in ("info-success", "info-indication-first", "info-optional-absent",
                           "info-unknown-enum", "info-max-strings")
        partial = case in ("info-unsupported-response", "info-unsupported-indication")
        assert (process.returncode == 0) == success, (case, stdout, stderr)
        assert sent == ([0x32, 0xbf] if success or partial or case == "info-short-constellation" else [0x32]), (case, sent)
        assert "cleanup=CONFIRMED" in stdout
        if success or partial:
            assert "outcome=RECEIVER_INFO" in stdout
        else:
            assert "outcome=FAIL" in stdout
        if partial:
            assert "outcome=RECEIVER_INFO_PARTIAL" in stdout and "unsupported=" in stdout
            assert "LOC RECEIVER revision=" not in stdout
        if case in ("info-response-only", "info-indication-only", "info-wrong-transaction"):
            assert "timeout" in stdout
            assert "LOC RECEIVER revision=" not in stdout  # No facts without BOTH completions.
        if case == "info-optional-absent":
            assert "LOC CONSTELLATION GPS=unavailable" in stdout
            assert "gnss-software=unavailable" in stdout
        if case == "info-unknown-enum":
            assert "LOC CONSTELLATION BDS=unknown raw=999" in stdout
        if case == "info-success":
            assert 'LOC RECEIVER gnss-software="MPSS-test-fixture"' in stdout
            assert "LOC CONSTELLATION GPS=enabled-mandatory raw=0" in stdout
            assert "LOC CONSTELLATION NavIC=unsupported raw=100" in stdout
        print(f"PASS {case}: read-only requests={[hex(x) for x in sent]}")
        print(stdout, end="")
        return
    success = case in ("mask-follow-sv-flow", "mask-follow-sv-cancel", "mask-follow-stop", "mask-follow-idle", "mask-fix-flow", "success", "unrelated", "indication-first", "echo-transaction",
                       "mask-captured", "mask-restore", "mask-both", "query-only", "tagged-event",
                       "sequence-legacy", "mask-query-only")
    assert (process.returncode == 0) == success, (case, process.returncode, stdout, stderr)
    if "follow-sv" in case:
        callbacks = [line for line in stdout.splitlines() if line.startswith("CALLBACK satellites=")]
        expected = [] if "bad-" in case else ["CALLBACK satellites=2"]
        metadata = [] if "bad-" in case else ["CALLBACK sv-metadata present=1 count=2"]
        engines = ["CALLBACK engine-raw=1"]
        if case.endswith("flow"):
            metadata += ["CALLBACK sv-metadata present=1 count=176",
                         "CALLBACK sv-metadata present=0 count=0",
                         "CALLBACK sv-metadata present=1 count=0"]
            engines += ["CALLBACK engine-raw=4294967295"]
            expected += ["CALLBACK satellites=176", "CALLBACK satellites=0"]
            assert "LOC SV list=absent" in stdout
            assert "LOC SV count=0 searching=0 tracked=0 cn0-valid=0 max-cn0=unavailable" in stdout
        assert callbacks == expected, (case, callbacks, stdout)
        assert [line for line in stdout.splitlines() if line.startswith("CALLBACK sv-metadata")] == metadata, stdout
        assert [line for line in stdout.splitlines() if line.startswith("CALLBACK engine-raw")] == engines, stdout
        if "bad-" not in case:
            assert "LOC SV count=2 searching=0 tracked=2 cn0-valid=2 max-cn0=37.5" in stdout
        else:
            assert process.returncode == 1 and "cleanup=CONFIRMED" in stdout, stdout
    if follow:
        assert not any(msg in setters for msg in sent), (case, sent)
        if case in ("mask-follow-config", "follow-legacy"):
            assert sent == [0x3b, 0x3f, 0x4b] and "provider-configuration-not-ready" in stdout
        else:
            assert "CALLBACK started" in stdout, (case, stdout)
        if success:
            assert "outcome=STOPPED" in stdout and "cleanup=CONFIRMED" in stdout
        if case in ("mask-follow-stop-refused", "mask-follow-disappear"):
            assert "cleanup=UNCONFIRMED" in stdout
            assert process.returncode == 2, (case, process.returncode)
    if case.startswith("mask-fix-"):
        callbacks = [line for line in stdout.splitlines() if line.startswith("CALLBACK")]
        expected = ["CALLBACK fix=1 utc=1700000000123"]
        if case == "mask-fix-flow":
            expected.append("CALLBACK fix=2 utc=1700000000124")
        assert callbacks == expected, (case, callbacks, stdout)
        assert "cleanup=CONFIRMED" in stdout, (case, stdout)
    assert "port=4107" in stdout, (case, stdout)
    if case not in fail_markers:
        assert "LOC CLOSED" in stdout, (case, stdout)
    if case in ("log-before-mutation", "log-first-set-tx"):
        assert not any(msg in setters or msg in (0x21, 0x22) for msg in sent), (case, sent)
    if case == "log-start-tx":
        assert 0x22 not in sent and 0x23 not in sent and sent.count(0x21) == 2, (case, sent)
    if case == "log-after-start":
        assert 0x23 in sent and all(sent.count(msg) == 2 for msg in setters), (case, sent)
    if case in ("unsupported", "get-response-only", "wrong-transaction-only", "duplicate-tlv", "short-result"):
        assert not any(msg in setters or msg in (0x21, 0x22) for msg in sent), (case, sent)
    if case in ("wrong-indication-only", "mask-unknown-bits", "mask-short", "mask-wrong-subscription"):
        assert not any(msg in setters or msg in (0x21, 0x22) for msg in sent), (case, sent)
    if case == "mask-captured":
        assert 0x3a not in sent, (case, sent)  # Already NFW-only locked; never alter it.
    if modern:
        assert 0x3e not in sent, (case, sent)
    if case == "mask-restore-schema":
        assert "cleanup=UNCONFIRMED" in stdout, (case, stdout)
    if case == "tagged-event":
        assert "LOC NMEA " in stdout, (case, stdout)
    if query:
        assert sent == [0x3b, 0x3f, 0x4b] and "outcome=QUERY" in stdout, (case, sent, stdout)
    if case in ("locked", "set-response-only"):
        assert 0x22 not in sent, (case, sent)
    if 0x22 in sent and case not in ("disappearance", "mask-follow-disappear"):
        assert 0x23 in sent and sent.count(0x21) == 2, (case, sent)
    if case not in ("restore-failure", "restore-readback-mismatch", "disappearance"):
        assert values == saved, (case, values)
    if case in ("restore-failure", "restore-readback-mismatch", "stop-failure", "unregister-failure", "disappearance"):
        assert "cleanup=UNCONFIRMED" in stdout, (case, stdout)
    print(f"PASS {case}: requests={[hex(x) for x in sent]}")
    print(stdout, end="")


cases = ("info-success", "info-indication-first", "info-optional-absent", "info-unknown-enum",
         "info-max-strings", "info-refused-response", "info-unsupported-response", "info-unsupported-indication",
         "info-short-revision", "info-missing-revision", "info-duplicate", "info-string-long",
         "info-string-control", "info-short-constellation", "info-failed-status", "info-short-status",
         "info-response-only", "info-indication-only", "info-wrong-transaction", "mask-follow-sv-flow", "mask-follow-sv-cancel", "mask-follow-sv-bad-list", "mask-follow-sv-bad-engine", "mask-follow-stop", "mask-follow-idle", "mask-follow-stop-refused", "mask-follow-config", "mask-follow-disappear", "follow-legacy",
         "mask-fix-flow", "mask-fix-cancel", "sequence-legacy", "mask-query-only", "mask-captured", "echo-transaction", "mask-restore", "mask-both", "mask-unknown-bits",
         "mask-short", "mask-wrong-subscription", "mask-restore-schema", "wrong-indication-only", "tagged-event",
         "query-only", "success", "indication-first", "unrelated", "unsupported", "get-response-only",
             "set-response-only", "wrong-transaction-only", "locked", "malformed-nmea",
             "malformed-qmi", "truncated-header", "disappearance", "start-timeout", "signal",
             "restore-failure", "stop-failure", "duplicate-tlv", "short-result",
             "restore-readback-mismatch", "unregister-failure", "log-before-mutation",
             "log-first-set-tx", "log-start-tx", "log-after-start")
for case in cases:
    run_case(sys.argv[1], case)
print(f"PASS: {len(cases)} AF_UNIX peer scenarios; no device result")
