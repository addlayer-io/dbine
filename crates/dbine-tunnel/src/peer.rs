//! Who is on the other end of a connection to the tunnel's local port.
//!
//! The port is on 127.0.0.1, so any account on the machine could connect to
//! it and reach the database with the user's SSH identity. A connection is
//! served only when its socket belongs to a process of the user running
//! DBine (DBine itself or its driver hosts); one that can't be attributed is
//! refused.
//!
//! - Linux: the socket's owner in `/proc/net/tcp` and `/proc/net/tcp6`.
//! - macOS: the socket among the open files of the user's processes
//!   (libproc), whose owner must be the user.
//! - Windows: the owning process from the TCP table (`GetExtendedTcpTable`),
//!   whose token user must be ours.
//!
//! Elsewhere every connection is refused.

use std::net::SocketAddr;

/// True when the client socket whose own address is `peer` and that is
/// connected to `local` (the listening port) belongs to the current user.
/// Blocking: it reads system tables.
pub(crate) fn same_user(peer: SocketAddr, local: SocketAddr) -> bool {
    imp::same_user(canonical(peer), canonical(local))
}

/// IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`) as IPv4.
fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

/// The owner (uid) of the socket at `local` connected to `remote`, from the
/// text of `/proc/net/tcp` or `/proc/net/tcp6`.
#[cfg(any(test, target_os = "linux"))]
fn proc_net_tcp_uid(table: &str, local: SocketAddr, remote: SocketAddr) -> Option<u32> {
    table.lines().skip(1).find_map(|line| {
        // sl local_address rem_address st tx:rx tr:when retrnsmt uid …
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            return None;
        }
        let (l, r) = (proc_addr(f[1])?, proc_addr(f[2])?);
        if canonical(l) != local || canonical(r) != remote {
            return None;
        }
        // Only a live connection says who owns it: a closed one the kernel
        // keeps (TIME_WAIT and the like) is listed with uid 0.
        (f[3] == "01").then(|| f[7].parse().ok()).flatten()
    })
}

/// `0100007F:1F90` (127.0.0.1:8080): the address as the kernel's native-endian
/// 32-bit words in hex, the port in hex.
#[cfg(any(test, target_os = "linux"))]
fn proc_addr(s: &str) -> Option<SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| ip.get(i * 8..i * 8 + 8).and_then(|w| u32::from_str_radix(w, 16).ok()).map(u32::to_ne_bytes);
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(target_os = "linux")]
mod imp {
    use std::net::SocketAddr;

    pub fn same_user(peer: SocketAddr, local: SocketAddr) -> bool {
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Some(owner) = std::fs::read_to_string(table).ok().and_then(|t| super::proc_net_tcp_uid(&t, peer, local)) {
                return owner == me;
            }
        }
        false
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use libc::{c_int, c_void, pid_t};
    use std::mem::{size_of, zeroed};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::ptr::null_mut;

    // <sys/proc_info.h>; libc doesn't have the socket part.
    const PROC_PIDFDSOCKETINFO: c_int = 3;
    const SOCKINFO_IN: i32 = 1;
    const SOCKINFO_TCP: i32 = 2;
    const INI_IPV4: u8 = 0x1;

    #[repr(C)]
    struct ProcFileInfo {
        openflags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guardflags: u32,
    }

    #[repr(C)]
    struct VinfoStat {
        dev: u32,
        mode: u16,
        nlink: u16,
        ino: u64,
        uid: u32,
        gid: u32,
        times: [i64; 8],
        size: i64,
        blocks: i64,
        blksize: i32,
        flags: u32,
        gen: u32,
        rdev: u32,
        qspare: [i64; 2],
    }

    #[repr(C)]
    struct SockbufInfo {
        cc: u32,
        hiwat: u32,
        mbcnt: u32,
        mbmax: u32,
        lowat: u32,
        flags: i16,
        timeo: i16,
    }

    /// `struct in_sockinfo` up to the addresses (also the start of
    /// `struct tcp_sockinfo`).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InSockinfo {
        fport: i32,
        lport: i32,
        gencnt: u64,
        flags: u32,
        flow: u32,
        vflag: u8,
        ip_ttl: u8,
        rfu_1: u32,
        faddr: [u8; 16],
        laddr: [u8; 16],
    }

    #[repr(C)]
    union Proto {
        ini: InSockinfo,
        /// Room for the largest member of the kernel's union.
        _raw: [u8; 1024],
    }

    #[repr(C)]
    struct SocketInfo {
        stat: VinfoStat,
        so: u64,
        pcb: u64,
        kind_type: i32,
        protocol: i32,
        family: i32,
        shorts: [i16; 8],
        oobmark: u32,
        rcv: SockbufInfo,
        snd: SockbufInfo,
        kind: i32,
        rfu_1: u32,
        proto: Proto,
    }

    #[repr(C)]
    struct SocketFdInfo {
        pfi: ProcFileInfo,
        psi: SocketInfo,
    }

    pub fn same_user(peer: SocketAddr, local: SocketAddr) -> bool {
        // SAFETY: no preconditions.
        let (me, uid) = unsafe { (libc::getpid(), libc::geteuid()) };
        if owns(me, peer, local) {
            return true;
        }
        all_pids().into_iter().filter(|&p| p > 0 && p != me && uid_of(p) == Some(uid)).any(|p| owns(p, peer, local))
    }

    fn all_pids() -> Vec<pid_t> {
        // SAFETY: a null buffer asks for the count; then the buffer and its size in bytes.
        unsafe {
            let n = libc::proc_listallpids(null_mut(), 0);
            if n <= 0 {
                return Vec::new();
            }
            let mut pids: Vec<pid_t> = vec![0; n as usize + 64];
            let n = libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<pid_t>()) as c_int);
            pids.truncate(n.max(0) as usize);
            pids
        }
    }

    fn uid_of(pid: pid_t) -> Option<u32> {
        // SAFETY: the buffer is a proc_bsdshortinfo and its size is passed.
        unsafe {
            let mut info: libc::proc_bsdshortinfo = zeroed();
            let size = size_of::<libc::proc_bsdshortinfo>() as c_int;
            (libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, (&raw mut info).cast::<c_void>(), size) == size).then_some(info.pbsi_uid)
        }
    }

    /// Whether `pid` has a TCP socket at `peer` connected to `local`.
    fn owns(pid: pid_t, peer: SocketAddr, local: SocketAddr) -> bool {
        // SAFETY: every buffer is passed with its size in bytes, and only
        // what the kernel filled (by the returned sizes) is read.
        unsafe {
            let fdsize = size_of::<libc::proc_fdinfo>();
            let bytes = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, null_mut(), 0);
            if bytes <= 0 {
                return false;
            }
            let mut fds: Vec<libc::proc_fdinfo> = vec![zeroed(); bytes as usize / fdsize + 32];
            let bytes = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), (fds.len() * fdsize) as c_int);
            if bytes <= 0 {
                return false;
            }
            fds.truncate(bytes as usize / fdsize);
            let need = (std::mem::offset_of!(SocketFdInfo, psi) + std::mem::offset_of!(SocketInfo, proto) + size_of::<InSockinfo>()) as c_int;
            for fd in fds.iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32) {
                let mut info: SocketFdInfo = zeroed();
                let got = libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDSOCKETINFO, (&raw mut info).cast::<c_void>(), size_of::<SocketFdInfo>() as c_int);
                if got < need || !(info.psi.kind == SOCKINFO_TCP || info.psi.kind == SOCKINFO_IN) {
                    continue;
                }
                let ini = info.psi.proto.ini;
                let l = addr(ini.vflag, &ini.laddr, ini.lport);
                let r = addr(ini.vflag, &ini.faddr, ini.fport);
                if super::canonical(l) == peer && super::canonical(r) == local {
                    return true;
                }
            }
            false
        }
    }

    /// An address of `in_sockinfo`: IPv4 in the last 4 bytes (`in4in6_addr`)
    /// or the 16 of IPv6; the port in network order.
    fn addr(vflag: u8, a: &[u8; 16], port: i32) -> SocketAddr {
        let port = u16::from_be(port as u16);
        let ip = if vflag & INI_IPV4 != 0 { IpAddr::V4(Ipv4Addr::new(a[12], a[13], a[14], a[15])) } else { IpAddr::V6(Ipv6Addr::from(*a)) };
        SocketAddr::new(ip, port)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL};
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};

    pub fn same_user(peer: SocketAddr, local: SocketAddr) -> bool {
        // The local port is IPv4 (127.0.0.1); dual-stack sockets connected to
        // it are listed in the IPv4 table too.
        let (SocketAddr::V4(peer), SocketAddr::V4(local)) = (peer, local) else { return false };
        let Some(pid) = owner_pid(peer.ip(), peer.port(), local.ip(), local.port()) else { return false };
        // SAFETY: no preconditions.
        if pid == unsafe { GetCurrentProcessId() } {
            return true;
        }
        // SAFETY: the handle is checked and closed; the pseudo handle of the
        // current process needs no closing.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let theirs = token_user(process);
            CloseHandle(process);
            let (Some(theirs), Some(mine)) = (theirs, token_user(GetCurrentProcess())) else { return false };
            let sid = |b: &Vec<u64>| (*(b.as_ptr() as *const TOKEN_USER)).User.Sid;
            EqualSid(sid(&theirs), sid(&mine)) != 0
        }
    }

    /// The process with the TCP socket at `lip:lport` connected to `rip:rport`.
    fn owner_pid(lip: &Ipv4Addr, lport: u16, rip: &Ipv4Addr, rport: u16) -> Option<u32> {
        // SAFETY: the table is read within the size the call filled, from an
        // 8-byte aligned buffer.
        unsafe {
            let mut size = 0u32;
            GetExtendedTcpTable(null_mut(), &mut size, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0);
            let mut buf: Vec<u64> = Vec::new();
            for _ in 0..4 {
                buf = vec![0; (size as usize).div_ceil(8) + 64];
                size = (buf.len() * 8) as u32;
                match GetExtendedTcpTable(buf.as_mut_ptr().cast::<c_void>(), &mut size, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0) {
                    NO_ERROR => break,
                    ERROR_INSUFFICIENT_BUFFER => buf.clear(),
                    _ => return None,
                }
            }
            if buf.is_empty() {
                return None;
            }
            let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
            let n = (*table).dwNumEntries as usize;
            let rows_bytes = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table) + n * size_of::<MIB_TCPROW_OWNER_PID>();
            if rows_bytes > buf.len() * 8 {
                return None;
            }
            let rows = std::slice::from_raw_parts((*table).table.as_ptr(), n);
            let ip = |a: u32| Ipv4Addr::from(a.to_ne_bytes());
            let port = |p: u32| u16::from_be(p as u16);
            rows.iter()
                .find(|r| ip(r.dwLocalAddr) == *lip && port(r.dwLocalPort) == lport && ip(r.dwRemoteAddr) == *rip && port(r.dwRemotePort) == rport)
                .map(|r| r.dwOwningPid)
        }
    }

    /// The TOKEN_USER of a process, in an aligned buffer.
    unsafe fn token_user(process: HANDLE) -> Option<Vec<u64>> {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        let mut buf: Vec<u64> = vec![0; (len as usize).div_ceil(8).max(1)];
        let ok = len != 0 && GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast::<c_void>(), (buf.len() * 8) as u32, &mut len) != 0;
        CloseHandle(token);
        ok.then_some(buf)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use std::net::SocketAddr;

    /// No way to tell who connected: refuse.
    pub fn same_user(_peer: SocketAddr, _local: SocketAddr) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:A1B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0000000000000000 100 0 0 10 0
   1: 0100007F:D431 0100007F:A1B2 01 00000000:00000000 00:00000000 00000000  1001        0 22222 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:C350 0100007F:A1B2 01 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0000000000000000 20 4 30 10 -1
   3: 0100007F:EA60 0100007F:A1B2 06 00000000:00000000 03:00000F9E 00000000     0        0 0 3 0000000000000000
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:E290 0000000000000000FFFF00000100007F:A1B2 01 00000000:00000000 00:00000000 00000000  1000        0 44444 1 0000000000000000 20 4 30 10 -1
   1: 00000000000000000000000001000000:E291 00000000000000000000000001000000:1F90 01 00000000:00000000 00:00000000 00000000  1002        0 55555 1 0000000000000000 20 4 30 10 -1
";

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn reads_addresses_of_proc_net_tcp() {
        // Little-endian words, as on every platform DBine ships for.
        if cfg!(target_endian = "little") {
            assert_eq!(proc_addr("0100007F:1F90"), Some(a("127.0.0.1:8080")));
            assert_eq!(proc_addr("00000000000000000000000001000000:1F90"), Some(a("[::1]:8080")));
            assert_eq!(proc_addr("0000000000000000FFFF00000100007F:0016"), Some(a("[::ffff:127.0.0.1]:22")));
        }
        assert_eq!(proc_addr("0100007F"), None);
        assert_eq!(proc_addr("0100:1F90"), None);
        assert_eq!(proc_addr("ZZ00007F:1F90"), None);
    }

    #[test]
    fn finds_the_owner_of_a_peer_socket() {
        if !cfg!(target_endian = "little") {
            return;
        }
        let listener = a("127.0.0.1:41394"); // 0xA1B2
        // Another user's client (uid 1001) and ours (1000).
        assert_eq!(proc_net_tcp_uid(TCP, a("127.0.0.1:54321"), listener), Some(1001));
        assert_eq!(proc_net_tcp_uid(TCP, a("127.0.0.1:50000"), listener), Some(1000));
        // A peer that isn't in the table: no owner.
        assert_eq!(proc_net_tcp_uid(TCP, a("127.0.0.1:1"), listener), None);
        // A dual-stack client, seen as IPv4-mapped in tcp6.
        assert_eq!(proc_net_tcp_uid(TCP6, a("127.0.0.1:58000"), listener), Some(1000));
        assert_eq!(proc_net_tcp_uid(TCP6, a("[::1]:58001"), a("[::1]:8080")), Some(1002));
        assert_eq!(proc_net_tcp_uid("", a("127.0.0.1:1"), listener), None);
        // A client that already closed (TIME_WAIT, listed as uid 0) has no
        // owner, even for a DBine running as root.
        assert_eq!(proc_net_tcp_uid(TCP, a("127.0.0.1:60000"), listener), None);
    }

    /// Our own connection is ours; one that doesn't exist isn't anyone's.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_connection_from_this_process_is_the_users() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(local).unwrap();
        let (_server, peer) = listener.accept().unwrap();
        assert_eq!(client.local_addr().unwrap(), peer);
        assert!(same_user(peer, local));
        assert!(!same_user(SocketAddr::new(peer.ip(), peer.port().wrapping_add(1).max(1)), local));
    }
}
