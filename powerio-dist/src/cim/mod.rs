//! Distribution CIM (IEC 61968-13 / CDPSM) import into the multiconductor
//! [`DistNetwork`](crate::model::DistNetwork).
//!
//! This is the distribution sibling of the transmission CGMES reader in the
//! `powerio` crate. It reads the CIM profile the GridAPPS-D / CIMHub
//! ecosystem exchanges — CIM100 with per-phase equipment
//! (`ACLineSegmentPhase`, `EnergyConsumerPhase`, `SwitchPhase`),
//! `PerLengthPhaseImpedance` matrices, and `EnergySource` substations — into
//! wire coordinates. Because it lands in `DistNetwork`, a parsed CIM feeder
//! converts to OpenDSS `.dss`, PowerModelsDistribution ENGINEERING JSON, and
//! BMOPF JSON through the same [`DistTargetFormat`](crate::DistTargetFormat)
//! machinery as every other distribution source.
//!
//! CIM serializes with the same CIMXML (IEC 61970-552) syntax as CGMES, so
//! the [`xml`] layer is a self-contained copy of the transmission reader's —
//! the two crates stay decoupled by design.
//!
//! Import only: the CIM writer (`DistNetwork` → CIM XML) is tracked follow-up
//! work, so `cim` has no [`DistTargetFormat`](crate::DistTargetFormat) and the
//! reader retains no source text (no byte-exact echo tier).
//!
//! # Phase convention
//!
//! CIM `SinglePhaseKind` codes map to the wire-coordinate terminal names
//! (OpenDSS node numbers) the rest of the crate uses: `A`→`1`, `B`→`2`,
//! `C`→`3`, `N`→`4`, and the split-phase `s1`/`s2`→`1`/`2`.

mod read;
mod xml;

pub(crate) use read::dir_has_cim;
pub use read::{parse_cim_file, parse_cim_str};
