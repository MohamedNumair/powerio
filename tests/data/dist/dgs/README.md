# DGS distribution fixtures

Every file in this directory is **original to this repository**, hand-authored
for the `powerio-dist` DGS reader tests from the public description of the
DIgSILENT PowerFactory DGS ASCII interface. None of it is copied from, or
derived from, DIgSILENT's own example files, which are copyrighted and are
**not** included here. The fixtures are covered by this repository's license.

| File | Purpose |
| --- | --- |
| `feeder_3ph_v7.dgs` | Complete MV/LV feeder: symmetrical `TypLne` (tier 2, Fortescue 3x3 with real mutual terms from Z0/Z1), a Dyn `TypTr2` transformer, a 3-phase `ElmLod`, a single-phase `ElmLodlv` pinned to phase 2 via `StaCubic.cPhInfo=L2`, a capacitor `ElmShnt`, a closed `ElmCoup` (bus fusion), the `ElmXnet` slack source, and an unknown class (warning). |
| `tower_4wire_v7.dgs` | Explicit natural-coordinate matrices (tier 1): a 4-conductor (3 phases + neutral) `R_c1`/`X_c1` matrix in **denormalized** `R_c1:SIZEROW`/`R_c1:r:c` column form on a `TypTow`. Preserved verbatim, 4th conductor kept. |
| `tower_matrix_table_v7.dgs` | The same section with the matrices in the **normalized** DGS 7.0 `$$Matrix` table (`FID;MatRow;MatColumn;Val`), referenced by FID from the type row; `X_c1` is strictly lower-triangular to exercise symmetric completion. |
| `tower_geometry_only_v7.dgs` | A line whose type is a bare `TypGeo` geometry with no computed matrices (tier 3): skipped with re-export guidance, never fabricated. |
