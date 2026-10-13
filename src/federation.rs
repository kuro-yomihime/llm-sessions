//! Stable origin identity and request-local federation discovery state.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, io};
use crate::model::SessionSummary;

pub const DEFAULT_HOPS: u32 = 8;

/// The federation protocol version this binary speaks on every transport.
/// Version 2 is the mixed-route contract: an access route may interleave
/// SSH and HTTP hops, and a peer names this version in its capabilities
/// before this observer sends it any guarded request. A peer answering
/// version 1 speaks the older SSH-only contract — it must never receive
/// mixed routes or chained discovery, so this observer treats it as a
/// shell leaf rather than negotiating a downgrade.
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryContext {
    pub visited: Vec<String>,
    pub remaining_hops: u32,
}

impl Default for DiscoveryContext {
    fn default() -> Self {
        Self {
            visited: Vec::new(),
            remaining_hops: DEFAULT_HOPS,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FederationScan {
    pub sessions: Vec<SessionSummary>,
    pub failures: Vec<String>,
}

fn marker(home: &Path) -> PathBuf {
    crate::state_home(home).join("llm-sessions/origin-id")
}

pub fn valid_origin(origin: &str) -> bool {
    origin.len() == 36
        && origin.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// Read only: observers must never initialize another account's marker.
pub fn read_origin(home: &Path) -> Result<String> {
    let path = marker(home);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        io(&path)(std::io::Error::new(
            error.kind(),
            format!("canonical origin identity unavailable: {error}"),
        ))
    })?;
    let origin = raw.trim();
    if !valid_origin(origin) {
        return Err(io(path)(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "canonical origin identity unavailable: malformed UUID marker",
        )));
    }
    Ok(origin.to_ascii_lowercase())
}

/// Initialize this local account atomically, without replacing an existing ID.
pub fn ensure_origin(home: &Path) -> Result<String> {
    match read_origin(home) {
        Ok(origin) => return Ok(origin),
        Err(crate::Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let path = marker(home);
    let parent = path.parent().expect("marker has a parent");
    std::fs::create_dir_all(parent).map_err(io(parent))?;
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(io("/dev/urandom"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io(parent))?;
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            write!(temporary, "-").map_err(io(&path))?;
        }
        write!(temporary, "{byte:02x}").map_err(io(&path))?;
    }
    writeln!(temporary).map_err(io(&path))?;
    temporary.as_file().sync_all().map_err(io(&path))?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io(&path)(error.error)),
    }
    // A concurrent initializer may have won. Its persisted ID is authoritative.
    read_origin(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_initializers_and_restarts_keep_one_identity() {
        let home = tempfile::tempdir().unwrap();
        let origins = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| ensure_origin(home.path()).unwrap()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(valid_origin(&origins[0]));
        assert_eq!(origins[0].as_bytes()[14], b'4');
        assert!(origins.iter().all(|origin| origin == &origins[0]));
        assert_eq!(read_origin(home.path()).unwrap(), origins[0]);
        assert_eq!(ensure_origin(home.path()).unwrap(), origins[0]);
    }

    #[test]
    fn missing_and_malformed_markers_are_not_repaired_by_reads() {
        let home = tempfile::tempdir().unwrap();
        let path = marker(home.path());
        assert!(
            read_origin(home.path())
                .unwrap_err()
                .to_string()
                .contains("identity unavailable")
        );
        assert!(!path.exists());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-an-origin\n").unwrap();
        assert!(read_origin(home.path()).is_err());
        assert!(ensure_origin(home.path()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not-an-origin\n");
    }

    #[test]
    fn marker_lives_in_the_state_namespace() {
        let home = tempfile::tempdir().unwrap();
        let origin = ensure_origin(home.path()).unwrap();
        assert_eq!(
            marker(home.path()),
            home.path().join(".local/state/llm-sessions/origin-id")
        );
        assert_eq!(
            std::fs::read_to_string(marker(home.path())).unwrap(),
            format!("{origin}\n")
        );
        assert!(!valid_origin("00000000-0000-0000-0000-00000000000g"));
        assert!(!valid_origin("00000000000000000000000000000000000000"));
    }
}
