//! Process facts: what a pid is, what it holds open.
//!
//! One home for reading `/proc`. Identity (argv, `comm`), state, parent,
//! CPU ticks, working directory and open descriptors are facts; what they
//! *mean* — fleet liveness attribution in [`crate::runtime::live`], health checks in
//! [`crate::runtime::doctor`] — is the consumers' judgement. The root is injectable,
//! so both are testable against a fixture tree instead of the live kernel.
//!
//! `/proc` visibility depends on permissions and mount options. An unreadable
//! entry means "cannot tell", never "dead"; consumers decide whether to degrade
//! or report the uncertainty.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::PathBuf;

/// The `/proc` tree to read: [`Proc::system`] in production, a fixture
/// directory in tests.
#[derive(Clone, Debug)]
pub struct Proc {
    root: PathBuf,
}

/// The part of `/proc/<pid>/stat` anyone downstream needs.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    /// Process state letter, e.g. `R`, `S`, `Z`.
    pub state: char,
    pub parent: u32,
    /// `utime + stime`, in clock ticks.
    pub ticks: u64,
}

impl Proc {
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/proc"),
        }
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn dir(&self, pid: u32) -> PathBuf {
        self.root.join(pid.to_string())
    }

    /// Numeric entries of the root. An unreadable root or entry means the
    /// process set is unknown, not empty.
    pub fn pids(&self) -> io::Result<Vec<u32>> {
        let mut pids = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    pub fn exists(&self, pid: u32) -> std::result::Result<bool, String> {
        self.dir(pid)
            .try_exists()
            .map_err(|error| error.to_string())
    }

    /// argv, split on NUL. `comm` is truncated to 15 bytes and OMP overwrites
    /// it (`omp daemon brok`), so `argv[0]` is the reliable name.
    pub fn argv(&self, pid: u32) -> Option<Vec<String>> {
        let raw = fs::read(self.dir(pid).join("cmdline")).ok()?;
        Some(
            raw.split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        )
    }

    pub fn comm(&self, pid: u32) -> Option<String> {
        fs::read_to_string(self.dir(pid).join("comm"))
            .ok()
            .map(|comm| comm.trim().to_owned())
    }

    /// State, parent and CPU ticks.
    ///
    /// Fields are positional, but field 2 is the executable name in
    /// parentheses and may itself contain spaces and parentheses — hence
    /// splitting after the *last* `)` rather than tokenizing the whole line.
    pub fn stat(&self, pid: u32) -> Option<Stat> {
        let raw = fs::read_to_string(self.dir(pid).join("stat")).ok()?;
        let after_comm = &raw[raw.rfind(')')? + 1..];
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let parent = fields.next()?.parse().ok()?;
        // `after_comm` starts at field 3; utime and stime are fields 14
        // and 15, so skip fields 5 through 13 after reading parent (4).
        let utime: u64 = fields.nth(9)?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        Some(Stat {
            state,
            parent,
            ticks: utime.checked_add(stime)?,
        })
    }

    pub fn cwd(&self, pid: u32) -> Option<PathBuf> {
        fs::read_link(self.dir(pid).join("cwd")).ok()
    }

    /// Open descriptors with their symlink targets: `(name, target)`. A
    /// `None` target is a dangling or unreadable link. An unreadable `fd`
    /// directory is a whole-tree error, so a consumer can tell "no
    /// descriptors" from "cannot look".
    pub fn descriptors(
        &self,
        pid: u32,
    ) -> std::result::Result<Vec<(OsString, Option<PathBuf>)>, std::io::Error> {
        let fd_dir = self.dir(pid).join("fd");
        let mut out = Vec::new();
        for entry in fs::read_dir(&fd_dir)? {
            let entry = entry?;
            out.push((entry.file_name(), fs::read_link(entry.path()).ok()));
        }
        Ok(out)
    }

    /// Whether the descriptor is open for writing, from its `fdinfo` flags.
    pub fn descriptor_is_writable(
        &self,
        pid: u32,
        descriptor: &OsStr,
    ) -> std::result::Result<bool, String> {
        let path = self.dir(pid).join("fdinfo").join(descriptor);
        let contents = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let flags = contents
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .map(str::trim)
            .ok_or_else(|| format!("missing flags in {}", path.display()))?;
        let flags = u32::from_str_radix(flags, 8)
            .map_err(|error| format!("cannot parse flags in {}: {error}", path.display()))?;
        Ok(flags & 0b11 != 0)
    }

    /// Every TCP socket in the network namespace, IPv4 and IPv6, from
    /// `net/tcp{,6}`. The tables are namespace-wide, so one read serves every
    /// pid; a descriptor's `socket:[inode]` target joins a row to its owner.
    pub fn tcp(&self) -> io::Result<Vec<TcpSocket>> {
        let mut sockets = Vec::new();
        for table in ["tcp", "tcp6"] {
            match fs::read_to_string(self.root.join("net").join(table)) {
                Ok(text) => sockets.extend(text.lines().skip(1).filter_map(parse_tcp_row)),
                // A kernel without IPv6 has no `tcp6`; that is not an error.
                Err(error) if table == "tcp6" && error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(sockets)
    }
}

/// One `net/tcp` row, reduced to what joining clients to servers needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSocket {
    pub local_port: u16,
    pub remote_port: u16,
    /// The peer is `127.0.0.0/8`, `::1`, or IPv4-mapped loopback.
    pub remote_loopback: bool,
    /// Kernel state: `0x01` established, `0x0A` listening.
    pub state: u8,
    pub inode: u64,
}

impl TcpSocket {
    pub const ESTABLISHED: u8 = 0x01;
    pub const LISTEN: u8 = 0x0A;
}

/// `sl local rem st tx:rx tr:when retrnsmt uid timeout inode …`, addresses
/// as `HEX_ADDR:HEX_PORT` with the address in host byte order per 32-bit
/// word — so an IPv4 loopback address ends in `7F`.
fn parse_tcp_row(line: &str) -> Option<TcpSocket> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let endpoint = |field: &str| {
        let (address, port) = field.split_once(':')?;
        let loopback = match address.len() {
            8 => address.ends_with("7F"),
            32 => {
                address == "00000000000000000000000001000000"
                    || (address.starts_with("0000000000000000FFFF0000") && address.ends_with("7F"))
            }
            _ => return None,
        };
        Some((loopback, u16::from_str_radix(port, 16).ok()?))
    };
    let (_, local_port) = endpoint(fields.get(1)?)?;
    let (remote_loopback, remote_port) = endpoint(fields.get(2)?)?;
    Some(TcpSocket {
        local_port,
        remote_port,
        remote_loopback,
        state: u8::from_str_radix(fields.get(3)?, 16).ok()?,
        inode: fields.get(9)?.parse().ok()?,
    })
}

/// The inode of a `socket:[inode]` descriptor target.
pub fn socket_inode(target: &std::path::Path) -> Option<u64> {
    target
        .to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_inventory_distinguishes_missing_and_unreadable_roots_from_empty() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(Proc::at(&empty).pids().unwrap().is_empty());
        assert_eq!(
            Proc::at(dir.path().join("missing"))
                .pids()
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        let not_a_directory = dir.path().join("file");
        fs::write(&not_a_directory, "").unwrap();
        assert!(Proc::at(&not_a_directory).pids().is_err());
    }

    #[test]
    fn stat_parses_parent_and_ticks_after_nested_parentheses_without_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("17");
        fs::create_dir(&pid_dir).unwrap();
        let stat = pid_dir.join("stat");
        let prefix = "17 (name (inner) suffix) S 42 0 0 0 0 0 0 0 0 0";
        fs::write(&stat, format!("{prefix} 5 7\n")).unwrap();
        let proc = Proc::at(dir.path());
        let parsed = proc.stat(17).unwrap();
        assert_eq!((parsed.state, parsed.parent, parsed.ticks), ('S', 42, 12));

        fs::write(&stat, format!("{prefix} {} 1\n", u64::MAX)).unwrap();
        assert!(proc.stat(17).is_none());
    }

    #[test]
    fn tcp_rows_parse_loopback_peers_in_both_families() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("net")).unwrap();
        let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        fs::write(
            dir.path().join("net/tcp"),
            format!(
                "{header}   0: 0100007F:C0D6 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11 1 0\n   1: 0100007F:D6D8 0100007F:C0D6 01 00000000:00000000 00:00000000 00000000  1000        0 12 1 0\n   2: 0553A8C0:CD0E 145A41AC:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 13 1 0\n"
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("net/tcp6"),
            format!(
                "{header}   0: 00000000000000000000000001000000:9C40 00000000000000000000000001000000:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 14 1 0\n"
            ),
        )
        .unwrap();
        let sockets = Proc::at(dir.path()).tcp().unwrap();
        let summary: Vec<_> = sockets
            .iter()
            .map(|s| (s.inode, s.state, s.remote_loopback, s.remote_port))
            .collect();
        assert_eq!(
            summary,
            [
                (11, TcpSocket::LISTEN, false, 0),
                (12, TcpSocket::ESTABLISHED, true, 0xC0D6),
                (13, TcpSocket::ESTABLISHED, false, 443),
                (14, TcpSocket::ESTABLISHED, true, 0x1F90),
            ]
        );
        assert_eq!(sockets[0].local_port, 0xC0D6);
        assert_eq!(socket_inode(std::path::Path::new("socket:[12]")), Some(12));
        assert_eq!(socket_inode(std::path::Path::new("/tmp/x")), None);
    }
}
