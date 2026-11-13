//! Turns away loopback connections that other users open. A browser keeps
//! its launch URL on its command line, where they can read it; on Linux the
//! kernel also names the owner of each loopback socket, so a stolen link is
//! useless. Root passes: it can read the books anyway. A socket missing from
//! the tables, as without /proc/net, keeps only the token and the one-time
//! code as protection rather than locking out its owner.

use std::fs;
use std::io;
use std::net::{SocketAddr, SocketAddrV4, TcpListener, TcpStream};

/// Accepts the next connection whose client socket belongs to this user or
/// to root, or whose owner `peer_uid` cannot tell, and closes the others.
pub(crate) fn accept_owned(listener: &TcpListener, peer_uid: impl Fn(&TcpStream) -> Option<u32>) -> io::Result<TcpStream> {
    let own = rustix::process::getuid().as_raw();
    loop {
        let (stream, _) = listener.accept()?;
        match peer_uid(&stream) {
            Some(uid) if uid != own && uid != 0 => drop(stream),
            _ => return Ok(stream),
        }
    }
}

/// The owner of the client socket of a loopback connection, from
/// /proc/net/tcp, or from tcp6 for an IPv4-mapped client. `None` where the
/// tables do not exist, as outside Linux, or lack the socket.
pub(crate) fn peer_uid(stream: &TcpStream) -> Option<u32> {
    let (SocketAddr::V4(local), SocketAddr::V4(remote)) = (stream.local_addr().ok()?, stream.peer_addr().ok()?) else { return None };
    let mapped = mapped_prefix();
    [("/proc/net/tcp", ""), ("/proc/net/tcp6", mapped.as_str())].into_iter().find_map(|(path, prefix)| {
        let table = fs::read_to_string(path).ok()?;
        socket_uid(&table, &format!("{prefix}{}", proc_addr(remote)), &format!("{prefix}{}", proc_addr(local)))
    })
}

/// The ::ffff: part of an IPv4-mapped address as tcp6 prints it: four
/// host-order words.
fn mapped_prefix() -> String {
    format!("{:016X}{:08X}", 0, u32::from_ne_bytes([0, 0, 0xff, 0xff]))
}

/// An IPv4 address and port the way /proc/net/tcp prints them: the address
/// as a host-order word, both in hex.
fn proc_addr(addr: SocketAddrV4) -> String {
    format!("{:08X}:{:04X}", u32::from_ne_bytes(addr.ip().octets()), addr.port())
}

/// Finds the socket with the given local and remote addresses in a
/// /proc/net/tcp table and returns its owner.
fn socket_uid(table: &str, local: &str, remote: &str) -> Option<u32> {
    table.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > 7 && fields[1] == local && fields[2] == remote { fields[7].parse().ok() } else { None }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Read;
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    #[test]
    fn owner_only_knows_loopback_owners() {
        // Two sockets of one loopback connection, as /proc/net/tcp lists them.
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:C350 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 101 1 0 20 4 30 10 -1
   1: 0100007F:D431 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1001        0 102 1 0 20 4 30 10 -1";
        assert_eq!(socket_uid(table, "0100007F:D431", "0100007F:C350"), Some(1001));
        assert_eq!(socket_uid(table, "0100007F:D432", "0100007F:C350"), None, "a socket that is not in the table");
        // tcp6 lists an IPv4-mapped connection behind the ::ffff: prefix, in
        // host-order words; the fixture has a little-endian machine's.
        if cfg!(target_endian = "little") {
            let table6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:D431 0000000000000000FFFF00000100007F:C350 01 00000000:00000000 00:00000000 00000000  1002        0 103 1 0 20 4 30 10 -1";
            let mapped = mapped_prefix();
            assert_eq!(socket_uid(table6, &format!("{mapped}0100007F:D431"), &format!("{mapped}0100007F:C350")), Some(1002));
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let accepted = accept_owned(&listener, peer_uid).unwrap();
        if Path::new("/proc/net/tcp").exists() {
            assert_eq!(peer_uid(&accepted), Some(rustix::process::getuid().as_raw()), "own connection owner");
        }
    }

    #[test]
    fn owner_only_closes_other_users_connections() {
        let own = rustix::process::getuid().as_raw();
        let stranger = Cell::new(true);
        let fake_uid = |_: &TcpStream| Some(if stranger.replace(false) { own + 1 } else { own });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut other = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let owner = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let accepted = accept_owned(&listener, fake_uid).unwrap();
        other.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        assert_eq!(other.read(&mut [0]).unwrap(), 0, "another user's connection stayed open");
        assert_eq!(accepted.peer_addr().unwrap(), owner.local_addr().unwrap(), "accept returned another user's connection");
    }
}
