# DGS test fixtures

Every file in this directory is **original to this repository**, hand-authored
for the `powerio` test suite from the public description of the DIgSILENT
PowerFactory DGS ASCII interface. None of it is copied from, or derived from,
DIgSILENT's own example files, which are copyrighted and are **not** included
here. The fixtures are covered by this repository's license.

| File | Purpose |
| --- | --- |
| `minimal_v7.dgs` | DGS 7.0 five-terminal network exercising every mapped class (ElmNet, ElmTerm, ElmXnet, ElmSym+TypSym, ElmShnt, two ElmLod, two ElmLne+TypLne with one sectioned by ElmLnesec, ElmTr2+TypTr2 with an off-neutral tap, a closed and an open ElmCoup, full StaCubic/StaSwitch wiring), plus quoting (`"Load;2"`), empty and `$empty$` cells, vector columns (`IntGrfcon rX:*`), and an unknown class (`ElmFoobar`). |
| `minimal_v5.dgs` | The same network in DGS 5.0 shape: numeric `ID` keys, no `OP` column, untyped-length column specs. Parses to the same electrical network as `minimal_v7.dgs`. |
| `minimal_v6_latin1.dgs` | A small DGS 6.0 network stored as ISO-8859-1 bytes with CRLF line endings and a Latin-1 character in a bus name, to exercise the byte-decoding path. |
| `opd_only_v7.dgs` | Operational-data-only export (`##`-keyed update rows, no `ElmTerm` topology); the reader rejects it with guidance to export the full model. |
| `binary_reject.pfd` | 64 arbitrary non-UTF-8 bytes standing in for an encrypted PowerFactory project export; the reader rejects `.pfd` with a friendly message. |
