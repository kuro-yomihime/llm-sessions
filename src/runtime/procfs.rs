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
}
