# CGMES fixtures

## cigre_mv/ and sample_grid_switches/

Vendored byte exact from
[sogno-platform/cimpy](https://github.com/sogno-platform/cimpy) @
`ac400d43c015afc4ac58e7e833b91fcba6f32812`
(`cimpy/examples/sampledata/CIGRE_MV` and
`cimpy/examples/sampledata/Sample_Grid_Switches/Node-Breaker`), Apache-2.0.

- `cigre_mv/` is the CIGRE Task Force C6.04.02 European MV benchmark network
  as a CGMES 2.4.15 EQ/TP/SV/DI set (a NEPLAN export using the 2012 vintage of
  the CIM16 namespace URI and **no SSH part** — which is exactly why it is
  here: element `p`/`q` must fall back to `SvPowerFlow`).
- `sample_grid_switches/` is a node-breaker switching topology demo
  (EQ/TP/SSH/SV/DL/GL) with retained breakers/disconnectors between
  topological nodes and no machines (so the missing angle reference must
  warn).

## micro30/

Original to this repository: a hand-written minimal CGMES 3.0 (CIM100 +
`eu:` extensions) set of four profile files with hand-computable expected
values, pinned by `powerio/tests/cgmes.rs`:

| quantity | fixture value | expected in `Network` (100 MVA base) |
|---|---|---|
| B1/B2 base | 110 kV | `base_kv` 110, z_base 121 Ω |
| B3 base | 20 kV | `base_kv` 20 |
| L1 r, x, bch | 2.42 Ω, 12.1 Ω, 3e-6 S | r 0.02 pu, x 0.1 pu, b 3.63e-4 pu |
| L1 patl / tatl | 400 A / 500 A | rate_a √3·110·0.4 ≈ 76.21 MVA, rate_b ≈ 95.26 MVA |
| TR1 end1 r, x @ 110 kV rated | 0.6 Ω, 10 Ω | r 0.6/121, x 10/121 pu |
| TR1 ratio | ratedU 110/21, RTC step 15 (neutral 13, 1.25 %/step, on end 2) | tap = (110/110)/(21/20)/1.025 ≈ 0.92915 |
| LOAD1 (SSH) | p 20, q 5 | load 20 MW / 5 MVAr |
| GEN1 (SSH) | p −45, q −12 (CIM load convention) | pg 45, qg 12 |
| GEN1 unit / limits | 0–80 MW, −20…25 MVAr, ratedS 50 | pmin/pmax, qmin/qmax, mbase |
| GEN1 AVR | targetValue 115.5 kV at its terminal | vg 115.5/110 = 1.05 |
| SH1 | 0.0012 S/section, SSH sections 2 | b = 0.0012·2·20² = 0.96 MVAr |
| BRK1 | normalOpen false, SSH open false, 3000 A | closed switch, current_rating 3000 |
| SvVoltage B1/B2/B3 | 112.2/0°, 111.65/−1.4°, 20.5/−4.2° | vm 1.02/1.015/1.025, va in degrees |
| TopologicalIsland angle ref | B1 | `BusType::Ref` on bus 1 |

## Not vendored

The ENTSO-E Conformity Assessment test configurations (MicroGrid, MiniGrid,
SmallGrid, RealGrid, both 2.4.15 and 3.0) carry bespoke ENTSO-E "as is,
attribution" terms rather than an SPDX license, so they are not committed
here. They are freely downloadable — the canonical ZIPs from
<https://www.entsoe.eu/data/cim/cim-conformity-and-interoperability/>, or the
unzipped per-profile XMLs vendored inside
`powsybl/powsybl-core` under `cgmes/cgmes-conformity/src/main/resources/`
(`conformity/cas-1.1.3-data-4.0.3/...` for 2.4.15,
`cgmes3-test-models/...` for 3.0) — and make good local corpus material for
the importer.
