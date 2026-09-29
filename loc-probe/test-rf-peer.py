#!/usr/bin/env python3
"""Independent packed LOC RF wire fixtures; AF_UNIX only, never QRTR."""
import os
import signal
import socket
import struct
import subprocess
import sys
import tempfile


def tlv(tag, value):
    return struct.pack('<BH', tag, len(value)) + value


def u32(value):
    return struct.pack('<I', value)


def run(binary, case):
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    parent.settimeout(3)
    with tempfile.TemporaryFile() as log:
        args = [binary, str(child.fileno())] + ([] if case == 'default' else ['--rf-metrics'])
        process = subprocess.Popen(args, pass_fds=(child.fileno(),), stdout=log, stderr=log)
        child.close()
        sent, registrations = [], []
        try:
            def send(kind, txn, msg, body=b'', node=7):
                qmi = struct.pack('<BHHH', kind, txn, msg, len(body)) + body
                parent.send(struct.pack('<6I', 1, node, 4107, 16, 9, 55) + qmi)

            def response(txn, msg, error=0):
                send(2, txn, msg, tlv(2, struct.pack('<HH', bool(error), error)))

            def indication(msg, body):
                send(4, 987, msg, body)

            def rf(tick=0, stats=True):
                body = tlv(1, struct.pack('<Q', tick)) + tlv(2, struct.pack('<i', -12))
                if stats:
                    body += tlv(0x10, struct.pack('<HHi', 0, 65535, -18))
                return body

            assert struct.unpack('<6I', parent.recv(20000)) == (10, 0, 0xfffffffe, 16, 0, 55)
            parent.send(struct.pack('<6I', 4, 7, 4107, 16, 9, 55))
            while True:
                data = parent.recv(20000)
                if not data:
                    break
                assert struct.unpack_from('<6I', data) == (1, 7, 4107, 16, 0, 55)
                kind, txn, msg, size = struct.unpack('<BHHH', data[24:31])
                body = data[31:]
                assert kind == 0 and size == len(body)
                sent.append(msg)
                if msg in (0x3b, 0x3f, 0x4b):
                    assert body == b''
                    response(txn, msg)
                    if msg == 0x3b:
                        extra = tlv(0x10, u32(3)) if case == 'legacy' else tlv(0x12, struct.pack('<Q', 2))
                    else:
                        extra = tlv(0x10, u32(131072 if msg == 0x3f else 2 if case == 'wrong-mode' else 4))
                    indication(msg, tlv(1, u32(0)) + extra)
                elif msg == 0x21:
                    mask = struct.unpack('<Q', body[3:11])[0]
                    registrations.append(mask)
                    assert body == tlv(1, struct.pack('<Q', mask)) + tlv(0x11, u32(1))
                    if len(registrations) == 1:
                        assert mask == (0x83 if case == 'default' else 0x40000000083), hex(mask)
                        if case == 'register-unsupported':
                            response(txn, msg, 94)
                            continue
                        if case == 'register-invalid':
                            response(txn, msg, 48)
                            continue
                        if case == 'register-timeout':
                            continue
                    else:
                        assert mask == 0
                    response(txn, msg)
                elif msg == 0x22:
                    assert body == tlv(1, b'\1') + tlv(0x10, u32(1)) + tlv(0x11, u32(3)) + tlv(0x13, u32(1000))
                    if case == 'order':
                        indication(0xce, rf(999))  # cannot count before Start ACK
                    response(txn, msg)
                    if case == 'default':
                        indication(0xce, b'')  # opt-in decoder must stay off
                        indication(0x25, tlv(1, b'\0'))
                elif msg == 0x33:
                    assert not body and case != 'default'
                    if case == 'criteria-response-refused':
                        response(txn, msg, 94)
                    else:
                        criteria = tlv(1, u32(73 if case == 'criteria-status-unknown' else 2 if case == 'criteria-unsupported' else 0))
                        if case not in ('criteria-absent', 'criteria-unsupported', 'criteria-status-unknown'):
                            criteria += tlv(0x10, u32(999 if case == 'criteria-enum-unknown' else 3))
                            criteria += tlv(0x11, u32(1)) + tlv(0x12, u32(1000))
                            criteria += tlv(0x14, u32(1)) + tlv(0x15, u32(0)) + tlv(0x16, u32(255000))
                            criteria += tlv(0x17, b'\0') + tlv(0x18, struct.pack('<iI', 2, 0))
                        if case == 'criteria-short':
                            criteria = tlv(1, u32(0)) + tlv(0x18, b'\0')
                        if case == 'criteria-bad-status':
                            criteria = tlv(1, b'\0')
                        if case == 'criteria-duplicate':
                            criteria += tlv(1, u32(0))
                        if case == 'criteria-application':
                            criteria += tlv(0x13, b'\x18' + b'a'*24 + b'\x20' + b'b'*32 + b'\x01\x08' + b'c'*8)
                        if case == 'criteria-bad-application':
                            criteria += tlv(0x13, b'\0\0\2\0')
                        if case == 'criteria-unknown-tlv':
                            criteria += tlv(0xee, b'unknown')
                        if case in ('order', 'criteria-indication-only'):
                            indication(msg, criteria)
                            if case != 'criteria-indication-only':
                                indication(0xce, rf(0))  # active report during paired command
                                response(txn, msg)
                        elif case == 'criteria-wrong-transaction':
                            response(txn + 1, msg)
                            indication(msg, criteria)
                        elif case == 'criteria-wrong-source':
                            response(txn, msg)
                            send(4, 999, msg, criteria, node=8)
                        else:
                            response(txn, msg)
                            if case != 'criteria-response-only':
                                indication(msg, criteria)
                    if case.startswith('criteria-') and case in (
                            'criteria-short', 'criteria-bad-status', 'criteria-duplicate',
                            'criteria-bad-application', 'criteria-response-only',
                            'criteria-indication-only', 'criteria-wrong-transaction', 'criteria-wrong-source'):
                        continue
                    if case == 'cancel':
                        process.send_signal(signal.SIGTERM)
                        continue
                    if case == 'service-gone':
                        parent.send(struct.pack('<6I', 5, 7, 4107, 16, 9, 55))
                        continue
                    if case == 'no-events':
                        continue
                    # Absent RF reports are still not zero RF; altitude-only is
                    # enough to prove active session traffic, not reception.
                    indication(0x25, tlv(1, b'\0'))
                    if case == 'no-rf':
                        continue
                    body = rf(1 if case == 'order' else 0, stats=case != 'optional-absent')
                    if case == 'empty':
                        body = b''
                    elif case == 'missing-tick':
                        body = tlv(2, u32(0))
                    elif case == 'short-tick':
                        body = tlv(1, b'\0') + tlv(2, u32(0))
                    elif case == 'short-gain':
                        body = tlv(1, bytes(8)) + tlv(2, b'\0')
                    elif case == 'short-stat':
                        body = rf(stats=False) + tlv(0x10, bytes(7))
                    elif case == 'duplicate':
                        body += tlv(1, bytes(8))
                    elif case == 'truncated':
                        body += b'\xee\x08\x00short'
                    elif case == 'unknown-tlv':
                        body += tlv(0xee, b'unknown')
                    elif case == 'all-stats':
                        body = rf(stats=False)
                        for tag in range(0x10, 0x1a):
                            body += tlv(tag, struct.pack('<i', -2147483648) if tag == 0x17 else struct.pack('<HHi', 65535, 0, 2147483647))
                        assert len(body) == 124
                    indication(0xce, body)
                    if case == 'tick-unchanged':
                        indication(0xce, body)
                    elif case == 'cap':
                        for tick in range(1, 270):
                            indication(0xce, rf(tick))
                    elif case == 'wrong-source':
                        send(4, 999, 0xce, rf(999), node=8)
                elif msg == 0x23:
                    assert body == tlv(1, b'\1')
                    if case == 'order':
                        indication(0xce, rf(999))  # cannot count during cleanup
                    response(txn, msg, 42 if case == 'stop-refused' else 0)
                else:
                    raise AssertionError(f'Unexpected command / global Set {msg:#x}')
            process.wait(timeout=3)
            log.seek(0)
            output = log.read().decode()
            failed = case in {
                'legacy', 'wrong-mode', 'register-unsupported', 'register-invalid', 'register-timeout',
                'criteria-response-refused', 'criteria-status-unknown', 'criteria-unsupported',
                'criteria-short', 'criteria-bad-status', 'criteria-duplicate', 'criteria-bad-application',
                'criteria-response-only', 'criteria-indication-only', 'criteria-wrong-transaction',
                'criteria-wrong-source', 'cancel', 'no-events', 'service-gone', 'stop-refused',
                'empty', 'missing-tick', 'short-tick', 'short-gain', 'short-stat', 'duplicate', 'truncated'}
            assert process.returncode == int(failed), (case, process.returncode, output)
            if case in ('legacy', 'wrong-mode'):
                assert sent == [0x3b, 0x3f, 0x4b], sent
            elif case.startswith('register-'):
                assert sent == [0x3b, 0x3f, 0x4b, 0x21, 0x21], sent
            elif case == 'service-gone':
                assert 'cleanup=UNCONFIRMED' in output, output
            else:
                assert sent == [0x3b, 0x3f, 0x4b, 0x21, 0x22] + ([] if case == 'default' else [0x33]) + [0x23, 0x21], sent
            if case not in ('service-gone', 'stop-refused'):
                assert 'cleanup=CONFIRMED' in output, output
            if case == 'default':
                assert 'LOC RF' not in output and 'LOC CRITERIA' not in output, output
            elif case == 'no-rf':
                assert 'reports=0 capped=no observation=unavailable criteria=accepted' in output, output
            elif case == 'optional-absent':
                assert 'gps-l1=unavailable' in output and 'first-tick=0 last-tick=0' in output, output
            elif case == 'order':
                assert output.count('LOC RF tick=') == 2 and 'first-tick=0 last-tick=1 tick-changed=yes' in output, output
                assert 'LOC RF tick=999' not in output, output
            elif case == 'cap':
                assert output.count('LOC RF tick=') == 256 and 'reports=256 capped=yes' in output, output
                assert output.count('LOC RX raw=04db03ce00') == 256, output
                assert 'last-tick=269 tick-changed=yes' in output, output
            elif case == 'tick-unchanged':
                assert 'reports=2 capped=no' in output and 'tick-changed=no' in output, output
            elif case == 'all-stats':
                assert 'band5-pga-db=-2147483648' in output and 'jammer-db:2147483647' in output, output
            elif case == 'criteria-absent':
                assert 'accuracy-raw=unavailable' in output and 'power-mode-raw=unavailable' in output, output
            elif case == 'criteria-enum-unknown':
                assert 'accuracy-raw=999' in output, output
            elif case == 'criteria-application':
                assert 'application-id=present' in output, output
            elif case in ('criteria-unknown-tlv', 'unknown-tlv'):
                assert 'unknown-tlv=ee/len7' in output, output
            if case in ('criteria-response-refused', 'criteria-status-unknown', 'criteria-unsupported'):
                assert 'criteria=refused' in output and 'RF_DIAGNOSTIC_PARTIAL' in output and 'LOC RF tick=' in output, output
            elif case.startswith('criteria-') and failed:
                assert 'criteria=failed' in output and 'LOC CRITERIA accepted' not in output, output
            if case == 'accepted':
                assert 'gps-l1=i-raw:0,q-raw:65535,jammer-db:-18' in output, output
                assert 'accuracy-raw=3' in output and 'power-mode-raw=2 tbm-ms=0' in output, output
            print('PASS RF protocol', case)
        finally:
            parent.close()
            if process.poll() is None:
                process.kill()
                process.wait()


if __name__ == '__main__':
    for scenario in ('accepted', 'default', 'legacy', 'wrong-mode', 'order', 'optional-absent',
                     'all-stats', 'unknown-tlv', 'no-rf', 'no-events', 'empty', 'missing-tick',
                     'short-tick', 'short-gain', 'short-stat', 'duplicate', 'truncated',
                     'tick-unchanged', 'cap', 'wrong-source', 'cancel', 'service-gone', 'stop-refused',
                     'register-unsupported', 'register-invalid', 'register-timeout',
                     'criteria-response-refused', 'criteria-unsupported', 'criteria-status-unknown',
                     'criteria-short', 'criteria-bad-status', 'criteria-duplicate',
                     'criteria-bad-application', 'criteria-response-only', 'criteria-indication-only',
                     'criteria-wrong-transaction', 'criteria-wrong-source', 'criteria-absent',
                     'criteria-enum-unknown', 'criteria-unknown-tlv', 'criteria-application'):
        run(sys.argv[1], scenario)
