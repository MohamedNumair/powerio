# CGMES and the CIM family

CGMES — the Common Grid Model Exchange Standard, IEC 61970-600 — is ENTSO-E's
profile stack over the IEC Common Information Model, and the format European
TSOs exchange grid models in. powerio reads CGMES file sets into the balanced
[`Network`] through the transmission hub (`powerio convert <dir> --from cgmes
--to matpower`, `parse_file` on a directory, `read_cgmes_dir` in Rust). This
chapter records the version landscape the reader and writer are built
against, exactly what they map today, and the roadmap. The writer
(`write_cgmes`/`write_cgmes_dir`; CLI `--to cgmes` / `--to cgmes3` with
`-o <dir>`) emits an EQ/TP/SSH/SV set at either release with deterministic
mRIDs — imported ids pass through element `uid`s — so write → read → write is
byte stable, and any hub format converts into CGMES.

## Version map

One "CIM version" is really three coordinates: the UML release, the IEC 61970
edition it appears in, and the namespace URI instance files carry.

| CIM UML | IEC 61970-301 | namespace | CGMES release | IEC 61970-600 |
|---|---|---|---|---|
| CIM14 | ed. 2009–11 | `…/2009/CIM-schema-cim14#` | — | — |
| CIM15 | ed. 5 (2013) | `…/2010/CIM-schema-cim15#` | — | — |
| CIM16 | ed. 6 (2016) | `http://iec.ch/TC57/2013/CIM-schema-cim16#` (vendors also emit 2012 vintages) | **2.4.15** (2014; ENTSO-E ext. `http://entsoe.eu/CIM/SchemaExtension/3/1#`) | IEC TS 61970-600-1/-2:2017 |
| CIM17 | ed. 7 (2020) + AMD1 (2022) | `…/2016/CIM-schema-cim17#` | 2.5 draft, abandoned | — |
| CIM100 (= CIM17v40 + 61968 CIM13 + 62325 CIM03) | same | `http://iec.ch/TC57/CIM100#` + `http://iec.ch/TC57/CIM100-European#` | **3.0** (2020/2021) | IEC 61970-600-1/-2:2021 |

Around the core: 61970-501 defines the RDFS serialization of the schema,
61970-552 the CIMXML instance-file syntax (`rdf:ID` defines, `rdf:about`
extends, `md:FullModel` headers, difference models), 61970-452/456/453/457
the equipment/solved-state/diagram/dynamics profile content, and 61968-13
("CDPSM") the *distribution* profile family that CIM-based distribution tools
(GridAPPS-D/CIMHub) exchange. ENTSO-E operational exchange still runs on
2.4.15 today with 3.0 conformity live since 2022; both matter.

A CGMES *case* is a set of profile instance files — EQ (equipment), TP
(topology), SSH (steady-state hypothesis), SV (state variables), plus
boundary (EQBD/TPBD), DL (diagram), GL (geography), DY (dynamics) — tied
together by `md:FullModel` dependency headers, usually zipped one file per
ZIP with an `<effectiveDateTime>_<process>_<actor>_<part>_<version>` naming
mask.

## What the importer supports

- **Both versions.** CIM16/2.4.15 and CIM100/3.0 namespaces (tolerant of
  vendor URI vintages), `entsoe:`/`eu:` extension attributes where they
  matter (operational limit kinds).
- **Input shapes.** A directory of instance XMLs (auto-detected or
  `--from cgmes`), or a single merged XML. DL/GL/DY parts are recognized and
  skipped loudly. ZIP containers are not opened yet — unzip first.
- **Bus-branch through TP.** `TopologicalNode` → bus (name, base kV, solved
  `SvVoltage`); the TP part is required (node-breaker collapse from raw
  ConnectivityNodes is roadmap). Works for node-breaker sets that include
  TP, with intra-node switches absorbed and inter-node switches kept.
- **Elements.** ACLineSegment (r/x/bch/gch, per-unit on the from-side base);
  two-winding PowerTransformer (per-end impedances on rated bases, ratio from
  rated voltages and the in-service RatioTapChanger step from SSH/SvTapStep,
  linear phase tap shifts); EnergyConsumer/Conform/NonConform/StationSupply
  loads; SynchronousMachine (+GeneratingUnit bounds, CIM load-convention sign
  flip, voltage-mode RegulatingControl → `vg`/remote regulated bus);
  ExternalNetworkInjection as a generator; EquivalentInjection at boundary
  points as load/generator; LinearShuntCompensator (SSH/SV sections); the
  Switch family with SSH open state; PATL/TATL/TC operational limits onto
  `rate_a`/`rate_b`/`rate_c` (current limits through √3·kV).
- **Slack.** `TopologicalIsland.AngleRefTopologicalNode`, else the best
  `referencePriority`, else an external injection, else the largest machine —
  and a loud warning when nothing qualifies.
- **Stated assumptions.** CGMES carries no system MVA base: per-unit lands on
  100 MVA (warned on every parse). `BaseFrequency` is honored when present,
  else 50 Hz (warned). No source text is retained — a multi-file set has no
  single byte-exact echo (a set is a directory, so `cgmes` stays outside the
  single-text `TargetFormat`), and every unconsumed class is counted into the
  parse warnings.

Fixtures: a hand-computed CGMES 3.0 micro set plus the CIGRE MV benchmark and
a node-breaker switching sample vendored from cimpy (Apache-2.0) — see
`tests/data/cgmes/README.md`, including where to fetch the ENTSO-E conformity
sets that are not license-clean to vendor.

## Roadmap

Tracked follow-up work, roughly in dependency order:

1. **Three-winding transformers** → the typed `Transformer3W` record
   (per-end star impedances map directly).
2. **Node-breaker collapse** when a set has no TP part, and ZIP container
   input.
3. **Boundary-set aware assembly** (multi-IGM merging on `eu:BoundaryPoint` /
   `entsoe:ConnectivityNode.boundaryPoint`) and difference models (61970-552
   `dm:`).
4. **DL/GL → typed geometry** (`Location`/`GeoMeta` landed in the model for
   exactly this) and dynamics (DY) retention.
5. **Distribution CIM** (IEC 61968-13 / the GridAPPS-D CIM100 profile) in
   `powerio-dist`, reusing the CIMXML layer — the multiconductor sibling of
   this importer, with CIMHub's IEEE feeders as fixtures.
6. **Schema-aware validation tooling** against the ENTSO-E Application
   Profiles Library artifacts (Apache-2.0 RDFS/SHACL).

[`Network`]: https://docs.rs/powerio/latest/powerio/struct.Network.html
