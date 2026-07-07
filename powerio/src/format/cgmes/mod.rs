//! CGMES (IEC 61970-600) CIMXML import: ENTSO-E's Common Grid Model Exchange
//! Standard, read into the balanced [`Network`](crate::network::Network).
//!
//! A CGMES case is a *set* of RDF/XML instance files, one per profile — EQ
//! (equipment), TP (topology), SSH (steady-state hypothesis), SV (state
//! variables), plus boundary/diagram/geography/dynamics parts — tied together
//! by `md:FullModel` headers. The reader takes a directory (or an explicit
//! file list), classifies each file by its header profile URIs, merges the
//! object descriptions across files (`rdf:ID` defines, `rdf:about` extends),
//! and maps the bus-branch view onto `Network`: `TopologicalNode` per bus,
//! with SSH supplying the operating point and SV the solved state.
//!
//! Both CGMES 2.4.15 (CIM16, `http://iec.ch/TC57/2013/CIM-schema-cim16#`,
//! ENTSO-E extensions under `entsoe:`) and CGMES 3.0 (CIM100,
//! `http://iec.ch/TC57/CIM100#`, `eu:` extensions) parse; the version is
//! detected from the `cim` namespace, tolerating vendor variants of the CIM16
//! URI year. Import only: the CGMES writer is tracked follow-up work, so
//! `cgmes` has no `TargetFormat` (the read-only precedent of `.pwb` and
//! gridfm), and no source text is retained (a multi-file set has no single
//! byte-exact echo).
//!
//! CGMES has no system MVA base (values are MW/MVAr/kV/ohm); the reader
//! normalizes onto 100 MVA. The base frequency comes from the EQ
//! `BaseFrequency` record when present, else 50 Hz (the ENTSO-E default),
//! reported as an assumption.

mod read;
mod xml;

pub use read::read_cgmes_dir;
pub(crate) use read::{dir_has_cgmes, read_cgmes_paths};

/// The CGMES release family a file set declares, from its `cim` namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CgmesVersion {
    /// CGMES 2.4.15 on CIM16 (`…/CIM-schema-cim16#`, any vintage year).
    V2_4_15,
    /// CGMES 3.0 on CIM100 (`…/CIM100#`).
    V3_0,
}

impl CgmesVersion {
    pub(crate) fn from_namespace(ns: &str) -> Option<Self> {
        if ns.contains("CIM100") {
            Some(CgmesVersion::V3_0)
        } else if ns.contains("CIM-schema-cim16") {
            Some(CgmesVersion::V2_4_15)
        } else {
            None
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CgmesVersion::V2_4_15 => "CGMES 2.4.15 (CIM16)",
            CgmesVersion::V3_0 => "CGMES 3.0 (CIM100)",
        }
    }
}
