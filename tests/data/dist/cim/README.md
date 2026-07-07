# Distribution CIM fixtures

- `micro_feeder.xml` is original to this repository: a hand-written minimal
  distribution CIM (IEC 61968-13 / GridAPPS-D style, CIM100 namespace) feeder
  with hand-computable expected values, pinned value-by-value in
  `powerio-dist/tests/cim.rs`. It exercises the mapped element set:
  `EnergySource`, `ACLineSegment` with `ACLineSegmentPhase` and a
  `PerLengthPhaseImpedance` matrix, a wye `EnergyConsumer` with per-phase
  `EnergyConsumerPhase`, a `LinearShuntCompensator` with `ShuntCompensatorPhase`,
  a `LoadBreakSwitch`, and a two-winding delta/wye `PowerTransformer` with
  `PowerTransformerEnd` records.

  | quantity | fixture value | expected in `DistNetwork` |
  |---|---|---|
  | connectivity nodes | sourcebus, n2, n3, n4 | 4 buses; n4 carries a grounded neutral terminal `4` |
  | line impedance | `PerLengthPhaseImpedance`, self r 0.001 / x 0.002 Ω/m, mutual r 0.0004 / x 0.0009, self b 1e-8 S/m | `DistLineCode` ohm/m: `r_series[0][0]=0.001`, `r_series[1][0]=0.0004`, `b_from[0][0]=0.5e-8` |
  | line length | `Conductor.length` 100 m | `length = 100` |
  | wye load | phaseConnection Y, per-phase p 1000 W / q 500 var | `p_nom=[1000;3]`, `q_nom=[500;3]`, terminal map `1,2,3,4` |
  | shunt | per-phase bPerSection 0.0001 S | diagonal siemens `b[0][0]=0.0001` |
  | switch | `LoadBreakSwitch` open false | closed switch n2→n3 |
  | transformer | 4160 V(Δ) / 480 V(Y), 500 kVA, end1 r 0.5 Ω x 2 Ω | `r_pct = 0.5 / (4160²/500000) × 100 ≈ 1.4446`, `xsc_pct ≈ 5.7785` |
  | source | `EnergySource.voltageMagnitude` 4160 V (line-to-line) | `v_magnitude = 4160/√3 ≈ 2401.78` V, 120° phase spacing |

## Not vendored (roadmap)

Real GridAPPS-D / CIMHub feeders (IEEE 13/34/123/8500 in CIM XML) are
excellent corpus material and are freely available from
[`GRIDAPPSD/Powergrid-Models`](https://github.com/GRIDAPPSD/Powergrid-Models)
and [`GRIDAPPSD/CIMHub`](https://github.com/GRIDAPPSD/CIMHub) (Battelle
permissive licensing). They are not vendored yet because their transformers use
the `TransformerTank` / `TransformerTankInfo` / test-data form, which this
reader does not map yet (it warns and skips, mapping `PowerTransformerEnd`
transformers only). Vendoring a real feeder pairs naturally with the
transformer-tank follow-up.
