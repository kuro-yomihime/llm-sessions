//! Stable origin identity, the guarded-request wire contract, and
//! request-local federation discovery state.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, io};
use crate::model::{RouteHop, SessionSummary, normalize_origin};

/// The hop budget every guarded request starts with, and the most any
/// request may ever claim: the default is the limit, so no caller can
/// widen discovery past it.
pub const MAX_HOPS: u32 = 8;

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
            remaining_hops: MAX_HOPS,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FederationScan {
    pub sessions: Vec<SessionSummary>,
    pub failures: Vec<String>,
}

/// The capabilities handshake, identical on every transport: the protocol
/// version a peer speaks and the stable origin identity it claims.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub version: u32,
    pub origin_id: String,
}

impl Capabilities {
    /// This instance's own handshake: the version this binary speaks and
    /// this account's origin identity. A missing or malformed marker is
    /// an explicit error — never an invented identity.
    pub fn own(home: &Path) -> Result<Self> {
        Ok(Self {
            version: PROTOCOL_VERSION,
            origin_id: read_origin(home)?,
        })
    }
}

/// A guarded request's own branch, as every transport states it: the
/// ancestors the request has already visited, and the hop budget left to
/// it. Both endpoints of the guarded listing — `federation list` over SSH
/// and `/v1/federation/list` over HTTP — bound it here, so the two
/// cannot drift apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guard {
    /// Ancestor origin ids: lowercased, well-formed, at most [`MAX_HOPS`].
    pub visited: Vec<String>,
    /// Remaining budget, at most [`MAX_HOPS`].
    pub hops: u32,
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            visited: Vec::new(),
            hops: MAX_HOPS,
        }
    }
}

/// Which half of a guarded request failed validation. The rule is the
/// protocol's; each endpoint reports the failure in its own voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// Not a JSON array of at most [`MAX_HOPS`] well-formed origin ids.
    Path,
    /// Not a hop budget of at most [`MAX_HOPS`].
    Hops,
}

impl Guard {
    /// Validate already-typed values, as the CLI's argument parser
    /// produces them.
    pub fn new(path: &str, hops: u32) -> Result<Self, Invalid> {
        Ok(Self {
            visited: visited(path)?,
            hops: bounded(hops)?,
        })
    }

    /// Parse the wire form: `path` a JSON array of ancestor origins,
    /// `hops` the remaining budget, absent meaning the default. The path
    /// is judged first, exactly as the endpoints always have.
    pub fn parse(path: &str, hops: Option<&str>) -> Result<Self, Invalid> {
        let visited = visited(path)?;
        let hops = match hops {
            None => MAX_HOPS,
            Some(raw) => raw.parse().map_err(|_| Invalid::Hops)?,
        };
        Ok(Self {
            visited,
            hops: bounded(hops)?,
        })
    }
}

/// The path half of a guarded request: a JSON array of ancestor origins,
/// each well-formed, at most [`MAX_HOPS`] of them, lowercased.
fn visited(path: &str) -> Result<Vec<String>, Invalid> {
    let mut visited: Vec<String> = serde_json::from_str(path).map_err(|_| Invalid::Path)?;
    if visited.len() > MAX_HOPS as usize || visited.iter().any(|id| !valid_origin(id)) {
        return Err(Invalid::Path);
    }
    for id in &mut visited {
        *id = normalize_origin(id);
    }
    Ok(visited)
}

/// The budget half of a guarded request: at most [`MAX_HOPS`].
fn bounded(hops: u32) -> Result<u32, Invalid> {
    if hops > MAX_HOPS {
        Err(Invalid::Hops)
    } else {
        Ok(hops)
    }
}

/// Whether a received access route can be travelled to `origin`: it names
/// a way to reach every hop, never repeats an origin, stays within
/// [`MAX_HOPS`], and ends at the origin it claims. One rule for the
/// transport that forwards routes and the endpoints that receive them.
pub fn valid_route(hops: &[RouteHop], origin: &str) -> bool {
    !hops.is_empty()
        && u32::try_from(hops.len()).is_ok_and(|length| length <= MAX_HOPS)
        && hops.last().is_some_and(|hop| hop.origin_id == origin)
        && hops.iter().enumerate().all(|(index, hop)| {
            valid_origin(&hop.origin_id)
                && hop.reachable()
                && !hops[..index]
                    .iter()
                    .any(|previous| previous.origin_id == hop.origin_id)
        })
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
    Ok(normalize_origin(origin))
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

    #[test]
    fn a_guard_bounds_and_normalizes_its_path_and_budget() {
        let guard = Guard::new(r#"["00000000-0000-4000-8000-00000000000A"]"#, 3).unwrap();
        assert_eq!(guard.visited, ["00000000-0000-4000-8000-00000000000a"]);
        assert_eq!(guard.hops, 3);

        assert_eq!(Guard::new("[]", MAX_HOPS).unwrap(), Guard::default());
        assert_eq!(Guard::parse("[]", None).unwrap(), Guard::default());
        assert_eq!(Guard::parse("[]", Some("3")).unwrap().hops, 3);

        assert_eq!(Guard::new("not json", 3).unwrap_err(), Invalid::Path);
        assert_eq!(
            Guard::new(r#"["not-a-uuid"]"#, 3).unwrap_err(),
            Invalid::Path
        );
        let full = format!(
            "[{}]",
            (0..=MAX_HOPS)
                .map(|index| format!("\"00000000-0000-4000-8000-{index:012}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(Guard::new(&full, 3).unwrap_err(), Invalid::Path);
        assert_eq!(Guard::new("[]", MAX_HOPS + 1).unwrap_err(), Invalid::Hops);
        assert_eq!(Guard::parse("[]", Some("nine")).unwrap_err(), Invalid::Hops);
        // The path is judged first, as the endpoints always have.
        assert_eq!(
            Guard::parse("not json", Some("nine")).unwrap_err(),
            Invalid::Path
        );
    }

    #[test]
    fn a_route_must_reach_its_origin_without_repeating_one() {
        let hop = |origin: &str| RouteHop {
            origin_id: origin.into(),
            name: origin.into(),
            ssh: origin.into(),
            url: None,
            token_env: None,
            home: None,
        };
        let a = "00000000-0000-4000-8000-00000000000a";
        let b = "00000000-0000-4000-8000-00000000000b";
        assert!(valid_route(&[hop(a), hop(b)], b));
        assert!(!valid_route(&[], b), "an empty route reaches nothing");
        assert!(
            !valid_route(&[hop(b), hop(a)], b),
            "it must end at the origin it claims"
        );
        assert!(!valid_route(&[hop(a), hop(a)], a), "an origin repeats");
    }
}
