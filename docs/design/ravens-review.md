# MG-RAVENS and PMD MATHEMATICAL review handoff

This records the review of [Rust #746](https://github.com/eigenergy/powerio/pull/746)
and its [Julia companion #197](https://github.com/eigenergy/PowerIO.jl/pull/197).
The format and fidelity chapters remain the user-facing contract.

## Ownership and compatibility

| Surface | Owner | Boundary |
|---|---|---|
| CIM identifiers, phase vocabulary and decoded profile detection | `powerio-core::cim` | No electrical model dependencies |
| Balanced RAVENS | `powerio-tx::format::ravens` | Reads and writes `BalancedNetwork` |
| Distribution RAVENS and PMD MATHEMATICAL | `powerio-dist` | Reads and writes `MulticonductorNetwork`; no tx dependency |
| RAVENS profile selection | `powerio-tnd::formats::ravens` | Dispatches to the appropriate reader and refuses mixed profiles |
| IR, catalog and public dispatch | `powerio` | One `ravens-json` token; family capabilities describe both profiles |
| Python, C and Julia | Existing typed facade and ABI 7 | No new generated C entry points or Julia runtime API |

Retained source bytes belong to a parsed module, not its electrical value.
Same-format emission can therefore be byte exact while canonical emission
through PowerIO IR reports representational losses. Tests exercise both paths.
The new source-format names use the next IR catalog, `pio-ir/2/0.11.8`;
the released 0.11.7 schema is unchanged. IR stays at version 2. Existing
public enum positions and serialized catalog shape remain compatible.

## Verified findings and regression coverage

Six independent reviews began after both companion branches had green CI.
They covered the complete original diff, then cross-checked the fixes.

| Finding | Regression evidence |
|---|---|
| A fixed-Q generator became a voltage controller | Balanced reader/writer tests and two converged native PowSybl AC solutions |
| An unequipped bus lost its voltage base | Native `Connector` metadata, unchanged electrical component counts, schema validation and IR round trip |
| Sparse polynomial terms and reactive-only ZIP terms changed meaning | Balanced cost and voltage-law tests |
| Distribution source impedance disappeared | Typed Thevenin branch, coupled sequence-to-phase matrix checks, repeated canonical writes and independent OpenDSS voltage comparison |
| Disabled equipment became active; finite neutral resistance became perfect ground | Distribution status and grounding regressions; native OpenDSS neutral-voltage comparison |
| Neutral poles, phase order, delta branch labels or load voltage bases changed | Distribution physical regression suite and native RAVENS schema gate |
| Kron-reduced transformer neutrals, angle units, switch shunts and zero-resistance leakage were recovered incorrectly | PMD reader/writer regressions and native PMD exports |
| Unbounded transformer ratings became zero capacity and erased impedance | Positive computational impedance base with explicit imputation, while the capacity remains unbounded |
| Center-tap polarity and asymmetric leakage changed during canonical conversion | Native PMD-derived winding-orientation regression and BMOPF explicit-zero leakage cases |
| Tiny malformed inputs caused panics or unbounded dimension allocation | Direct reader/writer malformed-input regressions and seeded `ravens_pmd_math` fuzz target |
| Unrelated warnings concealed electrical changes in the conversion matrix | Independent physical comparisons and a mutation test that corrupts an impedance despite unchanged metadata warnings |
| Report truncation hid field differences and fixture identity | Uncapped matrix detail output with fixture labels; bounded corpus reporting retains its existing API |
| The matrix compared an eliminated grounded return with earth as different nodes | Restored ground identity in the expected projection, with a mutation check that still rejects reversed winding orientation |
| CGMES voltage limits remained only in detailed connectivity | Apply the limits to member calculation buses and check both supported CGMES versions |
| Identifier substitutions and omitted area memberships were silent in exchange routes | Report actual old/new bus identifiers and lost areas; unchanged default IIDM area 1 does not trigger a loss finding |

## Independent oracles

`evals/validation/ravens/` pins MG-RAVENS commit
`094ad1163cb111df676884635aa03faa0538b688`. Distribution output uses its
unmodified generated schema. The upstream MATPOWER converter itself writes
two fields missing from that schema: `ConnectivityNode.SvVoltage` and
`MySettings`. The balanced check adds only those two typed shapes, runs the
native converter to prove that discrepancy, and includes rejection controls.
It does not claim that the unmodified schema accepts the balanced profile.
Every emitted test document passes through IR to prevent source-byte echo.
The archived ENWL example's precise source-semantic refusal is reported
separately from successfully validated output.

`evals/powsybl/check_ravens.py` compares direct and RAVENS-mediated routes
through IR for MATPOWER cases 9, 14, 30, 57 and 118. It checks native tables
after PSS/E 33, PSS/E 35, RAWX, XIIDM and JIIDM imports. The two documented
profile losses, generator voltage setpoints and third branch ratings, have
explicit diagnostic and recovered-value assertions. Other table fields
remain compared. The fixed-Q case additionally compares converged AC bus
voltages, angles and generator powers. The existing full PowSybl suite
continues to cover its broader PSS/E, IIDM, CGMES and UCTE corpus.

`powerio-dist/tests/pmd_math.rs` compares the writer against five independent
PMD v0.16.0 exports. The regeneration commands are in
`tests/data/dist/README.md`, with the oracle environment under
`powerio-dist/tools/pmd/`. Vendored examples retain their upstream licenses;
every added fixture is smaller than 100 KiB.

## Reproducing the gates

Run `bash scripts/ci-mirror.sh` for the repository's Linux CI configurations.
Additional focused commands and oracle setup are documented in:

- `evals/validation/ravens/README.md` — schema and converter-profile gate.
- `evals/powsybl/README.md` — native import and electrical comparisons.
- `tests/data/dist/README.md` — PMD export regeneration and fixture provenance.
- `fuzz/README.md` — the direct RAVENS/PMD parser and writer target.
- `docs/src/contributor-workflow.md` — binding and feature-matrix gates.

The conversion matrix's detailed artifact records individual fixtures,
diagnostics and typed differences. Its baseline pins observable grades and
finding codes; it is not an independent numerical oracle. Electrical checks
do not receive a blanket waiver because a conversion reports metadata loss.
Unsupported conductor projections require the affected device's own finding.
Two RAVENS transformer projections are checked against exact recovered values:
the center-tap fixture loses its additional leakage pairs, and the PMD
two-winding fixture loses nonunity winding taps. Their named diagnostics and
electrical changes remain visible; these routes are not called electrically
equivalent. The archived BMOPF IEEE 13 fixture declares `xfm2` as single phase
but gives each winding three unidentified terminals. Its transformer comparison
is explicitly unverified, scoped only to that component; other transformers,
lines and injections still undergo the physical checks.

Schema acceptance, canonical fixed points and matching finite test cases do
not prove universal electrical equivalence. Unsupported controls and fields
remain explicit fidelity findings. Input dimension limits prevent the
demonstrated allocation amplification; they are not a global process-memory
budget for arbitrarily large networks.
