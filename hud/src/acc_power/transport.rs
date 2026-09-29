// SPDX-License-Identifier: Apache-2.0
//! Linux/Bionic connected SEQPACKET I/O. All descriptors have explicit owners.

use super::codec::{Nonce, Packet, LIMIT};
use std::ffi::{c_long, c_void};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const SOL_SOCKET: i32 = 1;
const SO_TYPE: i32 = 3;
const SO_PEERCRED: i32 = 17;
const SO_PEERSEC: i32 = 31;
const SCM_RIGHTS: i32 = 1;
const SCM_PIDFD: i32 = 4;
const MSG_TRUNC: i32 = 0x20;
const MSG_CTRUNC: i32 = 8;
const MSG_DONTWAIT: i32 = 0x40;
const MSG_NOSIGNAL: i32 = 0x4000;
const MSG_CMSG_CLOEXEC: i32 = 0x40000000;
pub(super) const POLLIN: i16 = 1;
pub(super) const POLLOUT: i16 = 4;
pub(super) const POLL_FAILURE: i16 = 8 | 16 | 32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}
#[repr(C)]
#[derive(Default)]
struct Credentials {
    pid: i32,
    uid: u32,
    gid: u32,
}
#[repr(C)]
struct UnixAddress {
    family: u16,
    path: [u8; 108],
}
#[repr(C)]
struct Timespec {
    sec: c_long,
    nsec: c_long,
}
#[repr(C)]
struct Iovec {
    base: *mut c_void,
    len: usize,
}
#[repr(C)]
struct Msghdr {
    name: *mut c_void,
    namelen: u32,
    iov: *mut Iovec,
    iovlen: usize,
    control: *mut c_void,
    controllen: usize,
    flags: i32,
}
#[repr(C)]
struct Cmsg {
    len: usize,
    level: i32,
    kind: i32,
}

extern "C" {
    fn getsockopt(fd: i32, level: i32, option: i32, value: *mut c_void, len: *mut u32) -> i32;
    fn recvmsg(fd: i32, message: *mut Msghdr, flags: i32) -> isize;
    fn send(fd: i32, bytes: *const c_void, len: usize, flags: i32) -> isize;
    fn close(fd: i32) -> i32;
    fn getrandom(bytes: *mut c_void, len: usize, flags: u32) -> isize;
    fn clock_gettime(clock: i32, value: *mut Timespec) -> i32;
    fn eventfd(init: u32, flags: i32) -> i32;
    fn read(fd: i32, bytes: *mut c_void, len: usize) -> isize;
    fn write(fd: i32, bytes: *const c_void, len: usize) -> isize;
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
    fn shutdown(fd: i32, how: i32) -> i32;
    fn getsockname(fd: i32, addr: *mut UnixAddress, len: *mut u32) -> i32;
    fn getpeername(fd: i32, addr: *mut UnixAddress, len: *mut u32) -> i32;
}

pub(super) fn random_nonce() -> io::Result<Nonce> {
    let mut nonce = [0; 16];
    // SAFETY: writable fixed buffer; nonblocking entropy, no fallback identity.
    let count = unsafe { getrandom(nonce.as_mut_ptr().cast(), nonce.len(), 1) };
    if count != 16 || nonce == [0; 16] {
        return Err(io::Error::other("nonce unavailable"));
    }
    Ok(nonce)
}

pub(crate) fn boottime_ms() -> io::Result<u64> {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: valid output object, Linux CLOCK_BOOTTIME is 7 on both targets.
    if unsafe { clock_gettime(7, &mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if ts.sec < 0 || !(0..1_000_000_000).contains(&ts.nsec) {
        return Err(io::Error::other("invalid BOOTTIME"));
    }
    (ts.sec as u64)
        .checked_mul(1000)
        .and_then(|v| v.checked_add(ts.nsec as u64 / 1_000_000))
        .filter(|v| *v <= LIMIT)
        .ok_or_else(|| io::Error::other("BOOTTIME overflow"))
}

pub(super) fn seqpacket(fd: RawFd) -> bool {
    let mut kind = 0i32;
    let mut len = size_of::<i32>() as u32;
    // SAFETY: live fd from an OwnedFd, correctly sized output.
    (unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_TYPE,
            (&mut kind as *mut i32).cast(),
            &mut len,
        ) == 0
    }) && len == 4
        && kind == 5
}

pub(super) fn unnamed_pair_endpoint(fd: RawFd) -> bool {
    if !seqpacket(fd) {
        return false;
    }
    let mut listening = 1i32;
    let mut len = 4u32;
    // SAFETY: SO_ACCEPTCONN output sized as int.
    if unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            30,
            (&mut listening as *mut i32).cast(),
            &mut len,
        )
    } != 0
        || len != 4
        || listening != 0
    {
        return false;
    }
    for peer in [false, true] {
        let mut address = UnixAddress {
            family: 0,
            path: [0; 108],
        };
        let mut len = size_of::<UnixAddress>() as u32;
        // SAFETY: correctly sized sockaddr_un. Unnamed socketpair endpoints
        // return only the two-byte AF_UNIX family; named/abstract paths deny.
        let result = unsafe {
            if peer {
                getpeername(fd, &mut address, &mut len)
            } else {
                getsockname(fd, &mut address, &mut len)
            }
        };
        if result != 0 || len != 2 || address.family != 1 {
            return false;
        }
    }
    true
}

pub(super) fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut credentials = Credentials::default();
    let mut len = size_of::<Credentials>() as u32;
    // SAFETY: correctly sized Linux/Bionic ucred output; PID is never looked up.
    if unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut credentials as *mut Credentials).cast(),
            &mut len,
        )
    } != 0
        || len as usize != size_of::<Credentials>()
    {
        return Err(io::Error::other("peer credentials unavailable"));
    }
    Ok(credentials.uid)
}

pub(super) struct PeerIdentity {
    pub uid: u32,
    context: [u8; 256],
    len: usize,
}

impl PeerIdentity {
    pub fn context(&self) -> &[u8] {
        &self.context[..self.len]
    }
    pub fn valid(&self) -> bool {
        app_context(self.uid, self.context())
    }
    #[cfg(test)]
    pub fn injected(uid: u32, value: &[u8]) -> Self {
        let mut context = [0; 256];
        context[..value.len()].copy_from_slice(value);
        Self {
            uid,
            context,
            len: value.len(),
        }
    }
}

pub(super) fn authenticate(fd: RawFd, calling_uid: u32) -> io::Result<PeerIdentity> {
    if !unnamed_pair_endpoint(fd) {
        return Err(io::Error::other("not a connected unnamed SEQPACKET"));
    }
    let uid = peer_uid(fd)?;
    if uid != calling_uid {
        return Err(io::Error::other(
            "Binder caller differs from endpoint creator",
        ));
    }
    let mut context = [0; 256];
    let mut len = context.len() as u32;
    // SAFETY: bounded output and length; missing/oversized peer SID denies.
    if unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_PEERSEC,
            context.as_mut_ptr().cast(),
            &mut len,
        )
    } != 0
        || len == 0
        || len as usize > context.len()
    {
        return Err(io::Error::other("peer context unavailable"));
    }
    let mut len = len as usize;
    if context[len - 1] == 0 {
        len -= 1;
    }
    if !app_context(uid, &context[..len]) {
        return Err(io::Error::other("peer identity denied"));
    }
    Ok(PeerIdentity { uid, context, len })
}

/// Exact user0 levelFrom=all mapping, including canonical category ranges.
pub(super) fn app_context(uid: u32, context: &[u8]) -> bool {
    if !(10000..=19999).contains(&uid) || context.len() > 256 {
        return false;
    }
    let Ok(context) = std::str::from_utf8(context) else {
        return false;
    };
    let Some(categories) = context.strip_prefix("u:r:bengal_power_app:s0:") else {
        return false;
    };
    let app_id = uid - 10000;
    let expected = [app_id & 255, 256 + ((app_id >> 8) & 255), 512, 768];
    let mut found = [false; 4];
    let mut total = 0;
    fn category(token: &str) -> Option<u32> {
        let digits = token.strip_prefix('c')?;
        if digits.is_empty()
            || digits.len() > 4
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            return None;
        }
        digits.parse::<u32>().ok().filter(|v| *v <= 1023)
    }
    for token in categories.split(',') {
        let (first, last) = if let Some((a, b)) = token.split_once('.') {
            let (Some(a), Some(b)) = (category(a), category(b)) else {
                return false;
            };
            if b <= a || b - a >= 4 {
                return false;
            }
            (a, b)
        } else {
            let Some(value) = category(token) else {
                return false;
            };
            (value, value)
        };
        for value in first..=last {
            let Some(index) = expected.iter().position(|v| *v == value) else {
                return false;
            };
            if found[index] {
                return false;
            }
            found[index] = true;
            total += 1;
            if total > 4 {
                return false;
            }
        }
    }
    found == [true; 4]
}

fn aligned(n: usize) -> usize {
    (n + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

/// One recvmsg, never a stream read. Delivered SCM_RIGHTS and SCM_PIDFD are
/// closed even on oversized/invalid records. Linux closes omitted rights and
/// does not install a PIDFD when MSG_CTRUNC leaves no room for its complete int.
pub(super) fn receive(fd: RawFd) -> io::Result<Option<Packet>> {
    let mut bytes = [0u8; 65];
    // 1056B on 64-bit, enough for all Linux SCM_MAX_FD=253 descriptors.
    let mut control = [0usize; 132];
    let mut iov = Iovec {
        base: bytes.as_mut_ptr().cast(),
        len: bytes.len(),
    };
    // SAFETY: C msghdr has no Rust validity invariants; all pointer fields below
    // are initialized to valid live buffers for the duration of recvmsg.
    let mut msg: Msghdr = unsafe { zeroed() };
    msg.iov = &mut iov;
    msg.iovlen = 1;
    msg.control = control.as_mut_ptr().cast();
    msg.controllen = size_of_val(&control);
    // SAFETY: live fd and valid bounded buffers; rights arrive close-on-exec.
    let count = unsafe { recvmsg(fd, &mut msg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC) };
    if count < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    let used = msg.controllen.min(size_of_val(&control));
    let mut offset = 0usize;
    let mut ancillary = used != 0;
    while offset + size_of::<Cmsg>() <= used {
        // SAFETY: aligned control storage, bounded header established above.
        let header = unsafe { &*control.as_ptr().cast::<u8>().add(offset).cast::<Cmsg>() };
        if header.len < aligned(size_of::<Cmsg>()) || header.len > used - offset {
            ancillary = true;
            break;
        }
        if header.level == SOL_SOCKET && matches!(header.kind, SCM_RIGHTS | SCM_PIDFD) {
            let start = offset + aligned(size_of::<Cmsg>());
            // SCM_PIDFD is exactly one kernel-generated int, which can be a
            // negative errno if pidfd allocation failed. Only these two cmsg
            // types transfer descriptors; credentials/security data do not.
            let end = if header.kind == SCM_PIDFD {
                (start + size_of::<i32>()).min(offset + header.len)
            } else {
                offset + header.len
            };
            for pos in (start..end.saturating_sub(3)).step_by(4) {
                // SAFETY: kernel-generated descriptor integer inside this cmsg.
                let right = unsafe {
                    std::ptr::read_unaligned(control.as_ptr().cast::<u8>().add(pos).cast::<i32>())
                };
                // SAFETY: each received descriptor is exclusively ours and closed once.
                if right >= 0 {
                    unsafe {
                        close(right);
                    }
                }
            }
        }
        offset += aligned(header.len);
    }
    if count != 64 || msg.flags & (MSG_TRUNC | MSG_CTRUNC) != 0 || ancillary {
        return Err(io::Error::other("invalid packet or ancillary data"));
    }
    Packet::decode(&bytes[..64])
        .map(Some)
        .ok_or_else(|| io::Error::other("invalid BACP packet"))
}

pub(super) fn transmit(fd: RawFd, packet: Packet) -> io::Result<bool> {
    let bytes = packet
        .encode()
        .ok_or_else(|| io::Error::other("invalid outbound packet"))?;
    // SAFETY: one live immutable buffer; DONTWAIT and NOSIGNAL bound this worker.
    let count = unsafe {
        send(
            fd,
            bytes.as_ptr().cast(),
            bytes.len(),
            MSG_DONTWAIT | MSG_NOSIGNAL,
        )
    };
    if count == 64 {
        return Ok(true);
    }
    if count < 0 {
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            return Ok(false);
        }
        return Err(error);
    }
    Err(io::Error::other("short seqpacket send"))
}

pub(super) fn wake_fd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd returns a new owned fd, NONBLOCK|CLOEXEC.
    let fd = unsafe { eventfd(0, 0x800 | 0x80000) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: transfer newly returned fd exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(super) fn notify(fd: RawFd) {
    let one = 1u64;
    // SAFETY: fixed eventfd write, never used inside UART hooks. EAGAIN means
    // an existing unread signal already wakes the worker.
    unsafe {
        write(fd, (&one as *const u64).cast(), 8);
    }
}

pub(super) fn drain_wake(fd: RawFd) {
    let mut value = 0u64;
    // SAFETY: bounded nonblocking eventfd read.
    unsafe {
        read(fd, (&mut value as *mut u64).cast(), 8);
    }
}

pub(super) fn wait(fds: &mut [PollFd], timeout: i32) -> io::Result<()> {
    // SAFETY: contiguous live pollfd slice.
    if unsafe { poll(fds.as_mut_ptr(), fds.len(), timeout) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn disconnect(fd: &OwnedFd) {
    // SAFETY: owned connected socket; shutdown is idempotent and interrupts peer I/O.
    unsafe {
        shutdown(fd.as_raw_fd(), 2);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    extern "C" {
        fn socketpair(domain: i32, kind: i32, protocol: i32, fds: *mut i32) -> i32;
        fn sendmsg(fd: i32, message: *const Msghdr, flags: i32) -> isize;
        fn getuid() -> u32;
    }
    pub fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { socketpair(1, 5 | 0x80000, 0, fds.as_mut_ptr()) },
            0
        );
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }
    pub fn injected_auth(fd: RawFd, _: u32) -> io::Result<PeerIdentity> {
        assert!(unnamed_pair_endpoint(fd));
        // Real peercred is checked separately; ONLY the app UID/SID is injected.
        assert_eq!(peer_uid(fd)?, unsafe { getuid() });
        Ok(PeerIdentity::injected(
            10000,
            b"u:r:bengal_power_app:s0:c0,c256,c512,c768",
        ))
    }
    fn raw_send(fd: RawFd, bytes: &[u8], rights: &[RawFd]) {
        let mut control = [0usize; 132];
        let mut iov = Iovec {
            base: bytes.as_ptr().cast_mut().cast(),
            len: bytes.len(),
        };
        let mut msg: Msghdr = unsafe { zeroed() };
        msg.iov = &mut iov;
        msg.iovlen = 1;
        if !rights.is_empty() {
            assert!(rights.len() <= 253);
            let length = aligned(size_of::<Cmsg>()) + size_of_val(rights);
            unsafe {
                control.as_mut_ptr().cast::<Cmsg>().write(Cmsg {
                    len: length,
                    level: SOL_SOCKET,
                    kind: SCM_RIGHTS,
                });
                std::ptr::copy_nonoverlapping(
                    rights.as_ptr().cast::<u8>(),
                    control
                        .as_mut_ptr()
                        .cast::<u8>()
                        .add(aligned(size_of::<Cmsg>())),
                    size_of_val(rights),
                );
            }
            msg.control = control.as_mut_ptr().cast();
            msg.controllen = aligned(length);
        }
        assert_eq!(
            unsafe { sendmsg(fd, &msg, MSG_DONTWAIT | MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }
    #[test]
    fn received_pidfds_close_on_rejection_including_data_and_control_truncation() {
        extern "C" {
            fn setsockopt(fd: i32, level: i32, option: i32, value: *const c_void, len: u32) -> i32;
        }
        fn pidfds() -> Vec<i32> {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|entry| {
                    let entry = entry.unwrap();
                    let target = std::fs::read_link(entry.path()).ok()?;
                    (target.to_string_lossy() == "anon_inode:[pidfd]")
                        .then(|| entry.file_name().to_str().unwrap().parse().unwrap())
                })
                .collect()
        }
        let (receiver, sender) = pair();
        let on = 1i32;
        // The offering app can set this on the endpoint before/after transfer.
        // Unsupported kernels are explicitly NOT a passing PIDFD cleanup test.
        let enabled = unsafe {
            setsockopt(
                receiver.as_raw_fd(),
                SOL_SOCKET,
                76,
                (&on as *const i32).cast(),
                4,
            )
        };
        assert_eq!(
            enabled,
            0,
            "UNSUPPORTED SO_PASSPIDFD is not PASS: {}",
            io::Error::last_os_error()
        );
        let before = pidfds();
        for length in [64, 65, 4096, 64, 65, 4096, 64, 65] {
            raw_send(sender.as_raw_fd(), &vec![0; length], &[]);
            assert!(receive(receiver.as_raw_fd()).is_err());
        }
        let leaked: Vec<_> = pidfds()
            .into_iter()
            .filter(|fd| !before.contains(fd))
            .collect();
        println!(
            "SO_PASSPIDFD supported; eight rejected packets; leaked_pidfds={}",
            leaked.len()
        );
        // Keep red-before-green runs resource neutral even if receive regresses.
        for fd in &leaked {
            unsafe {
                close(*fd);
            }
        }
        assert!(leaked.is_empty(), "rejected SCM_PIDFD descriptors leaked");

        // SCM_CREDENTIALS + maximum SCM_RIGHTS exhausts the fixed control
        // buffer. Kernel PIDFD receive checks space BEFORE installing its FD;
        // delivered rights must still close when MSG_CTRUNC rejects the packet.
        assert_eq!(
            unsafe {
                setsockopt(
                    receiver.as_raw_fd(),
                    SOL_SOCKET,
                    16,
                    (&on as *const i32).cast(),
                    4,
                )
            },
            0
        ); // SO_PASSCRED
        extern "C" {
            fn pipe2(fds: *mut i32, flags: i32) -> i32;
        }
        let mut fds = [-1; 2];
        assert_eq!(unsafe { pipe2(fds.as_mut_ptr(), 0x800 | 0x80000) }, 0);
        let (pipe_read, pipe_write) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        raw_send(sender.as_raw_fd(), &[0; 64], &[pipe_write.as_raw_fd(); 253]);
        drop(pipe_write);
        assert!(receive(receiver.as_raw_fd()).is_err());
        let mut byte = 0u8;
        assert_eq!(
            unsafe { read(pipe_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
        let leaked: Vec<_> = pidfds()
            .into_iter()
            .filter(|fd| !before.contains(fd))
            .collect();
        for fd in &leaked {
            unsafe {
                close(*fd);
            }
        }
        assert!(leaked.is_empty(), "truncated ancillary leaked PIDFD");
    }

    #[test]
    fn real_seqpacket_exact_size_truncation_and_received_rights_are_closed() {
        let (receiver, sender) = pair();
        assert!(unnamed_pair_endpoint(receiver.as_raw_fd()));
        assert_eq!(peer_uid(receiver.as_raw_fd()).unwrap(), unsafe { getuid() });
        assert!(authenticate(receiver.as_raw_fd(), unsafe { getuid() }.wrapping_add(1)).is_err());
        let packet = Packet::hello();
        assert!(transmit(sender.as_raw_fd(), packet).unwrap());
        assert_eq!(receive(receiver.as_raw_fd()).unwrap(), Some(packet));
        assert_eq!(receive(receiver.as_raw_fd()).unwrap(), None);
        // Android LocalSocket's JNI SendFileDescriptorVector may send an empty
        // SCM_RIGHTS header with no outbound FDs. Linux elides that header.
        let bytes = packet.encode().unwrap();
        let mut iov = Iovec {
            base: bytes.as_ptr().cast_mut().cast(),
            len: bytes.len(),
        };
        let mut empty_rights = Cmsg {
            len: size_of::<Cmsg>(),
            level: SOL_SOCKET,
            kind: SCM_RIGHTS,
        };
        let mut message: Msghdr = unsafe { zeroed() };
        message.iov = &mut iov;
        message.iovlen = 1;
        message.control = (&mut empty_rights as *mut Cmsg).cast();
        message.controllen = size_of::<Cmsg>();
        assert_eq!(
            unsafe { sendmsg(sender.as_raw_fd(), &message, MSG_DONTWAIT | MSG_NOSIGNAL) },
            64
        );
        assert_eq!(receive(receiver.as_raw_fd()).unwrap(), Some(packet));
        for length in [0, 63, 65, 4096] {
            raw_send(sender.as_raw_fd(), &vec![0; length], &[]);
            assert!(receive(receiver.as_raw_fd()).is_err());
        }
        // Compare references to THIS eventfd, avoiding process-global FD counts
        // that other concurrently running tests can legitimately change.
        let right = wake_fd().unwrap();
        for length in [64, 65, 4096] {
            let rights = [right.as_raw_fd(); 253];
            raw_send(sender.as_raw_fd(), &vec![0; length], &rights);
            assert!(receive(receiver.as_raw_fd()).is_err());
        }
        // A pipe EOF proves ALL duplicate write ends received via SCM_RIGHTS
        // closed. Unlike /proc FD counting, this assertion is race independent.
        extern "C" {
            fn pipe2(fds: *mut i32, flags: i32) -> i32;
        }
        let mut fds = [-1; 2];
        assert_eq!(unsafe { pipe2(fds.as_mut_ptr(), 0x800 | 0x80000) }, 0);
        let (pipe_read, pipe_write) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        raw_send(sender.as_raw_fd(), &[0; 65], &[pipe_write.as_raw_fd(); 253]);
        drop(pipe_write);
        assert!(receive(receiver.as_raw_fd()).is_err());
        let mut byte = 0u8;
        assert_eq!(
            unsafe { read(pipe_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
        drop(sender);
        assert!(receive(receiver.as_raw_fd()).is_err());
    }
    #[test]
    fn exact_app_identity_rejects_uid_domain_and_mls_confusion() {
        for uid in [10000, 10255, 10256, 19999] {
            let id = uid - 10000;
            let context = format!(
                "u:r:bengal_power_app:s0:c{},c{},c512,c768",
                id & 255,
                256 + ((id >> 8) & 255)
            );
            assert!(app_context(uid, context.as_bytes()));
            assert!(!app_context(uid + 100000, context.as_bytes()));
            assert!(!app_context(uid + 1, context.as_bytes()));
        }
        assert!(app_context(
            10255,
            b"u:r:bengal_power_app:s0:c255.c256,c512,c768"
        ));
        for context in [
            "u:r:bengal_power_app:s0",
            "u:r:bengal_power_app:s0:c0,c256,c512",
            "u:r:bengal_power_app:s0:c0,c256,c512,c768,c769",
            "u:r:bengal_power_app:s0:c0.c256,c512,c768",
            "u:r:bengal_power_app:s0:c0,c256,c512,c768,c768",
            "u:r:bengal_power_app:s0:c00,c256,c512,c768",
            "u:r:bengal_power_app:s0:c0,c256,c512,c768:s0",
            "u:r:bengal_power_app:s1:c0,c256,c512,c768",
            "u:r:untrusted_app:s0:c0,c256,c512,c768",
            "u:r:bengal_power_app_extra:s0:c0,c256,c512,c768",
            "u:r:bengal_power_app:s0:c0,c256,c512,c768\0",
            "u:r:bengal_power_app:s0:c0,c256,c512,c768,",
        ] {
            assert!(!app_context(10000, context.as_bytes()), "{context}");
        }
        for uid in [0, 1000, 9999, 20000, 110000] {
            assert!(!app_context(
                uid,
                b"u:r:bengal_power_app:s0:c0,c256,c512,c768"
            ));
        }
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!unnamed_pair_endpoint(a.as_raw_fd()));
        let nonce = random_nonce().unwrap();
        assert_ne!(nonce, [0; 16]);
        assert_ne!(nonce, random_nonce().unwrap());
    }
}
