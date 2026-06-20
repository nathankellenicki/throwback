//! Game upgrading: ask update services whether a ROM has a newer version and apply it.
//!
//! Services are tried in order (ModRetro first). The first to *recognize* a ROM
//! owns it — we don't fall through after a match, even if it's already latest. A
//! recognized-with-update service returns an [`Artifact`] (a patch in any format
//! [`crate::patch`] understands, or a full ROM), which is applied and verified.
//!
//! There is intentionally no local manifest: the services are the source of
//! truth, so there's nothing for us to keep current.

mod modretro;

pub use modretro::ModRetroService;

use crate::patch::{self, Patch};
use thiserror::Error;

/// A ROM an update service recognized.
#[derive(Debug, Clone)]
pub struct Identity {
    pub service: String,
    pub title: String,
    pub current_version: String,
}

/// An available upgrade to a service's latest version.
#[derive(Debug)]
pub struct Upgrade {
    pub from_version: String,
    pub to_version: String,
    pub artifact: Artifact,
    pub changelog: Option<String>,
    /// Whether saves carry across the update, if the service says.
    pub save_compatible: Option<bool>,
}

/// How to reach the new version. Normalizes services that ship deltas vs whole ROMs.
#[derive(Debug)]
pub enum Artifact {
    /// A patch (IPS/UPS/BPS — detected by [`Patch::load`]) to apply to the input ROM.
    Patch(Vec<u8>),
    /// The full upgraded ROM, ready to use.
    FullRom(Vec<u8>),
}

/// A failure talking to a service. Not the same as "ROM not recognized" (that's
/// `Ok(None)` from [`UpdateService::identify`]).
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("malformed response: {0}")]
    Decode(String),
    /// The service refused this client (e.g. it now requires its official updater).
    #[error("{0}")]
    ClientRejected(String),
    /// The service reported an error for this ROM.
    #[error("service error: {0}")]
    Service(String),
}

/// One upgrade source. ModRetro today; others can be added to [`services`].
pub trait UpdateService {
    /// Stable id, for messages.
    fn name(&self) -> &str;

    /// Cheap recognition + version probe.
    ///   `Ok(None)`     — not this service's ROM (orchestrator tries the next).
    ///   `Ok(Some(id))` — recognized; carries title + current version.
    ///   `Err(_)`       — transport/service failure (recorded; try the next).
    fn identify(&self, rom: &[u8]) -> Result<Option<Identity>, ServiceError>;

    /// Produce the upgrade for a ROM this service identified.
    ///   `Ok(None)`     — already latest.
    fn fetch_upgrade(&self, rom: &[u8], id: &Identity) -> Result<Option<Upgrade>, ServiceError>;
}

/// The ordered list of services to consult. ModRetro is first; append others here.
pub fn services() -> Vec<Box<dyn UpdateService>> {
    vec![Box::new(ModRetroService::new())]
}

/// The result of consulting the services for a ROM.
pub enum Resolution {
    /// A service recognized the ROM and it's already the latest version.
    AlreadyLatest(Identity),
    /// A service recognized the ROM and a newer version is available.
    Update(Identity, Upgrade),
    /// A service recognized the ROM but failed to produce the upgrade.
    Failed(Identity, ServiceError),
    /// No service recognized the ROM. Carries any per-service errors so "couldn't
    /// reach the service" is distinguishable from "unknown game".
    Unrecognized(Vec<(String, ServiceError)>),
}

/// Consult `services` in order; the first to recognize `rom` owns it. `on_check`
/// is called with each service's name just before it's queried, so callers can
/// report progress.
pub fn resolve(
    rom: &[u8],
    services: &[Box<dyn UpdateService>],
    mut on_check: impl FnMut(&str),
) -> Resolution {
    let mut errors = Vec::new();
    for svc in services {
        on_check(svc.name());
        match svc.identify(rom) {
            Ok(Some(id)) => {
                return match svc.fetch_upgrade(rom, &id) {
                    Ok(None) => Resolution::AlreadyLatest(id),
                    Ok(Some(up)) => Resolution::Update(id, up),
                    Err(e) => Resolution::Failed(id, e),
                };
            }
            Ok(None) => continue,
            Err(e) => errors.push((svc.name().to_string(), e)),
        }
    }
    Resolution::Unrecognized(errors)
}

#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error(transparent)]
    Patch(#[from] patch::PatchError),
    #[error("the upgrade produced an empty ROM")]
    Empty,
    #[error("{0}")]
    Verify(String),
}

/// Apply an upgrade to `source` and verify the result, returning the upgraded ROM.
///
/// Verification (skipped when `verify` is false):
/// - UPS/BPS patches self-verify their target CRC inside [`Patch::apply_into`].
/// - IPS patches and full ROMs are checked against the GB header *and* global
///   checksum (matching what MRPatcher's own client does).
pub fn produce_upgraded_rom(
    up: &Upgrade,
    source: Vec<u8>,
    verify: bool,
) -> Result<Vec<u8>, UpgradeError> {
    let (rom, self_verified) = match &up.artifact {
        Artifact::Patch(bytes) => {
            let patch = Patch::load(bytes)?;
            let self_verified = patch.has_checksums(); // UPS/BPS check target CRC in apply
            let rom = patch.apply_into(source, verify)?;
            (rom, self_verified)
        }
        Artifact::FullRom(rom) => (rom.clone(), false),
    };

    if rom.is_empty() {
        return Err(UpgradeError::Empty);
    }
    if verify && !self_verified {
        verify_gb_rom(&rom)?;
    }
    Ok(rom)
}

/// Verify a GB/GBC ROM by its header (0x14D) and global (0x14E/0x14F) checksums.
/// Returns `Ok(())` for ROMs we can't recognize as GB (too short / not GB) — such
/// artifacts are the providing service's responsibility to vouch for.
fn verify_gb_rom(rom: &[u8]) -> Result<(), UpgradeError> {
    use crate::cartridge::{gb_global_checksum, gb_header_checksum};
    let (Some(header), Some(global)) = (gb_header_checksum(rom), gb_global_checksum(rom)) else {
        return Ok(());
    };
    let header_ok = rom.get(0x14D) == Some(&header);
    let global_ok = u16::from_be_bytes([rom[0x14E], rom[0x14F]]) == global;
    if header_ok && global_ok {
        Ok(())
    } else {
        Err(UpgradeError::Verify(format!(
            "upgraded ROM checksum mismatch (header {}, global {}) — the update may be corrupt",
            if header_ok { "OK" } else { "BAD" },
            if global_ok { "OK" } else { "BAD" },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock {
        name: &'static str,
        ident: fn() -> Result<Option<Identity>, ServiceError>,
        fetch: fn() -> Result<Option<Upgrade>, ServiceError>,
    }
    impl UpdateService for Mock {
        fn name(&self) -> &str {
            self.name
        }
        fn identify(&self, _: &[u8]) -> Result<Option<Identity>, ServiceError> {
            (self.ident)()
        }
        fn fetch_upgrade(&self, _: &[u8], _: &Identity) -> Result<Option<Upgrade>, ServiceError> {
            (self.fetch)()
        }
    }

    fn id() -> Identity {
        Identity { service: "mock".into(), title: "game".into(), current_version: "1.0".into() }
    }
    fn none_id() -> Result<Option<Identity>, ServiceError> {
        Ok(None)
    }
    fn some_id() -> Result<Option<Identity>, ServiceError> {
        Ok(Some(id()))
    }
    fn no_update() -> Result<Option<Upgrade>, ServiceError> {
        Ok(None)
    }
    fn an_update() -> Result<Option<Upgrade>, ServiceError> {
        Ok(Some(Upgrade {
            from_version: "1.0".into(),
            to_version: "1.1".into(),
            artifact: Artifact::FullRom(vec![1, 2, 3]),
            changelog: None,
            save_compatible: Some(true),
        }))
    }
    fn boom() -> Result<Option<Identity>, ServiceError> {
        Err(ServiceError::Network("down".into()))
    }

    #[test]
    fn first_to_recognize_wins_and_reports_update() {
        let svcs: Vec<Box<dyn UpdateService>> =
            vec![Box::new(Mock { name: "a", ident: some_id, fetch: an_update })];
        assert!(matches!(resolve(b"rom", &svcs, |_| {}), Resolution::Update(_, _)));
    }

    #[test]
    fn recognized_but_current_is_already_latest() {
        let svcs: Vec<Box<dyn UpdateService>> =
            vec![Box::new(Mock { name: "a", ident: some_id, fetch: no_update })];
        assert!(matches!(resolve(b"rom", &svcs, |_| {}), Resolution::AlreadyLatest(_)));
    }

    #[test]
    fn falls_through_to_the_next_service() {
        let svcs: Vec<Box<dyn UpdateService>> = vec![
            Box::new(Mock { name: "a", ident: none_id, fetch: no_update }),
            Box::new(Mock { name: "b", ident: some_id, fetch: an_update }),
        ];
        match resolve(b"rom", &svcs, |_| {}) {
            Resolution::Update(id, _) => assert_eq!(id.service, "mock"),
            _ => panic!("expected update from the second service"),
        }
    }

    #[test]
    fn no_match_records_service_errors() {
        let svcs: Vec<Box<dyn UpdateService>> = vec![
            Box::new(Mock { name: "a", ident: boom, fetch: no_update }),
            Box::new(Mock { name: "b", ident: none_id, fetch: no_update }),
        ];
        match resolve(b"rom", &svcs, |_| {}) {
            Resolution::Unrecognized(errs) => {
                assert_eq!(errs.len(), 1);
                assert_eq!(errs[0].0, "a");
            }
            _ => panic!("expected Unrecognized"),
        }
    }

    /// A 32 KB GB ROM with self-consistent header + global checksums.
    fn valid_gb_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x14D] = 0xE7; // header checksum of 25 zero bytes (0x134..=0x14C)
        // global = sum(all) - rom[0x14E] - rom[0x14F]; with only 0x14D and 0x14F
        // contributing, set 0x14F so the stored BE u16 equals the computed sum.
        rom[0x14F] = 0xE7;
        rom
    }

    #[test]
    fn verify_accepts_valid_gb_rom() {
        assert!(verify_gb_rom(&valid_gb_rom()).is_ok());
    }

    #[test]
    fn verify_rejects_corrupted_gb_rom() {
        let mut rom = valid_gb_rom();
        rom[0] = 0xFF; // perturbs the global sum
        assert!(matches!(verify_gb_rom(&rom), Err(UpgradeError::Verify(_))));
    }

    #[test]
    fn produce_full_rom_verifies_and_rejects_empty() {
        let up = Upgrade {
            from_version: "1.0".into(),
            to_version: "1.1".into(),
            artifact: Artifact::FullRom(valid_gb_rom()),
            changelog: None,
            save_compatible: None,
        };
        assert!(produce_upgraded_rom(&up, vec![], true).is_ok());

        let empty = Upgrade { artifact: Artifact::FullRom(vec![]), ..up };
        assert!(matches!(produce_upgraded_rom(&empty, vec![], true), Err(UpgradeError::Empty)));
    }
}
