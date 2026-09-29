#!/usr/bin/env python3
"""Real LOC client/socket replay for optional UTC aiding; never opens QRTR."""
import os
import signal
import socket
import struct
import subprocess
import sys


def tlv(tag, data):
    return struct.pack('<BH', tag, len(data)) + data


def message(kind, txn, msg, body=b''):
    return struct.pack('<BHHH', kind, txn, msg, len(body)) + body


def run(binary, case):
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    parent.settimeout(4)
    env = dict(os.environ, LOC_TEST_FOLLOW='1', LOC_TEST_TIME=case)
    process = subprocess.Popen([binary, str(child.fileno())], pass_fds=(child.fileno(),),
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env)
    child.close()
    sent = []
    try:
        def send(kind, txn, msg, body=b'', node=7):
            parent.send(struct.pack('<6I', 1, node, 77, 16, 2, 55) + message(kind, txn, msg, body))
        def response(txn, msg, error=0):
            send(2, txn, msg, tlv(2, struct.pack('<HH', bool(error), error)))
        def indication(msg, status=0, extra=b''):
            send(4, 900, msg, tlv(1, struct.pack('<I', status)) + extra)
        first = parent.recv(20000)
        assert struct.unpack('<6I', first) == (10, 0, 0xfffffffe, 16, 0, 55), first.hex()
        parent.send(struct.pack('<6I', 4, 7, 77, 16, 2, 55))
        injected = False
        while True:
            data = parent.recv(20000)
            if not data:
                break
            kind, txn, msg, size = struct.unpack('<BHHH', data[24:31])
            body = data[31:]
            assert kind == 0 and size == len(body)
            sent.append(msg)
            if msg in (0x3b, 0x3f, 0x4b):
                response(txn, msg)
                extra = tlv(0x12, struct.pack('<Q', 2)) if msg == 0x3b else tlv(0x10, struct.pack('<I', 0 if msg == 0x3f else 4))
                indication(msg, extra=extra)
            elif msg == 0x21:
                fields = {body[0]: body[3:11]}
                mask = struct.unpack('<Q', fields[1])[0]
                assert mask == (0 if 0x23 in sent else 0x93), f'time request mask missing: {mask:#x}'
                if not (case == 'transport-failure' and injected):
                    response(txn, msg)
                if injected:
                    break
            elif msg == 0x22:
                # Event while another command awaits its response must route.
                send(4, 901, 0x28)
                response(txn, msg)
            elif msg == 0x38:
                assert case != 'expired', 'invalid queued sample was transmitted'
                assert not injected, 'unbounded/repeated injection'
                injected = True
                assert body[0:3] == b'\x01\x08\x00' and body[11:14] == b'\x02\x04\x00' and len(body) == 18, body.hex()
                utc = struct.unpack('<Q', body[3:11])[0]
                uncertainty = struct.unpack('<I', body[14:18])[0]
                assert 1700000005000 <= utc < 1700000008000, utc
                assert uncertainty == 27
                if case == 'response-rejected':
                    response(txn, msg, 94); send(4, 903, 0x28)
                elif case == 'indication-rejected':
                    response(txn, msg); indication(msg, 2); send(4, 903, 0x28)
                elif case == 'service-gone':
                    parent.send(struct.pack('<6I', 5, 7, 77, 16, 2, 55))
                elif case == 'transport-failure':
                    parent.shutdown(socket.SHUT_WR)
                elif case == 'cancel':
                    process.send_signal(signal.SIGTERM)
                elif case == 'malformed':
                    response(txn, msg); send(4, 902, msg, tlv(1, b'\0'))
                else:
                    # Unrelated peer/transaction cannot satisfy completion.
                    send(2, txn, msg, tlv(2, b'\0'*4), node=8)
                    response(txn + 1, msg)
                    if case == 'indication-first':
                        indication(msg); response(txn, msg)
                    else:
                        response(txn, msg); indication(msg)
            elif msg == 0x23:
                if case != 'transport-failure':
                    response(txn, msg)
            else:
                raise AssertionError(f'unexpected command {msg:#x}')
        output = process.communicate(timeout=4)[0].decode()
        assert process.returncode == (2 if case in ('service-gone', 'transport-failure') else 1 if case == 'malformed' else 0), (case, process.returncode, output)
        assert (0x38 in sent) == (case != 'expired') and (case == 'service-gone' or sent[-2:] == [0x23, 0x21]), (sent, output)
        assert output.count('CALLBACK time-request') >= 2, output
        if case in ('response-rejected', 'indication-rejected'):
            assert 'CALLBACK time-result=1' in output and 'CALLBACK standalone-after-refusal' in output, output
        elif case == 'expired':
            assert 'CALLBACK time-result=2' in output, output
        elif case not in ('cancel', 'malformed', 'service-gone', 'transport-failure'):
            assert 'CALLBACK time-result=0' in output, output
        print('PASS UTC protocol', case)
    finally:
        parent.close()
        if process.poll() is None:
            process.kill(); process.communicate()


def run_continuity(binary, case):
    """Lost optional UTC completion must not remove an active position stream."""
    import select
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    parent.settimeout(4)
    process = subprocess.Popen([binary, str(child.fileno())], pass_fds=(child.fileno(),),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            env=dict(os.environ, LOC_TEST_FOLLOW='1', LOC_TEST_TIME='continuity',
                     LOC_TEST_CANCEL_TWO_FIXES='1'))
    child.close()
    sent, utc_txn = [], None
    bad = case.startswith('late-malformed')
    try:
        def send(kind, txn, msg, body=b''):
            parent.send(struct.pack('<6I', 1, 7, 77, 16, 2, 55) + message(kind, txn, msg, body))
        def response(txn, msg):
            send(2, txn, msg, tlv(2, struct.pack('<HH', 0, 0)))
        def indication(msg, body=b''):
            send(4, 900, msg, tlv(1, struct.pack('<I', 0)) + body)
        def fix(stamp):
            send(4, 901, 0x24, tlv(1, struct.pack('<I', 0)) + tlv(2, b'\x01') +
                 tlv(0x10, struct.pack('<d', 12.25)) + tlv(0x11, struct.pack('<d', -34.5)) +
                 tlv(0x23, struct.pack('<I', 1)) + tlv(0x25, struct.pack('<Q', stamp)))
        assert struct.unpack('<6I', parent.recv(20000)) == (10, 0, 0xfffffffe, 16, 0, 55)
        parent.send(struct.pack('<6I', 4, 7, 77, 16, 2, 55))
        while True:
            data = parent.recv(20000)
            if not data:
                break
            kind, txn, msg, size = struct.unpack('<BHHH', data[24:31])
            body = data[31:]
            assert kind == 0 and size == len(body)
            sent.append(msg)
            if msg in (0x3b, 0x3f, 0x4b):
                response(txn, msg)
                extra = tlv(0x12, struct.pack('<Q', 2)) if msg == 0x3b else tlv(0x10, struct.pack('<I', 0 if msg == 0x3f else 4))
                indication(msg, extra)
            elif msg == 0x21:
                assert struct.unpack('<Q', body[3:11])[0] == (0 if 0x23 in sent else 0x93)
                if case == 'late-during-unregister' and 0x23 in sent:
                    response(utc_txn, 0x38)
                    indication(0x38)
                response(txn, msg)
                if 0x23 in sent:
                    break
            elif msg == 0x22:
                response(txn, msg)
            elif msg == 0x38:
                assert utc_txn is None, 'UTC must stay disabled after ambiguous completion'
                utc_txn = txn
                if case in ('response-only', 'late-indication'):
                    response(txn, msg)
                elif case in ('indication-only', 'late-response'):
                    indication(msg)
                fix(1700000000123)  # Native events must flow even during the UTC wait.
                # Regression signal: old code sends Stop at the 2s timeout.
                assert not select.select([parent], [], [], 2.2)[0], 'UTC timeout stopped/re-injected into active session'
                send(4, 902, 0x28)  # Another request cannot re-enable UTC.
                if case == 'late-indication':
                    indication(0x38)
                elif case == 'late-response':
                    response(utc_txn, 0x38)
                elif case == 'wrong-transaction':
                    response(utc_txn + 100, 0x38)
                    response(utc_txn, 0x38)
                    indication(0x38)
                elif case == 'late-malformed-response':
                    send(2, utc_txn, 0x38, tlv(2, b'\x00'))
                elif case == 'late-malformed-indication':
                    send(4, 903, 0x38, tlv(1, b'\x00'))
                elif case == 'late-malformed-frame':
                    send(4, 903, 0x38, b'\x01\x04')
                if not bad:
                    entry = struct.pack('<IIHBIBfff', 255, 1, 7, 1, 3, 3, 45.0, 123.0, 37.5)
                    send(4, 904, 0x25, tlv(1, b'\x00') + tlv(0x10, b'\x01' + entry))
                    fix(1700000000124)  # Second callback cancels via observer, not wall time.
            elif msg == 0x23:
                if case == 'late-during-stop':
                    response(utc_txn, 0x38)
                    indication(0x38)
                if case == 'stop-refused':
                    send(2, txn, msg, tlv(2, struct.pack('<HH', 1, 3)))
                elif case != 'stop-timeout':
                    response(txn, msg)
            else:
                raise AssertionError(f'unexpected command {msg:#x}')
        output = process.communicate(timeout=4)[0].decode()
        assert process.returncode == (2 if case in ('stop-refused', 'stop-timeout') else 1 if bad else 0), (case, output)
        assert sent == [0x3b, 0x3f, 0x4b, 0x21, 0x22, 0x38, 0x23, 0x21], (sent, output)
        assert 'CALLBACK time-result=3' in output, output
        assert ('cleanup=UNCONFIRMED' if case in ('stop-refused', 'stop-timeout') else 'cleanup=CONFIRMED') in output, output
        assert 'CALLBACK fix=1 utc=1700000000123' in output, output
        if not bad:
            assert 'CALLBACK fix=2 utc=1700000000124' in output, output
            assert 'CALLBACK satellites=1' in output, output
        else:
            assert 'CALLBACK fix=2' not in output, output
        if case in ('late-indication', 'late-response', 'wrong-transaction', 'late-during-stop', 'late-during-unregister'):
            assert 'LOC TIME late-completion ignored' in output, output
        assert output.count('CALLBACK time-request') == 1, output
        print('PASS UTC continuity', case)
    finally:
        parent.close()
        if process.poll() is None:
            process.kill(); process.communicate()


if __name__ == '__main__':
    for scenario in ('accepted', 'indication-first', 'response-rejected', 'indication-rejected', 'cancel', 'malformed', 'service-gone', 'transport-failure', 'expired'):
        run(sys.argv[1], scenario)

    for scenario in ('response-only', 'indication-only', 'neither', 'late-indication', 'late-response',
                     'wrong-transaction', 'late-during-stop', 'late-during-unregister', 'stop-refused',
                     'stop-timeout', 'late-malformed-response',
                     'late-malformed-indication', 'late-malformed-frame'):
        run_continuity(sys.argv[1], scenario)
