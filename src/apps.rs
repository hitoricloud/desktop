//! Executables of this user that have internet sockets open, named the way Xray's `process` rule sees them.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;

/// Basenames of `/proc/<pid>/exe` (as Xray's process matcher takes them) of the current user's
/// processes holding at least one TCP/UDP socket; sorted, deduplicated.
pub fn scan() -> Vec<String> {
    let inodes = internet_inodes();
    let (Ok(me), Ok(procs)) = (fs::metadata("/proc/self"), fs::read_dir("/proc")) else { return Vec::new() };
    let me = me.uid();
    let mut names = BTreeSet::new();
    for entry in procs.flatten() {
        let pid = entry.file_name();
        let Some(pid) = pid.to_str().filter(|p| p.bytes().all(|b| b.is_ascii_digit())) else { continue };
        if entry.metadata().map(|m| m.uid()).ok() != Some(me) {
            continue;
        }
        let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) else { continue };
        let exe = exe.to_string_lossy();
        let name = exe.rsplit('/').next().unwrap_or(&exe);
        if !names.contains(name) && has_socket(pid, &inodes) {
            names.insert(name.to_owned());
        }
    }
    names.into_iter().collect()
}

/// Socket inodes of every TCP/UDP socket in this network namespace.
fn internet_inodes() -> HashSet<u64> {
    let mut inodes = HashSet::new();
    for table in ["tcp", "tcp6", "udp", "udp6"] {
        let Ok(text) = fs::read_to_string(format!("/proc/net/{table}")) else { continue };
        inodes.extend(text.lines().skip(1).filter_map(|l| l.split_whitespace().nth(9)?.parse::<u64>().ok()).filter(|&i| i != 0));
    }
    inodes
}

fn has_socket(pid: &str, inodes: &HashSet<u64>) -> bool {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { return false };
    fds.flatten().any(|fd| {
        let Ok(link) = fs::read_link(fd.path()) else { return false };
        let inode = link.to_str().and_then(|l| l.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok());
        inode.is_some_and(|i| inodes.contains(&i))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, UdpSocket};
    use std::os::unix::net::UnixListener;

    fn me() -> String {
        std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn scan_sees_own_tcp_socket() {
        let _listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let names = scan();
        assert!(names.contains(&me()), "{names:?}");
        assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted and deduplicated: {names:?}");
    }

    #[test]
    fn scan_sees_own_udp_socket() {
        let _socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(scan().contains(&me()));
    }

    #[test]
    fn unix_sockets_do_not_count() {
        let path = std::env::temp_dir().join(format!("hitori-apps-test-{}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let unix = UnixListener::bind(&path).unwrap();
        let inode = fs::metadata(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&unix))).unwrap().ino();
        assert!(!internet_inodes().contains(&inode));
        drop(unix);
        fs::remove_file(&path).unwrap();
    }
}
