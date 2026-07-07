//! Read and write DIgSILENT PowerFactory DGS ASCII power flow cases.
//!
//! A DGS file is a flat list of class tables. Each table opens with a header
//! line `$$Class;attr(type);attr(type);…` and is followed by semicolon separated
//! data rows; lines starting with `*` are comments. Column types are `i`
//! (integer), `r`/`d` (real), `a[:N]` (string), and `p` (object pointer). The
//! reader accepts the three ASCII generations that differ only in shape: v5 keys
//! rows by a numeric `ID` column, v6/v7 by a string `FID` plus an `OP` operation
//! flag (`C`/`M` create, `U` update, `D` delete, `I` ignore), and v7 may rename
//! the id column. Pointer values that start with `##` are foreign keys into a
//! PowerFactory project that is not in the file.
//!
//! Topology is not on the element rows. A `StaCubic` cubicle names the terminal
//! it sits in (`fold_id`), the element it connects (`obj_id`), and which
//! terminal of that element (`obj_bus`); an open `StaSwitch` in a cubicle breaks
//! the connection. `ElmTerm` rows become [`Network`] buses (id `1..n` in file
//! order); closed `ElmCoup` couplers fuse the two terminals they join, open ones
//! become [`Switch`]es. Lines (`ElmLne`+`TypLne`, optionally sectioned by
//! `ElmLnesec`), two- and three-winding transformers (`ElmTr2`/`ElmTr3` with
//! their `Typ*`), loads (`ElmLod`/`ElmLodlv`/`ElmLodmv`), shunts (`ElmShnt`),
//! machines (`ElmSym`/`ElmGenstat`/`ElmPvsys`/`ElmAsm`), and the external grid
//! (`ElmXnet`) map to the neutral model; per-unit values ride on a synthesized
//! `base_mva = 100` (DGS carries no system base) and the impedance base of each
//! bus. `base_frequency` comes from `ElmNet.frnom`.
//!
//! Everything else — graphics (`IntGrf*`), folders, feeders, protection,
//! unbalanced attributes, and unknown classes — is reported once per class with
//! a row count and dropped. Columns the neutral model does not name are kept in
//! element `extras` under `dgs_<attr>` keys (a generator, which has no extras, is
//! summarized in a warning instead). [`write_dgs`] emits DGS 7.0 ASCII and
//! inverts the reader's column layout for the cross-format write path; a
//! same-format write echoes the retained (decoded) source.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::Value;

use super::{Conversion, sanitize_quoted, set_bus_kind, warn_extra_branch_rating_sets, zbase};
use crate::network::{
    Branch, BranchCharging, Bus, BusId, BusType, Extras, Generator, Impedance, Load,
    LoadVoltageModel, Network, Shunt, ShuntBlock, SourceFormat, Switch, SwitchedShuntControl,
    SwitchedShuntMode, Transformer3W, Winding,
};
use crate::{Error, Result};

const FMT: &str = "DIgSILENT DGS";

/// DGS carries no system MVA base; the reader synthesizes the conventional one.
const BASE_MVA: f64 = 100.0;

/// PowerFactory's default nominal frequency when no grid states one.
const DEFAULT_FRNOM: f64 = 50.0;

/// Characters that would shift or truncate a DGS field (delimiter, quote, and
/// the reference/pattern metacharacters); the writer maps them to `_`.
const FID_FORBIDDEN: &[char] = &[';', '"', '*', '?', '=', ',', '\\', '~', '|'];

// ---- Encoding & binary detection -------------------------------------------

/// Decode raw DGS bytes into text. The official example files ship as ISO-8859-1
/// (Latin-1) with CRLF; some tools emit UTF-8 or UTF-16. Sniff the BOM, then try
/// strict UTF-8, and finally fall back to Latin-1, where every byte maps to a
/// code point so the decode never fails.
pub(crate) fn decode_dgs_bytes(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, true);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        // Latin-1: each byte is its own Unicode scalar value (0..=255).
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Decode UTF-16 code units after the BOM has been stripped. Lone surrogates and
/// a trailing odd byte become the replacement character rather than an error.
fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| {
            let hi = pair[0];
            let lo = *pair.get(1).unwrap_or(&0);
            if big_endian {
                u16::from_be_bytes([hi, lo])
            } else {
                u16::from_le_bytes([hi, lo])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// The friendly rejection for an encrypted `.pfd` project export (or any binary
/// payload): tell the caller how to produce an ASCII DGS instead.
pub(crate) fn pfd_rejection() -> Error {
    Error::FormatRead {
        format: FMT,
        message: "this looks like an encrypted PowerFactory export (.pfd or binary). \
                  Export ASCII DGS instead: PowerFactory \u{2192} File \u{2192} Export \u{2192} \
                  DGS (*.dgs), DGS version 6.0 or 7.0"
            .into(),
    }
}

/// Reject binary/encrypted content up front: a control byte other than tab/CR/LF,
/// or a document with no `$$` table header at all, is not ASCII DGS.
fn sniff_binary(source: &str) -> Result<()> {
    if source
        .bytes()
        .any(|b| b < 0x09 || (0x0e..0x20).contains(&b) || b == 0x0b || b == 0x0c)
    {
        return Err(pfd_rejection());
    }
    if !source.lines().any(|l| l.trim_start().starts_with("$$")) {
        return Err(pfd_rejection());
    }
    Ok(())
}

// ---- Layer 1: table scan ---------------------------------------------------

/// One data row: its source line (for error context) and one cell per column,
/// `None` for an empty or `$empty$` cell.
struct DgsRow {
    line_no: usize,
    cells: Vec<Option<String>>,
}

/// One class table. Columns are kept as bare names; the declared type only
/// gates the unknown-type warning at scan time, and numeric parsing is on demand.
struct DgsTable {
    class: String,
    columns: Vec<String>,
    /// Column name to index, for the typed accessors.
    index: HashMap<String, usize>,
    /// The identity column: `ID`/`FID`, or a v7 `IdColumn` override; else 0.
    id_col: usize,
    op_col: Option<usize>,
    rows: Vec<DgsRow>,
}

impl DgsTable {
    fn col(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    fn id_of(&self, row: &DgsRow) -> String {
        row.cells
            .get(self.id_col)
            .and_then(|c| c.as_deref())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

/// The scanned document plus the configuration read from `General`.
struct DgsDoc {
    tables: Vec<DgsTable>,
    by_class: HashMap<String, usize>,
    decimal: char,
}

impl DgsDoc {
    fn table(&self, class: &str) -> Option<&DgsTable> {
        self.by_class.get(class).map(|&i| &self.tables[i])
    }

    fn rows(&self, class: &str) -> &[DgsRow] {
        self.table(class).map_or(&[], |t| t.rows.as_slice())
    }
}

/// Split a DGS line into semicolon-separated cells, honoring double-quote
/// quoting (`"a;b"` is one cell, `""` an embedded quote).
fn split_semicolons(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if quoted && chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    quoted = !quoted;
                }
            }
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Parse a `name[@seq|#seq](type)` column spec into its bare name. The name may
/// contain `:` (denormalized vector cells like `rX:0`); only a `(type)` suffix
/// and an `@seq`/`#seq` ordinal are stripped. An unknown type letter warns once.
fn parse_column(
    spec: &str,
    seen_unknown: &mut HashSet<char>,
    warnings: &mut Vec<String>,
) -> String {
    let spec = spec.trim();
    let name_part = match spec.rfind('(') {
        Some(open) if spec.ends_with(')') => {
            let tyspec = &spec[open + 1..spec.len() - 1];
            let letter = tyspec.chars().next().unwrap_or('?');
            if !matches!(
                tyspec.split(':').next().unwrap_or("").trim(),
                "i" | "r" | "d" | "a" | "p"
            ) && seen_unknown.insert(letter)
            {
                warnings.push(format!(
                    "DGS column type '{letter}' is unknown; read as text"
                ));
            }
            &spec[..open]
        }
        _ => spec,
    };
    name_part
        .split(['@', '#'])
        .next()
        .unwrap_or(name_part)
        .trim()
        .to_string()
}

/// Scan the source into class tables and read the `General` configuration
/// (version and decimal separator). Rejects duplicate table names and an
/// unsupported or missing version.
fn scan(source: &str, warnings: &mut Vec<String>) -> Result<DgsDoc> {
    let mut tables: Vec<DgsTable> = Vec::new();
    let mut by_class: HashMap<String, usize> = HashMap::new();
    let mut seen_unknown = HashSet::new();

    for (i, raw) in source.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('*') {
            continue;
        }
        if let Some(body) = trimmed.strip_prefix("$$") {
            let mut cells = split_semicolons(body);
            if cells.is_empty() {
                continue;
            }
            let class = cells.remove(0).trim().to_string();
            let columns: Vec<String> = cells
                .iter()
                .map(|c| parse_column(c, &mut seen_unknown, warnings))
                .collect();
            let mut index = HashMap::with_capacity(columns.len());
            for (ci, col) in columns.iter().enumerate() {
                index.entry(col.clone()).or_insert(ci);
            }
            let id_col = columns
                .iter()
                .position(|c| c == "FID" || c == "ID")
                .unwrap_or(0);
            let op_col = columns.iter().position(|c| c == "OP");
            if by_class.contains_key(&class) {
                return Err(Error::FormatRead {
                    format: FMT,
                    message: format!("duplicate table `{class}` at line {}", i + 1),
                });
            }
            by_class.insert(class.clone(), tables.len());
            tables.push(DgsTable {
                class,
                columns,
                index,
                id_col,
                op_col,
                rows: Vec::new(),
            });
            continue;
        }
        // A data row for the current (most recent) table.
        let Some(table) = tables.last_mut() else {
            continue;
        };
        let cells = split_semicolons(trimmed);
        if cells.len() != table.columns.len() {
            return Err(Error::FormatRead {
                format: FMT,
                message: format!(
                    "table `{}` row at line {} has {} cells, header declares {}",
                    table.class,
                    i + 1,
                    cells.len(),
                    table.columns.len()
                ),
            });
        }
        let cells = cells
            .into_iter()
            .map(|c| {
                if c.is_empty() || c == "$empty$" {
                    None
                } else {
                    Some(c)
                }
            })
            .collect();
        table.rows.push(DgsRow {
            line_no: i + 1,
            cells,
        });
    }

    let decimal = read_config(&tables, warnings)?;
    let mut doc = DgsDoc {
        tables,
        by_class,
        decimal,
    };
    filter_rows(&mut doc, warnings)?;
    Ok(doc)
}

/// Read the required `General` table: validate the version (major 5/6/7) and
/// pick up the decimal separator, id-column override, and attribute mode.
fn read_config(tables: &[DgsTable], warnings: &mut Vec<String>) -> Result<char> {
    let general = tables
        .iter()
        .find(|t| t.class == "General")
        .ok_or(Error::FormatRead {
            format: FMT,
            message: "no General table; not a DGS export".into(),
        })?;
    let descr = general.col("Descr");
    let val = general.col("Val");
    let (Some(descr), Some(val)) = (descr, val) else {
        return Err(Error::FormatRead {
            format: FMT,
            message: "General table has no Descr/Val columns".into(),
        });
    };
    let setting = |key: &str| -> Option<&str> {
        general.rows.iter().find_map(|r| {
            let d = r.cells.get(descr)?.as_deref()?;
            (d.trim() == key)
                .then(|| r.cells.get(val).and_then(|c| c.as_deref()))
                .flatten()
        })
    };

    let version = setting("Version").ok_or(Error::FormatRead {
        format: FMT,
        message: "General table has no Version row".into(),
    })?;
    let major: u8 = version
        .trim()
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| Error::FormatRead {
            format: FMT,
            message: format!("unrecognized DGS version {version:?}"),
        })?;
    if !(5..=7).contains(&major) {
        return Err(Error::FormatRead {
            format: FMT,
            message: format!(
                "DGS version {version:?} is not supported (this reader handles 5.0, 6.0, 7.0)"
            ),
        });
    }

    let decimal = match setting("DecimalSeparator").map(str::trim) {
        Some(",") => ',',
        _ => '.',
    };
    if matches!(setting("AttributeMode").map(str::trim), Some("DISPLAYED")) {
        warnings.push(
            "DGS AttributeMode is DISPLAYED; exported values may be in project-display units \
             rather than SI"
                .into(),
        );
    }
    Ok(decimal)
}

/// Apply the `OP` operation flag and drop `##`-keyed update rows, `Deletion`
/// tables, and enforce per-table id uniqueness. Rows the file keeps as object
/// definitions remain; everything else is counted into a summary warning.
fn filter_rows(doc: &mut DgsDoc, warnings: &mut Vec<String>) -> Result<()> {
    let mut updates = 0usize;
    let mut deletes = 0usize;
    let mut external = 0usize;
    let mut deletion_rows = 0usize;

    for table in &mut doc.tables {
        if table.class == "General" {
            continue;
        }
        if table.class == "Deletion" {
            deletion_rows += table.rows.len();
            table.rows.clear();
            continue;
        }
        let op_col = table.op_col;
        let id_col = table.id_col;
        let mut kept: Vec<DgsRow> = Vec::with_capacity(table.rows.len());
        let mut ids: HashSet<String> = HashSet::with_capacity(table.rows.len());
        for row in std::mem::take(&mut table.rows) {
            let id = row
                .cells
                .get(id_col)
                .and_then(|c| c.as_deref())
                .unwrap_or("")
                .trim();
            if id.starts_with("##") {
                external += 1;
                continue;
            }
            let op = op_col
                .and_then(|i| row.cells.get(i))
                .and_then(|c| c.as_deref())
                .map_or("", str::trim);
            match op {
                "D" => {
                    deletes += 1;
                    continue;
                }
                "I" => continue,
                "U" => updates += 1,
                _ => {}
            }
            if !id.is_empty() && !ids.insert(id.to_string()) {
                return Err(Error::FormatRead {
                    format: FMT,
                    message: format!(
                        "duplicate id {id:?} in table `{}` at line {}",
                        table.class, row.line_no
                    ),
                });
            }
            kept.push(row);
        }
        table.rows = kept;
    }

    if updates > 0 {
        warnings.push(format!(
            "{updates} DGS update row(s) (OP=U) treated as object definitions"
        ));
    }
    if deletes > 0 {
        warnings.push(format!("{deletes} DGS delete row(s) (OP=D) skipped"));
    }
    if external > 0 {
        warnings.push(format!(
            "{external} DGS row(s) key an external PowerFactory project (## id) and were skipped"
        ));
    }
    if deletion_rows > 0 {
        warnings.push(format!(
            "DGS Deletion table skipped ({deletion_rows} rows): operational deletions reference an \
             external project"
        ));
    }
    Ok(())
}

// ---- Typed cell accessors --------------------------------------------------

/// Parse a numeric cell, translating a comma decimal separator first.
fn parse_num(s: &str, decimal: char) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if decimal == ',' {
        let swapped: String = t.chars().map(|c| if c == ',' { '.' } else { c }).collect();
        lexical_core::parse::<f64>(swapped.as_bytes()).ok()
    } else {
        lexical_core::parse::<f64>(t.as_bytes()).ok()
    }
}

fn bad_field(field: &str, tok: &str, line_no: usize) -> Error {
    Error::FormatRead {
        format: FMT,
        message: format!("{field} value {tok:?} is not a number at line {line_no}"),
    }
}

/// The raw text of a cell, or `None` for a missing column or empty cell.
fn cell<'a>(t: &'a DgsTable, r: &'a DgsRow, name: &str) -> Option<&'a str> {
    let i = t.col(name)?;
    r.cells.get(i)?.as_deref()
}

/// A string attribute that is not a `##` foreign key.
fn text<'a>(t: &'a DgsTable, r: &'a DgsRow, name: &str) -> Option<&'a str> {
    cell(t, r, name).filter(|s| !s.starts_with("##"))
}

/// A raw pointer attribute (may be a `##` external reference).
fn ptr<'a>(t: &'a DgsTable, r: &'a DgsRow, name: &str) -> Option<&'a str> {
    cell(t, r, name).map(str::trim).filter(|s| !s.is_empty())
}

fn num(t: &DgsTable, r: &DgsRow, name: &str, default: f64, decimal: char) -> Result<f64> {
    match cell(t, r, name) {
        None => Ok(default),
        Some(s) if s.starts_with("##") => Ok(default),
        Some(s) => parse_num(s, decimal).ok_or_else(|| bad_field(name, s, r.line_no)),
    }
}

fn int(t: &DgsTable, r: &DgsRow, name: &str, default: i64, decimal: char) -> Result<i64> {
    Ok(num(t, r, name, default as f64, decimal)?.round() as i64)
}

// ---- Layer 2: object index & topology --------------------------------------

/// A resolved reference: the table and row a pointer names.
type Resolved<'a> = (&'a DgsTable, &'a DgsRow);

/// One terminal connection of an element, from a cubicle.
struct Conn {
    /// The `obj_bus` side index (0 = from/HV, 1 = to/LV, 2 = tertiary).
    side: i64,
    /// Position of the connected terminal in `ElmTerm` file order.
    term_pos: usize,
    /// False when a `StaSwitch` in the cubicle is open.
    closed: bool,
}

/// Disjoint-set union over terminal positions; the smallest index in a set is
/// its root, so a fused bus keeps the first terminal's id.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut r = x;
        while self.parent[r] != r {
            r = self.parent[r];
        }
        // Path compression.
        let mut c = x;
        while self.parent[c] != r {
            let next = self.parent[c];
            self.parent[c] = r;
            c = next;
        }
        r
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            let (lo, hi) = (ra.min(rb), ra.max(rb));
            self.parent[hi] = lo;
        }
    }
}

/// The resolved topology derived from the cubicle and switch tables.
struct Topology<'a> {
    /// `ElmTerm` rows in file order.
    terminals: Vec<&'a DgsRow>,
    /// Element FID to its terminal connections.
    conns: HashMap<String, Vec<Conn>>,
    /// Union-find over terminal positions (couplers fuse closed ones).
    uf: UnionFind,
    /// Position to the surviving [`BusId`] (its set root's id).
    bus_of_pos: Vec<BusId>,
}

/// The whole reader: resolves references and builds the [`Network`].
struct Reader<'a> {
    doc: &'a DgsDoc,
    by_id: HashMap<&'a str, (usize, usize)>,
    decimal: char,
    warnings: &'a mut Vec<String>,
    external_notes: BTreeSet<String>,
}

impl<'a> Reader<'a> {
    fn build(
        doc: &'a DgsDoc,
        name_hint: Option<&str>,
        warnings: &'a mut Vec<String>,
    ) -> Result<Network> {
        let mut by_id: HashMap<&str, (usize, usize)> = HashMap::new();
        for (ti, t) in doc.tables.iter().enumerate() {
            for (ri, r) in t.rows.iter().enumerate() {
                let id = r
                    .cells
                    .get(t.id_col)
                    .and_then(|c| c.as_deref())
                    .unwrap_or("");
                if !id.is_empty() {
                    by_id.insert(id, (ti, ri));
                }
            }
        }
        let decimal = doc.decimal;
        let mut reader = Reader {
            doc,
            by_id,
            decimal,
            warnings,
            external_notes: BTreeSet::new(),
        };
        reader.run(name_hint)
    }

    fn resolve(&self, key: &str) -> Option<Resolved<'a>> {
        let (ti, ri) = self.by_id.get(key)?;
        let t = &self.doc.tables[*ti];
        Some((t, &t.rows[*ri]))
    }

    fn topology(&mut self) -> Topology<'a> {
        let terminals: Vec<&DgsRow> = self.doc.rows("ElmTerm").iter().collect();
        let term_table = self.doc.table("ElmTerm");
        let mut term_pos = HashMap::with_capacity(terminals.len());
        if let Some(tt) = term_table {
            for (pos, r) in terminals.iter().enumerate() {
                term_pos.insert(tt.id_of(r), pos);
            }
        }

        // Cubicle to closed-state, from StaSwitch (fold_id -> cubicle FID).
        let mut cubicle_closed: HashMap<String, bool> = HashMap::new();
        if let Some(sw) = self.doc.table("StaSwitch") {
            for r in &sw.rows {
                if let Some(cub) = ptr(sw, r, "fold_id") {
                    let closed = int(sw, r, "on_off", 1, self.decimal).unwrap_or(1) != 0;
                    let entry = cubicle_closed.entry(cub.to_string()).or_insert(true);
                    *entry = *entry && closed;
                }
            }
        }

        // Element FID -> connections, from StaCubic.
        let mut conns: HashMap<String, Vec<Conn>> = HashMap::new();
        if let Some(cub) = self.doc.table("StaCubic") {
            for r in &cub.rows {
                let side = int(cub, r, "obj_bus", -1, self.decimal).unwrap_or(-1);
                let Some(obj) = ptr(cub, r, "obj_id") else {
                    continue;
                };
                if side < 0 || obj.starts_with("##") {
                    continue; // spare cubicle or external element
                }
                let Some(term) = ptr(cub, r, "fold_id").and_then(|f| term_pos.get(f).copied())
                else {
                    continue;
                };
                let closed = cubicle_closed.get(&cub.id_of(r)).copied().unwrap_or(true);
                conns.entry(obj.to_string()).or_default().push(Conn {
                    side,
                    term_pos: term,
                    closed,
                });
            }
        }

        let uf = UnionFind::new(terminals.len());
        Topology {
            terminals,
            conns,
            uf,
            bus_of_pos: Vec::new(),
        }
    }

    #[expect(clippy::too_many_lines)]
    fn run(&mut self, name_hint: Option<&str>) -> Result<Network> {
        if self.doc.table("ElmTerm").is_none_or(|t| t.rows.is_empty()) {
            return Err(Error::FormatRead {
                format: FMT,
                message: "this DGS file carries only operational updates (no ElmTerm topology); \
                          export the full model"
                    .into(),
            });
        }

        let (name, base_frequency) = self.grid_info(name_hint);
        let mut topo = self.topology();

        // Fuse terminals joined by closed couplers; collect open couplers as
        // switches. Two-terminal connections come from the cubicle sides.
        let mut open_switches: Vec<(usize, usize, Option<String>)> = Vec::new();
        let mut fused = 0usize;
        if let Some(coup) = self.doc.table("ElmCoup") {
            for r in &coup.rows {
                let id = coup.id_of(r);
                let Some(ends) = Self::two_terminals(&topo, &id) else {
                    continue;
                };
                let on = int(coup, r, "on_off", 1, self.decimal)? != 0;
                let isclosed = int(coup, r, "isclosed", 1, self.decimal)? != 0;
                let outserv = int(coup, r, "outserv", 0, self.decimal)? != 0;
                let sw_closed = ends.2 && ends.3;
                if on && isclosed && !outserv && sw_closed {
                    topo.uf.union(ends.0, ends.1);
                    fused += 1;
                } else {
                    open_switches.push((
                        ends.0,
                        ends.1,
                        text(coup, r, "loc_name").map(str::to_owned),
                    ));
                }
            }
        }
        if fused > 0 {
            self.warnings.push(format!(
                "fused {fused} terminal pair(s) joined by closed couplers"
            ));
        }

        // Assign surviving bus ids (1..n in file order, minus fused terminals).
        let n = topo.terminals.len();
        topo.bus_of_pos = vec![BusId(0); n];
        for pos in 0..n {
            let root = topo.uf.find(pos);
            topo.bus_of_pos[pos] = BusId(root + 1);
        }

        let term_table = self.doc.table("ElmTerm").expect("checked above");
        let mut buses: Vec<Bus> = Vec::new();
        let mut bus_pos: HashMap<BusId, usize> = HashMap::new();
        let mut isolated = 0usize;
        let mut merged: HashMap<usize, Vec<String>> = HashMap::new();
        for pos in 0..n {
            if topo.uf.find(pos) != pos {
                // Absorbed terminal: record its name under the surviving bus.
                let name = text(term_table, topo.terminals[pos], "loc_name").unwrap_or("");
                merged
                    .entry(topo.uf.find(pos))
                    .or_default()
                    .push(name.to_string());
                continue;
            }
            let row = topo.terminals[pos];
            let uknom = num(term_table, row, "uknom", 0.0, self.decimal)?;
            let outserv = int(term_table, row, "outserv", 0, self.decimal)? != 0;
            if outserv {
                isolated += 1;
            }
            let mut extras =
                Self::extras(term_table, row, &["iUsage", "outserv", "phtech", "uknom"]);
            if let Some(names) = merged.get(&pos) {
                if !names.is_empty() {
                    extras.insert(
                        "dgs_merged_terms".into(),
                        Value::Array(names.iter().cloned().map(Value::String).collect()),
                    );
                }
            }
            let mut bus = Bus::new(
                BusId(pos + 1),
                if outserv {
                    BusType::Isolated
                } else {
                    BusType::Pq
                },
                uknom,
            );
            bus.name = text(term_table, row, "loc_name").map(str::to_owned);
            bus.uid = Some(term_table.id_of(row));
            bus.extras = extras;
            bus_pos.insert(BusId(pos + 1), buses.len());
            buses.push(bus);
        }
        // Merged names may have been collected before the surviving bus was
        // pushed; fold any that arrived late.
        for (pos, names) in &merged {
            if let Some(&bi) = bus_pos.get(&BusId(pos + 1)) {
                let entry = buses[bi]
                    .extras
                    .entry("dgs_merged_terms".into())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(arr) = entry {
                    for nm in names {
                        let v = Value::String(nm.clone());
                        if !arr.contains(&v) {
                            arr.push(v);
                        }
                    }
                }
            }
        }
        if isolated > 0 {
            self.warnings.push(format!(
                "{isolated} out-of-service terminal(s) marked isolated"
            ));
        }

        let bus_of = |pos: usize, topo: &Topology| topo.bus_of_pos[pos];

        let mut loads = Vec::new();
        let mut shunts = Vec::new();
        let mut branches = Vec::new();
        let mut generators = Vec::new();
        let mut transformers_3w = Vec::new();
        let mut switches = Vec::new();
        let mut ref_buses: Vec<BusId> = Vec::new();
        let mut pv_buses: Vec<BusId> = Vec::new();

        // Open couplers become switches between the (possibly fused) buses.
        for (a, b, name) in open_switches {
            let (fa, fb) = (topo.uf.find(a), topo.uf.find(b));
            if fa == fb {
                continue;
            }
            let mut sw = Switch::new(topo.bus_of_pos[a], topo.bus_of_pos[b], false);
            sw.uid = name;
            switches.push(sw);
        }

        self.loads(&topo, &bus_of, &mut loads)?;
        self.shunts(&topo, &bus_of, &buses, &bus_pos, &mut shunts)?;
        self.machines(
            &topo,
            &bus_of,
            &mut generators,
            &mut ref_buses,
            &mut pv_buses,
        )?;
        self.external_grids(
            &topo,
            &bus_of,
            &mut generators,
            &mut ref_buses,
            &mut pv_buses,
        )?;
        self.lines(
            &topo,
            &bus_of,
            &buses,
            &bus_pos,
            base_frequency,
            &mut branches,
        )?;
        self.series_elements(&topo, &bus_of, &buses, &bus_pos, &mut branches)?;
        self.transformers_2w(&topo, &bus_of, &buses, &bus_pos, &mut branches)?;
        self.transformers_3w(&topo, &bus_of, &mut transformers_3w)?;

        // Bus kinds: PV first, then Ref (a reference wins over PV).
        for bus in &pv_buses {
            set_bus_kind(&mut buses, &bus_pos, *bus, BusType::Pv);
        }
        for bus in &ref_buses {
            set_bus_kind(&mut buses, &bus_pos, *bus, BusType::Ref);
        }
        // Slack fallback: promote a generator bus when no reference survived.
        if !buses.iter().any(|b| b.kind == BusType::Ref) {
            if let Some(bus) = Self::pick_slack(&generators) {
                set_bus_kind(&mut buses, &bus_pos, bus, BusType::Ref);
                self.warnings.push(
                    "no DGS reference machine (ElmXnet SL / ip_ctrl); promoted a generator bus to \
                     slack"
                        .into(),
                );
            }
        }
        // A PV/reference bus takes the voltage magnitude of a machine on it.
        let mut vg_of: HashMap<BusId, f64> = HashMap::new();
        for g in &generators {
            if g.in_service {
                vg_of.entry(g.bus).or_insert(g.vg);
            }
        }
        for bus in &mut buses {
            if matches!(bus.kind, BusType::Pv | BusType::Ref) {
                if let Some(&vg) = vg_of.get(&bus.id) {
                    bus.vm = vg;
                }
            }
        }

        for note in std::mem::take(&mut self.external_notes) {
            self.warnings.push(note);
        }
        self.warn_ignored_classes();
        self.warnings
            .push("DGS carries no system base; base_mva set to 100 MVA".into());

        let net = Network {
            name,
            base_mva: BASE_MVA,
            base_frequency,
            geo: None,
            buses,
            loads,
            shunts,
            branches,
            switches,
            generators,
            storage: Vec::new(),
            hvdc: Vec::new(),
            transformers_3w,
            areas: Vec::new(),
            solver: None,
            source_format: SourceFormat::Dgs,
            source: None,
        };
        net.check_references(FMT)?;
        Ok(net)
    }

    /// The grid name and base frequency from the first `ElmNet`, warning if
    /// grids disagree on frequency.
    fn grid_info(&mut self, name_hint: Option<&str>) -> (String, f64) {
        let mut name = None;
        let mut freq = None;
        let mut disagree = false;
        if let Some(net) = self.doc.table("ElmNet") {
            for r in &net.rows {
                let f = num(net, r, "frnom", DEFAULT_FRNOM, self.decimal).unwrap_or(DEFAULT_FRNOM);
                match freq {
                    None => {
                        freq = Some(f);
                        name = text(net, r, "loc_name").map(str::to_owned);
                    }
                    Some(first) if (first - f).abs() > 1e-6 => disagree = true,
                    _ => {}
                }
            }
        }
        if disagree {
            self.warnings
                .push("DGS grids declare different nominal frequencies; used the first".into());
        }
        (
            name.or_else(|| name_hint.map(str::to_owned))
                .unwrap_or_else(|| "case".into()),
            freq.unwrap_or(DEFAULT_FRNOM),
        )
    }

    /// The two terminal positions and switch states of a two-terminal element.
    fn two_terminals(topo: &Topology, id: &str) -> Option<(usize, usize, bool, bool)> {
        let conns = topo.conns.get(id)?;
        let from = conns.iter().find(|c| c.side == 0)?;
        let to = conns.iter().find(|c| c.side == 1)?;
        Some((from.term_pos, to.term_pos, from.closed, to.closed))
    }

    /// The single terminal position and switch state of a one-terminal element.
    fn one_terminal(topo: &Topology, id: &str) -> Option<(usize, bool)> {
        let conns = topo.conns.get(id)?;
        let c = conns
            .iter()
            .find(|c| c.side == 0)
            .or_else(|| conns.first())?;
        Some((c.term_pos, c.closed))
    }

    fn loads<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        loads: &mut Vec<Load>,
    ) -> Result<()> {
        // ElmLod: general load with an optional TypLod voltage model.
        if let Some(t) = self.doc.table("ElmLod") {
            for r in &t.rows {
                let id = t.id_of(r);
                let Some((pos, closed)) = Self::one_terminal(topo, &id) else {
                    self.skip_unconnected("load", &id);
                    continue;
                };
                let scale = num(t, r, "scale0", 1.0, self.decimal)?;
                let p = num(t, r, "plini", 0.0, self.decimal)? * scale;
                let q = num(t, r, "qlini", 0.0, self.decimal)? * scale;
                let mut load = Load::new(bus_of(pos, topo), p, q);
                load.in_service = closed && int(t, r, "outserv", 0, self.decimal)? == 0;
                load.uid = Some(id.clone());
                load.voltage_model = self.load_voltage_model(t, r, p, q)?;
                load.extras =
                    Self::extras(t, r, &["plini", "qlini", "scale0", "outserv", "typ_id"]);
                loads.push(load);
            }
        }
        // ElmLodlv / ElmLodmv: low/medium-voltage aggregate loads.
        for class in ["ElmLodlv", "ElmLodmv"] {
            if let Some(t) = self.doc.table(class) {
                for r in &t.rows {
                    let id = t.id_of(r);
                    let Some((pos, closed)) = Self::one_terminal(topo, &id) else {
                        self.skip_unconnected("load", &id);
                        continue;
                    };
                    let scale = num(t, r, "scale0", 1.0, self.decimal)?;
                    let p = num(t, r, "plini", 0.0, self.decimal)? * scale;
                    let q = if cell(t, r, "qlini").is_some() {
                        num(t, r, "qlini", 0.0, self.decimal)?
                    } else {
                        let s = num(t, r, "slini", 0.0, self.decimal)?;
                        let recap = int(t, r, "pf_recap", 0, self.decimal)? == 1;
                        let kap = if recap { -1.0 } else { 1.0 };
                        kap * (s * s - p * p).max(0.0).sqrt()
                    };
                    let mut load = Load::new(bus_of(pos, topo), p, q);
                    load.in_service = closed && int(t, r, "outserv", 0, self.decimal)? == 0;
                    load.uid = Some(id.clone());
                    load.extras = Self::extras(
                        t,
                        r,
                        &["plini", "qlini", "slini", "scale0", "pf_recap", "outserv"],
                    );
                    loads.push(load);
                }
            }
        }
        if let Some(t) = self.doc.table("ElmLodlvp") {
            if !t.rows.is_empty() {
                self.warnings.push(format!(
                    "`ElmLodlvp` table ignored ({} rows): partial line loads are not mapped",
                    t.rows.len()
                ));
            }
        }
        Ok(())
    }

    /// Read a TypLod exponential voltage model, if the load points at one with
    /// non-zero exponents.
    fn load_voltage_model(
        &mut self,
        t: &DgsTable,
        r: &DgsRow,
        p: f64,
        q: f64,
    ) -> Result<Option<LoadVoltageModel>> {
        let Some(typ) = ptr(t, r, "typ_id") else {
            return Ok(None);
        };
        let Some((tt, tr)) = self.resolve(typ) else {
            if typ.starts_with("##") {
                self.external_notes.insert(
                    "some DGS loads reference a load type outside the file; used constant power"
                        .into(),
                );
            }
            return Ok(None);
        };
        if tt.class != "TypLod" {
            return Ok(None);
        }
        let kpu = num(tt, tr, "kpu", 0.0, self.decimal)?;
        let kqu = num(tt, tr, "kqu", 0.0, self.decimal)?;
        if kpu.abs() < 1e-12 && kqu.abs() < 1e-12 {
            return Ok(None);
        }
        Ok(Some(LoadVoltageModel::Exponential {
            p,
            q,
            v_nom: None,
            gamma_p: kpu,
            gamma_q: kqu,
        }))
    }

    fn shunts<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        buses: &[Bus],
        bus_pos: &HashMap<BusId, usize>,
        shunts: &mut Vec<Shunt>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmShnt") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((pos, closed)) = Self::one_terminal(topo, &id) else {
                self.skip_unconnected("shunt", &id);
                continue;
            };
            let bus = bus_of(pos, topo);
            let base_kv = super::bus_kv(buses, bus_pos, bus);
            let shtype = int(t, r, "shtype", 2, self.decimal)?;
            let ncapa = num(t, r, "ncapa", 1.0, self.decimal)?.max(1.0);
            let ushnm = {
                let u = num(t, r, "ushnm", base_kv, self.decimal)?;
                if u.abs() < 1e-9 { base_kv } else { u }
            };
            let (g, b) = self.shunt_gb(t, r, shtype, ncapa, base_kv, ushnm)?;
            let mut shunt = Shunt::new(bus, g, b);
            shunt.in_service = closed && int(t, r, "outserv", 0, self.decimal)? == 0;
            shunt.uid = Some(id.clone());
            if int(t, r, "iswitch", 0, self.decimal)? == 1 {
                let ncapx = num(t, r, "ncapx", ncapa, self.decimal)?.max(1.0);
                let step = if ncapa.abs() < 1e-9 { b } else { b / ncapa };
                shunt.control = Some(SwitchedShuntControl::new(
                    SwitchedShuntMode::Discrete,
                    1.0,
                    1.0,
                    vec![ShuntBlock::new(ncapx as u32, step)],
                ));
            }
            shunt.extras = Self::extras(
                t,
                r,
                &[
                    "shtype", "ncapa", "ncapx", "ushnm", "qtotn", "iswitch", "outserv",
                ],
            );
            shunts.push(shunt);
        }
        Ok(())
    }

    /// Shunt `(g, b)` in MW/MVAr at 1 p.u.: design ohms when present, else the
    /// rated-power fallback. Capacitive (shtype 2/0/3/4) is positive `b`, a
    /// reactor (shtype 1) negative.
    fn shunt_gb(
        &self,
        t: &DgsTable,
        r: &DgsRow,
        shtype: i64,
        ncapa: f64,
        base_kv: f64,
        ushnm: f64,
    ) -> Result<(f64, f64)> {
        let has_ohms = ["rrea", "xrea", "bcap", "gparac"]
            .iter()
            .any(|c| cell(t, r, c).is_some());
        if has_ohms {
            let rrea = num(t, r, "rrea", 0.0, self.decimal)?;
            let xrea = num(t, r, "xrea", 0.0, self.decimal)?;
            let bcap = num(t, r, "bcap", 0.0, self.decimal)?;
            let gparac = num(t, r, "gparac", 0.0, self.decimal)?;
            let (rp, xp) = match shtype {
                1 => (rrea, xrea),
                2 => {
                    let bs = bcap * 1e-6;
                    let gs = gparac * 1e-6;
                    let denom = gs * gs + bs * bs;
                    if denom.abs() < 1e-30 {
                        (0.0, 0.0)
                    } else {
                        (gs / denom, -bs / denom)
                    }
                }
                // R-L-C and Rp forms collapse to the series R plus capacitor.
                _ => {
                    let bs = bcap * 1e-6;
                    (
                        rrea,
                        if bs.abs() < 1e-30 {
                            xrea
                        } else {
                            -1e6 / bcap + xrea
                        },
                    )
                }
            };
            let denom = rp * rp + xp * xp;
            if denom.abs() < 1e-30 {
                return Ok((0.0, 0.0));
            }
            let g = ncapa * base_kv * base_kv * rp / denom;
            let b = -ncapa * base_kv * base_kv * xp / denom;
            return Ok((g, b));
        }
        let qtotn = num(t, r, "qtotn", 0.0, self.decimal)?;
        let sign = if shtype == 1 { -1.0 } else { 1.0 };
        // With an unknown bus base (`uknom = 0`, common in MATPOWER-sourced
        // files) `qtotn` is taken as the susceptance directly.
        let ratio = if base_kv.abs() < 1e-9 || ushnm.abs() < 1e-9 {
            1.0
        } else {
            base_kv / ushnm
        };
        Ok((0.0, sign * ncapa * qtotn * ratio * ratio))
    }

    fn machines<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        generators: &mut Vec<Generator>,
        ref_buses: &mut Vec<BusId>,
        pv_buses: &mut Vec<BusId>,
    ) -> Result<()> {
        for class in ["ElmSym", "ElmGenstat", "ElmPvsys", "ElmAsm"] {
            let Some(t) = self.doc.table(class) else {
                continue;
            };
            let mut dropped_attrs: BTreeSet<String> = BTreeSet::new();
            for r in &t.rows {
                let id = t.id_of(r);
                let Some((pos, closed)) = Self::one_terminal(topo, &id) else {
                    self.skip_unconnected("generator", &id);
                    continue;
                };
                let bus = bus_of(pos, topo);
                let ngnum = num(t, r, "ngnum", 1.0, self.decimal)?.max(1.0);
                let scale = num(t, r, "scale0", 1.0, self.decimal)?;
                let motor = int(t, r, "i_mot", 0, self.decimal)? == 1;
                let is_asm = class == "ElmAsm";
                let (sgn, cosn) = self.machine_type(t, r, class);
                let mut pg = num(t, r, "pgini", 0.0, self.decimal)? * ngnum;
                let mut qg = num(t, r, "qgini", 0.0, self.decimal)? * ngnum;
                if class == "ElmGenstat" || class == "ElmPvsys" {
                    pg = num(t, r, "pgini", 0.0, self.decimal)? * ngnum * scale;
                }
                if motor {
                    pg = -pg;
                    qg = -qg;
                }
                let mut g = Generator::new(bus);
                g.pg = pg;
                g.qg = qg;
                g.mbase = sgn * ngnum;
                g.vg = num(t, r, "usetp", 1.0, self.decimal)?;
                g.in_service = closed && int(t, r, "outserv", 0, self.decimal)? == 0;
                g.uid = Some(id.clone());

                // Q limits: absolute Mvar columns win, else per-unit of sgn.
                if cell(t, r, "cQ_max").is_some() || cell(t, r, "cQ_min").is_some() {
                    g.qmax = num(t, r, "cQ_max", sgn, self.decimal)? * ngnum;
                    g.qmin = num(t, r, "cQ_min", -sgn, self.decimal)? * ngnum;
                } else if is_asm {
                    g.qmax = sgn * ngnum;
                    g.qmin = -sgn * ngnum;
                } else {
                    g.qmax = num(t, r, "q_max", 1.0, self.decimal)? * sgn * ngnum;
                    g.qmin = num(t, r, "q_min", -1.0, self.decimal)? * sgn * ngnum;
                }
                g.pmax = if cell(t, r, "Pmax_uc").is_some() {
                    num(t, r, "Pmax_uc", 0.0, self.decimal)?
                } else {
                    sgn * cosn * ngnum
                };
                g.pmin = num(t, r, "Pmin_uc", 0.0, self.decimal)?;

                // Bus kind: reference machine, voltage control, or PQ.
                let ip_ctrl = int(t, r, "ip_ctrl", 0, self.decimal)? == 1;
                let voltage = matches!(text(t, r, "av_mode"), Some("constv"))
                    || int(t, r, "iv_mode", 0, self.decimal)? != 0;
                if ip_ctrl {
                    ref_buses.push(bus);
                } else if voltage && !is_asm {
                    pv_buses.push(bus);
                }
                if is_asm && cell(t, r, "qgini").is_none() {
                    self.warnings
                        .push("ElmAsm reactive dispatch not exported; set to 0".into());
                }

                for name in Self::dropped_attrs(t, r, GEN_CONSUMED) {
                    dropped_attrs.insert(name);
                }
                generators.push(g);
            }
            if !dropped_attrs.is_empty() {
                self.warnings.push(format!(
                    "`{class}` columns not mapped (a generator carries no extras): {}",
                    dropped_attrs.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Rated apparent power `sgn` (MVA) and power factor `cosn` for a machine,
    /// from its type. Defaults to the system base when the type is external.
    fn machine_type(&mut self, t: &DgsTable, r: &DgsRow, class: &str) -> (f64, f64) {
        let Some(typ) = ptr(t, r, "typ_id") else {
            return (BASE_MVA, 0.8);
        };
        let Some((tt, tr)) = self.resolve(typ) else {
            if typ.starts_with("##") {
                self.external_notes.insert(format!(
                    "some {class} machines reference a type outside the file; rated power \
                     defaulted to {BASE_MVA} MVA"
                ));
            }
            return (BASE_MVA, 0.8);
        };
        let sgn = num(tt, tr, "sgn", 0.0, self.decimal)
            .ok()
            .filter(|s| s.abs() > 1e-9)
            .or_else(|| {
                num(tt, tr, "pgn", 0.0, self.decimal)
                    .ok()
                    .filter(|p| p.abs() > 1e-9)
            })
            .unwrap_or(BASE_MVA);
        let cosn = num(tt, tr, "cosn", 0.8, self.decimal).unwrap_or(0.8);
        (sgn, cosn)
    }

    fn external_grids<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        generators: &mut Vec<Generator>,
        ref_buses: &mut Vec<BusId>,
        pv_buses: &mut Vec<BusId>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmXnet") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((pos, closed)) = Self::one_terminal(topo, &id) else {
                self.skip_unconnected("external grid", &id);
                continue;
            };
            let bus = bus_of(pos, topo);
            let bustp = text(t, r, "bustp").unwrap_or("SL").trim().to_uppercase();
            let mut g = Generator::new(bus);
            g.pg = num(t, r, "pgini", 0.0, self.decimal)?;
            g.qg = num(t, r, "qgini", 0.0, self.decimal)?;
            g.vg = num(t, r, "usetp", 1.0, self.decimal)?;
            g.mbase = BASE_MVA;
            g.in_service = closed && int(t, r, "outserv", 0, self.decimal)? == 0;
            g.uid = Some(id.clone());
            let snss = num(t, r, "snss", 0.0, self.decimal)?;
            let lim = if snss.abs() > 1e-9 { snss } else { 1e4 };
            g.pmax = lim;
            g.pmin = -lim;
            g.qmax = lim;
            g.qmin = -lim;
            match bustp.as_str() {
                "SL" => ref_buses.push(bus),
                "PV" => pv_buses.push(bus),
                _ => {}
            }
            generators.push(g);
        }
        self.warnings.push(
            "ElmXnet external grid mapped to a generator with wide P/Q limits (DGS states \
             short-circuit power, not dispatch limits)"
                .into(),
        );
        Ok(())
    }

    fn lines<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        buses: &[Bus],
        bus_pos: &HashMap<BusId, usize>,
        f: f64,
        branches: &mut Vec<Branch>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmLne") else {
            return Ok(());
        };
        // Group ElmLnesec by their parent line (fold_id).
        let mut sections: HashMap<String, Vec<&DgsRow>> = HashMap::new();
        let sec_table = self.doc.table("ElmLnesec");
        if let Some(st) = sec_table {
            for r in &st.rows {
                if let Some(parent) = ptr(st, r, "fold_id") {
                    sections.entry(parent.to_string()).or_default().push(r);
                }
            }
        }
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((from, to, cf, ct)) = Self::two_terminals(topo, &id) else {
                self.skip_unconnected("line", &id);
                continue;
            };
            let from_bus = bus_of(from, topo);
            let to_bus = bus_of(to, topo);
            let base_kv = super::bus_kv(buses, bus_pos, from_bus);
            let z_base = zbase(base_kv, BASE_MVA);

            let (mut r_pu, mut x_pu, mut b_pu) = (0.0, 0.0, 0.0);
            let mut uline = base_kv;
            let mut sline = 0.0;
            let fline = num(t, r, "fline", 1.0, self.decimal)?;

            let secs = sections.get(&id);
            let use_secs = secs.is_some_and(|s| !s.is_empty());
            if use_secs {
                let mut rows: Vec<&&DgsRow> = secs.unwrap().iter().collect();
                let st = sec_table.expect("sections came from ElmLnesec");
                rows.sort_by(|a, b| {
                    num(st, a, "index", 0.0, self.decimal)
                        .unwrap_or(0.0)
                        .partial_cmp(&num(st, b, "index", 0.0, self.decimal).unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for (i, sec) in rows.iter().enumerate() {
                    let typ = self.line_type(st, sec, &id)?;
                    let len = num(st, sec, "dline", 0.0, self.decimal)?;
                    let (dr, dx, db, u, s) = self.line_section(typ, len, z_base, f)?;
                    r_pu += dr;
                    x_pu += dx;
                    b_pu += db;
                    if i == 0 {
                        uline = u;
                        sline = s;
                    }
                }
            } else {
                let typ = self.line_type(t, r, &id)?;
                let len = num(t, r, "dline", 1.0, self.decimal)?;
                let (dr, dx, db, u, s) = self.line_section(typ, len, z_base, f)?;
                r_pu = dr;
                x_pu = dx;
                b_pu = db;
                uline = u;
                sline = s;
            }

            if base_kv > 1e-9 && (uline - base_kv).abs() > 0.5 {
                self.warnings.push(format!(
                    "line {id:?}: rated voltage {uline} kV differs from bus base {base_kv} kV"
                ));
            }

            let mut branch = Branch::new(from_bus, to_bus, r_pu, x_pu);
            branch.b = b_pu;
            branch.rate_a = 3.0_f64.sqrt() * uline * sline * fline;
            branch.in_service = cf && ct && int(t, r, "outserv", 0, self.decimal)? == 0;
            branch.uid = Some(id.clone());
            branch.extras = Self::extras(t, r, &["dline", "fline", "outserv", "typ_id", "pStoch"]);
            branches.push(branch);
        }
        Ok(())
    }

    /// Resolve a line/section type, which is fatal to a branch when absent — a
    /// line without impedance data is electrically meaningless.
    fn line_type(&self, t: &DgsTable, r: &DgsRow, line: &str) -> Result<Resolved<'a>> {
        let typ = ptr(t, r, "typ_id").unwrap_or("");
        self.resolve(typ)
            .filter(|(tt, _)| tt.class == "TypLne")
            .ok_or_else(|| Self::missing_type(line, typ))
    }

    /// Per-section `(r_pu, x_pu, b_pu, uline, sline)` from a `TypLne`.
    fn line_section(
        &self,
        typ: Resolved<'a>,
        len: f64,
        z_base: f64,
        f: f64,
    ) -> Result<(f64, f64, f64, f64, f64)> {
        let (tt, tr) = typ;
        let rline = num(tt, tr, "rline", 0.0, self.decimal)?;
        let xline = num(tt, tr, "xline", 0.0, self.decimal)?;
        let cline = num(tt, tr, "cline", 0.0, self.decimal)?;
        let uline = num(tt, tr, "uline", 0.0, self.decimal)?;
        let sline = num(tt, tr, "sline", 0.0, self.decimal)?;
        let r_pu = rline * len / z_base;
        let x_pu = xline * len / z_base;
        let b_s = std::f64::consts::TAU * f * cline * 1e-6 * len;
        let b_pu = b_s * z_base;
        Ok((r_pu, x_pu, b_pu, uline, sline))
    }

    fn series_elements<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        buses: &[Bus],
        bus_pos: &HashMap<BusId, usize>,
        branches: &mut Vec<Branch>,
    ) -> Result<()> {
        // ElmZpu: common-impedance element, per-unit on its own Sn base.
        if let Some(t) = self.doc.table("ElmZpu") {
            for r in &t.rows {
                let id = t.id_of(r);
                let Some((from, to, cf, ct)) = Self::two_terminals(topo, &id) else {
                    self.skip_unconnected("impedance", &id);
                    continue;
                };
                let sn = num(t, r, "Sn", BASE_MVA, self.decimal)?;
                let scale = if sn.abs() < 1e-9 { 1.0 } else { BASE_MVA / sn };
                let mut branch = Branch::new(
                    bus_of(from, topo),
                    bus_of(to, topo),
                    num(t, r, "r_pu", 0.0, self.decimal)? * scale,
                    num(t, r, "x_pu", 0.0, self.decimal)? * scale,
                );
                branch.in_service = cf && ct && int(t, r, "outserv", 0, self.decimal)? == 0;
                branch.uid = Some(id.clone());
                branch.extras = Self::extras(t, r, &["r_pu", "x_pu", "Sn", "outserv"]);
                branches.push(branch);
            }
        }
        // ElmSind: series reactor (ohms at the bus base).
        if let Some(t) = self.doc.table("ElmSind") {
            for r in &t.rows {
                let id = t.id_of(r);
                let Some((from, to, cf, ct)) = Self::two_terminals(topo, &id) else {
                    self.skip_unconnected("reactor", &id);
                    continue;
                };
                let base_kv = super::bus_kv(buses, bus_pos, bus_of(from, topo));
                let z_base = zbase(base_kv, BASE_MVA);
                let mut branch = Branch::new(
                    bus_of(from, topo),
                    bus_of(to, topo),
                    num(t, r, "rrea", 0.0, self.decimal)? / z_base,
                    num(t, r, "xrea", 0.0, self.decimal)? / z_base,
                );
                branch.in_service = cf && ct && int(t, r, "outserv", 0, self.decimal)? == 0;
                branch.uid = Some(id.clone());
                branch.extras = Self::extras(t, r, &["rrea", "xrea", "outserv"]);
                branches.push(branch);
            }
        }
        // ElmScap: series capacitor.
        if let Some(t) = self.doc.table("ElmScap") {
            for r in &t.rows {
                let id = t.id_of(r);
                let Some((from, to, cf, ct)) = Self::two_terminals(topo, &id) else {
                    self.skip_unconnected("series capacitor", &id);
                    continue;
                };
                let gcap = num(t, r, "gcap", 0.0, self.decimal)?;
                let bcap = num(t, r, "bcap", 0.0, self.decimal)?;
                if gcap.abs() < 1e-12 && bcap.abs() < 1e-12 {
                    continue;
                }
                let base_kv = super::bus_kv(buses, bus_pos, bus_of(from, topo));
                let z_base = zbase(base_kv, BASE_MVA);
                let d = gcap * gcap + bcap * bcap;
                let mut branch = Branch::new(
                    bus_of(from, topo),
                    bus_of(to, topo),
                    gcap / d / z_base,
                    -bcap / d / z_base,
                );
                branch.in_service = cf && ct && int(t, r, "outserv", 0, self.decimal)? == 0;
                branch.uid = Some(id.clone());
                branch.extras = Self::extras(t, r, &["gcap", "bcap", "outserv"]);
                branches.push(branch);
            }
        }
        Ok(())
    }

    fn transformers_2w<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        buses: &[Bus],
        bus_pos: &HashMap<BusId, usize>,
        branches: &mut Vec<Branch>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmTr2") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((from, to, cf, ct)) = Self::two_terminals(topo, &id) else {
                self.skip_unconnected("2-winding transformer", &id);
                continue;
            };
            let from_bus = bus_of(from, topo);
            let to_bus = bus_of(to, topo);
            let base_h = super::bus_kv(buses, bus_pos, from_bus);
            let base_l = super::bus_kv(buses, bus_pos, to_bus);

            let typ = ptr(t, r, "typ_id").unwrap_or("");
            let (tt, tr) = self
                .resolve(typ)
                .filter(|(tt, _)| tt.class == "TypTr2")
                .ok_or_else(|| Self::missing_type(&id, typ))?;

            let strn = {
                let s = num(tt, tr, "strn", 0.0, self.decimal)?;
                if s.abs() < 1e-9 { BASE_MVA } else { s }
            };
            let uktr = num(tt, tr, "uktr", 0.0, self.decimal)?;
            let pcutr = num(tt, tr, "pcutr", 0.0, self.decimal)?;
            let z_t = uktr / 100.0;
            let r_t = if cell(tt, tr, "uktrr").is_some() {
                num(tt, tr, "uktrr", 0.0, self.decimal)? / 100.0
            } else {
                pcutr / (1000.0 * strn)
            };
            let x_t = (z_t * z_t - r_t * r_t).max(0.0).sqrt();
            let rebase = BASE_MVA / strn;
            let rpu = r_t * rebase;
            let xpu = x_t * rebase;

            // Magnetizing admittance -> from-side charging.
            let curmg = num(tt, tr, "curmg", 0.0, self.decimal)?;
            let pfe = num(tt, tr, "pfe", 0.0, self.decimal)?;
            let y_m = curmg / 100.0;
            let g_m = pfe / (1000.0 * strn);
            let b_m = -(y_m * y_m - g_m * g_m).max(0.0).sqrt();
            let g_fr = g_m * strn / BASE_MVA;
            let b_fr = b_m * strn / BASE_MVA;

            // Tap composition (pandapower-verified): t·e^{jα} = 1 + du·e^{jθ}.
            let nntap = num(t, r, "nntap", 0.0, self.decimal)?;
            let nntap0 = num(tt, tr, "nntap0", 0.0, self.decimal)?;
            let dutap = num(tt, tr, "dutap", 0.0, self.decimal)?;
            let phitr = num(tt, tr, "phitr", 0.0, self.decimal)?;
            let tap_side = int(tt, tr, "tap_side", 0, self.decimal)?;
            let nt2ag = num(tt, tr, "nt2ag", 0.0, self.decimal)?;
            let du = (nntap - nntap0) * dutap / 100.0;
            let theta = phitr.to_radians();
            let re = 1.0 + du * theta.cos();
            let im = du * theta.sin();
            let ratio_t = (re * re + im * im).sqrt();
            let alpha = im.atan2(re).to_degrees();
            let utrn_h = {
                let u = num(tt, tr, "utrn_h", base_h, self.decimal)?;
                if u.abs() < 1e-9 { base_h } else { u }
            };
            let utrn_l = {
                let u = num(tt, tr, "utrn_l", base_l, self.decimal)?;
                if u.abs() < 1e-9 { base_l } else { u }
            };
            // The nominal-voltage ratio adjustment; identity when any base or
            // rated voltage is unknown (0), so the tap changer alone sets the ratio.
            let ratio_kv = if base_h > 1e-9 && base_l > 1e-9 && utrn_h > 1e-9 && utrn_l > 1e-9 {
                (utrn_h / base_h) / (utrn_l / base_l)
            } else {
                1.0
            };
            let (tap, shift) = if tap_side == 1 {
                (ratio_kv / ratio_t, -alpha + nt2ag * 30.0)
            } else {
                (ratio_kv * ratio_t, alpha + nt2ag * 30.0)
            };

            let ratfac = num(t, r, "ratfac", 1.0, self.decimal)?;
            let mut branch = Branch::new(from_bus, to_bus, rpu, xpu);
            branch.tap = if tap.abs() < 1e-12 { 1.0 } else { tap };
            branch.shift = shift;
            branch.rate_a = strn * ratfac;
            if g_fr.abs() > 1e-15 || b_fr.abs() > 1e-15 {
                branch.charging = Some(BranchCharging::new(g_fr, b_fr, 0.0, 0.0));
            }
            branch.in_service = cf && ct && int(t, r, "outserv", 0, self.decimal)? == 0;
            branch.uid = Some(id.clone());
            branch.extras = Self::extras(t, r, &["nntap", "ratfac", "outserv", "typ_id"]);
            branches.push(branch);
        }
        Ok(())
    }

    fn transformers_3w<F: Fn(usize, &Topology) -> BusId>(
        &mut self,
        topo: &Topology,
        bus_of: &F,
        transformers_3w: &mut Vec<Transformer3W>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmTr3") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let conns = topo.conns.get(&id);
            let side = |s: i64| -> Option<usize> {
                conns.and_then(|c| c.iter().find(|c| c.side == s).map(|c| c.term_pos))
            };
            let (Some(hv), Some(mv), Some(lv)) = (side(0), side(1), side(2)) else {
                self.skip_unconnected("3-winding transformer", &id);
                continue;
            };
            let typ = ptr(t, r, "typ_id").unwrap_or("");
            let (tt, tr) = self
                .resolve(typ)
                .filter(|(tt, _)| tt.class == "TypTr3")
                .ok_or_else(|| Self::missing_type(&id, typ))?;

            let pair = |uk: &str, sh: f64, sl: f64| -> Result<Impedance> {
                let base = sh.min(sl);
                let base = if base.abs() < 1e-9 { BASE_MVA } else { base };
                let z = num(tt, tr, uk, 0.0, self.decimal)? / 100.0 * (BASE_MVA / base);
                Ok(Impedance::new(0.0, z, base))
            };
            let sh = num(tt, tr, "strn3_h", BASE_MVA, self.decimal)?;
            let sm = num(tt, tr, "strn3_m", BASE_MVA, self.decimal)?;
            let sl = num(tt, tr, "strn3_l", BASE_MVA, self.decimal)?;
            let z12 = pair("uktr3_h", sh, sm)?;
            let z23 = pair("uktr3_m", sm, sl)?;
            let z31 = pair("uktr3_l", sl, sh)?;

            let winding = |bus_pos: usize, kv: &str, rate: f64, tapc: f64, clock: f64| Winding {
                bus: bus_of(bus_pos, topo),
                tap: tapc,
                shift: clock * 30.0,
                nominal_kv: num(tt, tr, kv, 0.0, self.decimal).unwrap_or(0.0),
                rate_a: rate,
                rate_b: 0.0,
                rate_c: 0.0,
            };
            let windings = [
                winding(hv, "utrn3_h", sh, 1.0, 0.0),
                winding(
                    mv,
                    "utrn3_m",
                    sm,
                    1.0,
                    num(tt, tr, "nt3ag_m", 0.0, self.decimal)?,
                ),
                winding(
                    lv,
                    "utrn3_l",
                    sl,
                    1.0,
                    num(tt, tr, "nt3ag_l", 0.0, self.decimal)?,
                ),
            ];
            let mut t3 = Transformer3W::new(windings, [z12, z23, z31]);
            t3.in_service = int(t, r, "outserv", 0, self.decimal)? == 0;
            t3.name = text(t, r, "loc_name").map(str::to_owned);
            t3.uid = Some(id.clone());
            transformers_3w.push(t3);
        }
        Ok(())
    }

    fn pick_slack(generators: &[Generator]) -> Option<BusId> {
        generators
            .iter()
            .filter(|g| g.in_service)
            .max_by(|a, b| {
                a.pmax
                    .partial_cmp(&b.pmax)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|g| g.bus)
    }

    fn skip_unconnected(&mut self, what: &str, id: &str) {
        self.warnings
            .push(format!("{what} {id:?} has no connected cubicle; skipped"));
    }

    fn missing_type(element: &str, typ: &str) -> Error {
        Error::FormatRead {
            format: FMT,
            message: format!(
                "line/transformer {element:?} references type {typ:?} that is not in the file; \
                 re-export from PowerFactory with types included"
            ),
        }
    }

    /// Unmapped, non-structural columns of a mapped element, as `dgs_<attr>`.
    fn extras(t: &DgsTable, r: &DgsRow, consumed: &[&str]) -> Extras {
        let mut extras = Extras::new();
        for (i, col) in t.columns.iter().enumerate() {
            if is_structural(col) || consumed.contains(&col.as_str()) {
                continue;
            }
            if let Some(Some(v)) = r.cells.get(i) {
                extras.insert(format!("dgs_{col}"), Value::String(v.clone()));
            }
        }
        extras
    }

    /// Names of the unmapped, non-structural columns with a value (for a
    /// generator, which has no extras map).
    fn dropped_attrs(t: &DgsTable, r: &DgsRow, consumed: &[&str]) -> Vec<String> {
        t.columns
            .iter()
            .enumerate()
            .filter(|(i, col)| {
                !is_structural(col)
                    && !consumed.contains(&col.as_str())
                    && matches!(r.cells.get(*i), Some(Some(_)))
            })
            .map(|(_, col)| col.clone())
            .collect()
    }

    /// One warning per unmapped class with a row count.
    fn warn_ignored_classes(&mut self) {
        for t in &self.doc.tables {
            if t.rows.is_empty() || MAPPED_CLASSES.contains(&t.class.as_str()) {
                continue;
            }
            self.warnings.push(format!(
                "`{}` table ignored ({} rows): not mapped",
                t.class,
                t.rows.len()
            ));
        }
    }
}

/// Structural columns present on every element row.
fn is_structural(name: &str) -> bool {
    matches!(
        name,
        "ID" | "FID" | "OP" | "loc_name" | "fold_id" | "chr_name"
    )
}

/// Columns the machine converters consume (never routed to extras/warnings).
const GEN_CONSUMED: &[&str] = &[
    "typ_id", "i_mot", "iv_mode", "av_mode", "ip_ctrl", "ngnum", "scale0", "outserv", "pgini",
    "qgini", "usetp", "q_max", "q_min", "cQ_max", "cQ_min", "Pmax_uc", "Pmin_uc", "snss", "bustp",
];

/// Classes the reader maps or consumes structurally; everything else is warned.
const MAPPED_CLASSES: &[&str] = &[
    "General",
    "ElmNet",
    "ElmTerm",
    "ElmLod",
    "ElmLodlv",
    "ElmLodmv",
    "ElmShnt",
    "ElmSym",
    "ElmGenstat",
    "ElmPvsys",
    "ElmAsm",
    "ElmXnet",
    "ElmZpu",
    "ElmSind",
    "ElmScap",
    "ElmLne",
    "ElmLnesec",
    "ElmCoup",
    "ElmTr2",
    "ElmTr3",
    "TypLod",
    "TypLne",
    "TypSym",
    "TypAsmo",
    "TypAsm",
    "TypTr2",
    "TypTr3",
    "StaCubic",
    "StaSwitch",
];

/// `a / b`, or `a` when `b` is ~0 (identity for a missing base).
fn safe_div(a: f64, b: f64) -> f64 {
    if b.abs() < 1e-12 { a } else { a / b }
}

/// Parse retained source from the format hub.
pub(crate) fn parse_dgs_source(
    source: Arc<String>,
    name_hint: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Network> {
    sniff_binary(&source)?;
    let doc = scan(&source, warnings)?;
    let mut net = Reader::build(&doc, name_hint, warnings)?;
    net.source = Some(source);
    Ok(net)
}

/// Parse a DIgSILENT DGS case into a [`Network`].
///
/// Read warnings are available through the shared [`crate::parse_file`] /
/// [`crate::parse_str`] entry points. This direct helper returns only the typed
/// network.
pub fn parse_dgs(content: &str) -> Result<Network> {
    let mut warnings = Vec::new();
    parse_dgs_source(Arc::new(content.to_owned()), None, &mut warnings)
}

// ---- Writer ----------------------------------------------------------------

/// Append a formatted row terminated with CRLF (PowerFactory export convention).
macro_rules! line {
    ($s:expr, $($arg:tt)*) => {{
        let _ = write!($s, $($arg)*);
        $s.push_str("\r\n");
    }};
}

/// A finite `f64` in shortest round-trip form.
fn fmt_f64(x: f64) -> String {
    format!("{x}")
}

/// The `sqrt(3)` line-rating factor.
fn root3() -> f64 {
    3.0_f64.sqrt()
}

/// A collected cubicle connection to emit as a `StaCubic` row.
struct CubicleOut {
    terminal: String,
    side: i64,
    element: String,
}

/// Serialize `net` to DGS 7.0 ASCII text.
///
/// The inverse of the reader's column layout: each element becomes its DGS class
/// row plus a per-branch `Typ*` and the `StaCubic` rows that wire it to its
/// terminals, so a `.dgs` -> [`Network`] -> `.dgs` round trip preserves the power
/// flow core. Output is deterministic (no timestamps) and uses CRLF line endings
/// to match PowerFactory exports. Same-format byte-exact echo rides the retained
/// source (see [`crate::write_as`]); this is the cross-format path.
#[must_use]
pub fn write_dgs(net: &Network) -> Conversion {
    Writer::new(net).run()
}

struct Writer<'a> {
    net: &'a Network,
    out: String,
    cubicles: Vec<CubicleOut>,
    warnings: Vec<String>,
    used_fids: HashSet<String>,
    bus_fid: HashMap<BusId, String>,
    bus_kv: HashMap<BusId, f64>,
    counter: usize,
    sanitized: usize,
    nonfinite: bool,
}

impl<'a> Writer<'a> {
    fn new(net: &'a Network) -> Self {
        Writer {
            net,
            out: String::new(),
            cubicles: Vec::new(),
            warnings: Vec::new(),
            used_fids: HashSet::new(),
            bus_fid: HashMap::new(),
            bus_kv: HashMap::new(),
            counter: 0,
            sanitized: 0,
            nonfinite: false,
        }
    }

    fn num(&mut self, x: f64) -> String {
        if x.is_finite() {
            fmt_f64(x)
        } else {
            self.nonfinite = true;
            fmt_f64(if x > 0.0 {
                1e10
            } else if x < 0.0 {
                -1e10
            } else {
                0.0
            })
        }
    }

    fn next(&mut self) -> usize {
        self.counter += 1;
        self.counter
    }

    /// A sanitized, deduplicated, ≤40-char FID from `raw` (or a synthesized
    /// `prefix` id when empty).
    fn fid(&mut self, raw: &str, prefix: &str) -> String {
        let base: String = if raw.trim().is_empty() {
            format!("{prefix}{}", self.next())
        } else {
            let clean = sanitize_quoted(raw.trim(), FID_FORBIDDEN, '_');
            if matches!(clean, std::borrow::Cow::Owned(_)) {
                self.sanitized += 1;
            }
            clean.chars().take(40).collect()
        };
        let mut candidate = base.clone();
        let mut n = 1u32;
        while !self.used_fids.insert(candidate.clone()) {
            let stem: String = base.chars().take(34).collect();
            candidate = format!("{stem}_{n}");
            n += 1;
        }
        candidate
    }

    /// A sanitized, ≤40-char display name.
    fn name(&mut self, raw: &str) -> String {
        let clean = sanitize_quoted(raw, FID_FORBIDDEN, '_');
        if matches!(clean, std::borrow::Cow::Owned(_)) {
            self.sanitized += 1;
        }
        clean.chars().take(40).collect()
    }

    fn cubicle(&mut self, terminal: &str, side: i64, element: &str) {
        self.cubicles.push(CubicleOut {
            terminal: terminal.to_string(),
            side,
            element: element.to_string(),
        });
    }

    fn run(mut self) -> Conversion {
        // Header banner (stable text, no embedded date, so output is byte-stable).
        self.out.push_str(
            "********************************************************************************\r\n\
             *\r\n\
             * powerio DGS export\r\n\
             *\r\n\
             ********************************************************************************\r\n\
             \r\n",
        );
        line!(self.out, "$$General;FID(a:40);Descr(a:40);Val(a:40)");
        line!(self.out, "1;Version;7.0");
        self.out.push_str("\r\n");

        // Grid.
        let net_name = self.name(&self.net.name.clone());
        let freq = self.num(self.net.base_frequency);
        line!(
            self.out,
            "$$ElmNet;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);frnom(r)"
        );
        line!(self.out, "Grid;C;{net_name};;{freq}");
        self.out.push_str("\r\n");

        // Terminals (buses). FIDs and kv map first, then rows.
        for b in &self.net.buses {
            let fid = self.fid(b.uid.as_deref().unwrap_or(""), "TERM");
            self.bus_fid.insert(b.id, fid);
            self.bus_kv.insert(b.id, b.base_kv);
        }
        line!(
            self.out,
            "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);iUsage(i);outserv(i);uknom(r)"
        );
        for b in &self.net.buses {
            let fid = self.bus_fid[&b.id].clone();
            let name = self.name(b.name.as_deref().unwrap_or(&fid));
            let outserv = i32::from(b.kind == BusType::Isolated);
            let uknom = self.num(b.base_kv);
            line!(self.out, "{fid};C;{name};Grid;0;{outserv};{uknom}");
        }
        self.out.push_str("\r\n");

        self.write_external_grid();
        self.write_machines();
        self.write_loads();
        self.write_shunts();
        self.write_lines();
        self.write_transformers_2w();
        self.write_transformers_3w();
        self.write_couplers();
        self.write_cubicles();
        self.write_warnings();

        Conversion {
            text: self.out,
            warnings: self.warnings,
        }
    }

    /// The reference bus and the index of its first generator (if any).
    fn slack(&self) -> (Option<BusId>, Option<usize>) {
        let ref_bus = self
            .net
            .buses
            .iter()
            .find(|b| b.kind == BusType::Ref)
            .map(|b| b.id);
        let gen_idx = ref_bus.and_then(|rb| self.net.generators.iter().position(|g| g.bus == rb));
        (ref_bus, gen_idx)
    }

    fn write_external_grid(&mut self) {
        let (ref_bus, gen_idx) = self.slack();
        let Some(ref_bus) = ref_bus else {
            return;
        };
        line!(
            self.out,
            "$$ElmXnet;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);bustp(a:2);outserv(i);pgini(r);qgini(r);usetp(r);snss(r)"
        );
        let (uid, pg, qg, vg, in_service) = if let Some(i) = gen_idx {
            let g = &self.net.generators[i];
            (g.uid.clone(), g.pg, g.qg, g.vg, g.in_service)
        } else {
            let vm = self
                .net
                .buses
                .iter()
                .find(|b| b.id == ref_bus)
                .map_or(1.0, |b| b.vm);
            (None, 0.0, 0.0, vm, true)
        };
        let fid = self.fid(uid.as_deref().unwrap_or(""), "XNET");
        let name = fid.clone();
        let (pgs, qgs, vgs) = (self.num(pg), self.num(qg), self.num(vg));
        let outserv = i32::from(!in_service);
        line!(
            self.out,
            "{fid};C;{name};Grid;SL;{outserv};{pgs};{qgs};{vgs};10000"
        );
        let term = self.bus_fid[&ref_bus].clone();
        self.cubicle(&term, 0, &fid);
        self.out.push_str("\r\n");
    }

    fn write_machines(&mut self) {
        let (_, xnet_idx) = self.slack();
        let gens: Vec<usize> = (0..self.net.generators.len())
            .filter(|i| Some(*i) != xnet_idx)
            .collect();
        if gens.is_empty() {
            return;
        }
        // Emit the ElmSym rows and collect the per-generator type params.
        let mut types: Vec<(String, f64, f64, f64)> = Vec::new();
        line!(
            self.out,
            "$$ElmSym;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);typ_id(p);i_mot(i);iv_mode(i);ngnum(i);outserv(i);pgini(r);qgini(r);usetp(r);q_min(r);q_max(r)"
        );
        for &i in &gens {
            let g = &self.net.generators[i];
            let (bus, pg, qg, vg, pmax, qmin, qmax, mbase, in_service) = (
                g.bus,
                g.pg,
                g.qg,
                g.vg,
                g.pmax,
                g.qmin,
                g.qmax,
                g.mbase,
                g.in_service,
            );
            let bus_kv = self.bus_kv.get(&bus).copied().unwrap_or(0.0);
            let bus_kind = self.net.buses.iter().find(|b| b.id == bus).map(|b| b.kind);
            let sgn = if mbase > 0.0 {
                mbase
            } else {
                pmax.abs().max(pg.abs()).max(1.0)
            };
            let cosn = if pmax > 0.0 && sgn > 0.0 {
                (pmax / sgn).clamp(0.01, 1.0)
            } else {
                0.8
            };
            let fid = self.fid(g.uid.as_deref().unwrap_or(""), "SYM");
            let type_fid = self.fid(&format!("T{fid}"), "TSYM");
            let name = fid.clone();
            let iv_mode = i32::from(matches!(bus_kind, Some(BusType::Pv | BusType::Ref)));
            let outserv = i32::from(!in_service);
            let (pgs, qgs, vgs) = (self.num(pg), self.num(qg), self.num(vg));
            let qmins = self.num(safe_div(qmin, sgn));
            let qmaxs = self.num(safe_div(qmax, sgn));
            line!(
                self.out,
                "{fid};C;{name};Grid;{type_fid};0;{iv_mode};1;{outserv};{pgs};{qgs};{vgs};{qmins};{qmaxs}"
            );
            let term = self.bus_fid[&bus].clone();
            self.cubicle(&term, 0, &fid);
            types.push((type_fid, cosn, sgn, bus_kv));
        }
        self.out.push_str("\r\n");
        line!(
            self.out,
            "$$TypSym;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);cosn(r);sgn(r);ugn(r)"
        );
        for (fid, cosn, sgn, ugn) in types {
            let (cosns, sgns, ugns) = (self.num(cosn), self.num(sgn), self.num(ugn));
            line!(self.out, "{fid};C;{fid};Grid;{cosns};{sgns};{ugns}");
        }
        self.out.push_str("\r\n");
    }

    fn write_loads(&mut self) {
        if self.net.loads.is_empty() {
            return;
        }
        line!(
            self.out,
            "$$ElmLod;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);outserv(i);plini(r);qlini(r);scale0(r)"
        );
        for l in &self.net.loads {
            let (bus, p, q, in_service) = (l.bus, l.p, l.q, l.in_service);
            let fid = self.fid(l.uid.as_deref().unwrap_or(""), "LOD");
            let name = fid.clone();
            let outserv = i32::from(!in_service);
            let (ps, qs) = (self.num(p), self.num(q));
            line!(self.out, "{fid};C;{name};Grid;{outserv};{ps};{qs};1");
            let term = self.bus_fid[&bus].clone();
            self.cubicle(&term, 0, &fid);
        }
        self.out.push_str("\r\n");
    }

    fn write_shunts(&mut self) {
        if self.net.shunts.is_empty() {
            return;
        }
        line!(
            self.out,
            "$$ElmShnt;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);shtype(i);ncapa(i);ncapx(i);outserv(i);qtotn(r);ushnm(r)"
        );
        for sh in &self.net.shunts {
            let (bus, g, b, in_service) = (sh.bus, sh.g, sh.b, sh.in_service);
            let base_kv = self.bus_kv.get(&bus).copied().unwrap_or(0.0);
            let ushnm = if base_kv.abs() < 1e-9 { 1.0 } else { base_kv };
            let shtype = if b < 0.0 { 1 } else { 2 };
            let fid = self.fid(sh.uid.as_deref().unwrap_or(""), "SHNT");
            let name = fid.clone();
            let outserv = i32::from(!in_service);
            let qtotn = self.num(b.abs());
            let ushnms = self.num(ushnm);
            line!(
                self.out,
                "{fid};C;{name};Grid;{shtype};1;1;{outserv};{qtotn};{ushnms}"
            );
            let term = self.bus_fid[&bus].clone();
            self.cubicle(&term, 0, &fid);
            let _ = g;
        }
        self.out.push_str("\r\n");
    }

    fn write_lines(&mut self) {
        let lines: Vec<usize> = (0..self.net.branches.len())
            .filter(|&i| !self.net.branches[i].is_transformer())
            .collect();
        if lines.is_empty() {
            return;
        }
        let f = self.net.base_frequency;
        let mut types: Vec<(String, f64, f64, f64, f64, f64)> = Vec::new();
        line!(
            self.out,
            "$$ElmLne;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);typ_id(p);dline(r);fline(r);outserv(i)"
        );
        for &i in &lines {
            let br = &self.net.branches[i];
            let (from, to, r, x, b, rate_a, in_service) = (
                br.from,
                br.to,
                br.r,
                br.x,
                br.legacy_total_charging_b(),
                br.rate_a,
                br.in_service,
            );
            let base_kv = self.bus_kv.get(&from).copied().unwrap_or(0.0);
            let z_base = zbase(base_kv, BASE_MVA);
            let rline = r * z_base;
            let xline = x * z_base;
            let cline = b / (z_base * std::f64::consts::TAU * f) * 1e6;
            // A nominal 1 kV rating when the bus base is unknown keeps rate_a
            // recoverable (the reader's Z_base uses the bus base, not `uline`).
            let uline = if base_kv.abs() > 1e-9 { base_kv } else { 1.0 };
            let sline = rate_a / (root3() * uline);
            let fid = self.fid(br.uid.as_deref().unwrap_or(""), "LNE");
            let type_fid = self.fid(&format!("T{fid}"), "TLNE");
            let name = fid.clone();
            let outserv = i32::from(!in_service);
            line!(self.out, "{fid};C;{name};Grid;{type_fid};1;1;{outserv}");
            let (ft, tt) = (self.bus_fid[&from].clone(), self.bus_fid[&to].clone());
            self.cubicle(&ft, 0, &fid);
            self.cubicle(&tt, 1, &fid);
            types.push((type_fid, rline, xline, cline, uline, sline));
        }
        self.out.push_str("\r\n");
        line!(
            self.out,
            "$$TypLne;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);rline(r);xline(r);cline(r);uline(r);sline(r);nlnph(i)"
        );
        for (fid, rline, xline, cline, uline, sline) in types {
            let (rl, xl, cl, ul, sl) = (
                self.num(rline),
                self.num(xline),
                self.num(cline),
                self.num(uline),
                self.num(sline),
            );
            line!(self.out, "{fid};C;{fid};Grid;{rl};{xl};{cl};{ul};{sl};3");
        }
        self.out.push_str("\r\n");
    }

    fn write_transformers_2w(&mut self) {
        let xfmrs: Vec<usize> = (0..self.net.branches.len())
            .filter(|&i| self.net.branches[i].is_transformer())
            .collect();
        if xfmrs.is_empty() {
            return;
        }
        // Type params: strn, uktr, pcutr, curmg, pfe, dutap, nntap0, phitr,
        // tap_side, nt2ag, utrn_h, utrn_l.
        #[allow(clippy::type_complexity)]
        let mut types: Vec<(String, [f64; 12])> = Vec::new();
        line!(
            self.out,
            "$$ElmTr2;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);typ_id(p);nntap(i);outserv(i);ratfac(r)"
        );
        for &i in &xfmrs {
            let br = &self.net.branches[i];
            let (from, to, r, x, rate_a, tap, shift, in_service) = (
                br.from,
                br.to,
                br.r,
                br.x,
                br.rate_a,
                br.effective_tap(),
                br.shift,
                br.in_service,
            );
            let charging = br.charging;
            let base_h = self.bus_kv.get(&from).copied().unwrap_or(0.0);
            let base_l = self.bus_kv.get(&to).copied().unwrap_or(0.0);

            let strn = if rate_a > 0.0 { rate_a } else { BASE_MVA };
            let ratfac = safe_div(rate_a, strn);
            let z_pu = (r * r + x * x).sqrt();
            let uktr = z_pu * strn;
            let pcutr = r * strn * strn * 10.0;

            // Magnetizing from the from-side charging (0 for a plain transformer).
            let (g_fr, b_fr) = charging.map_or((0.0, 0.0), |c| (c.g_fr, c.b_fr));
            let (curmg, pfe) = if g_fr.abs() > 1e-15 || b_fr.abs() > 1e-15 {
                (1e4 * (g_fr * g_fr + b_fr * b_fr).sqrt() / strn, g_fr * 1e5)
            } else {
                (0.0, 0.0)
            };

            // Tap: d·e^{jφ} = tap·e^{jshift} − 1, encoded with one step at HV.
            let shift_rad = shift.to_radians();
            let re = tap * shift_rad.cos() - 1.0;
            let im = tap * shift_rad.sin();
            let d = (re * re + im * im).sqrt();
            let phitr = im.atan2(re).to_degrees();
            let dutap = 100.0 * d;

            let fid = self.fid(br.uid.as_deref().unwrap_or(""), "TR2");
            let type_fid = self.fid(&format!("T{fid}"), "TTR2");
            let name = fid.clone();
            let outserv = i32::from(!in_service);
            let ratfacs = self.num(ratfac);
            line!(
                self.out,
                "{fid};C;{name};Grid;{type_fid};1;{outserv};{ratfacs}"
            );
            let (ft, tt) = (self.bus_fid[&from].clone(), self.bus_fid[&to].clone());
            self.cubicle(&ft, 0, &fid);
            self.cubicle(&tt, 1, &fid);
            types.push((
                type_fid,
                [
                    strn, uktr, pcutr, curmg, pfe, dutap, 0.0, phitr, 0.0, 0.0, base_h, base_l,
                ],
            ));
        }
        self.out.push_str("\r\n");
        line!(
            self.out,
            "$$TypTr2;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);strn(r);uktr(r);pcutr(r);curmg(r);pfe(r);dutap(r);nntap0(i);phitr(r);tap_side(i);nt2ag(r);utrn_h(r);utrn_l(r)"
        );
        for (fid, p) in types {
            let vals: Vec<String> = p.iter().map(|&v| self.num(v)).collect();
            // nntap0 (index 6) and tap_side (index 8) are integers.
            line!(
                self.out,
                "{fid};C;{fid};Grid;{};{};{};{};{};{};0;{};0;{};{};{}",
                vals[0],
                vals[1],
                vals[2],
                vals[3],
                vals[4],
                vals[5],
                vals[7],
                vals[9],
                vals[10],
                vals[11]
            );
        }
        self.out.push_str("\r\n");
    }

    fn write_transformers_3w(&mut self) {
        if self.net.transformers_3w.is_empty() {
            return;
        }
        #[allow(clippy::type_complexity)]
        let mut types: Vec<(String, [f64; 11])> = Vec::new();
        line!(
            self.out,
            "$$ElmTr3;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);typ_id(p);outserv(i)"
        );
        for tr in &self.net.transformers_3w {
            let [wh, wm, wl] = [&tr.windings[0], &tr.windings[1], &tr.windings[2]];
            let (bh, bm, bl) = (wh.bus, wm.bus, wl.bus);
            let (sh, sm, sl) = (wh.rate_a, wm.rate_a, wl.rate_a);
            let strn_h = if sh > 0.0 { sh } else { BASE_MVA };
            let strn_m = if sm > 0.0 { sm } else { BASE_MVA };
            let strn_l = if sl > 0.0 { sl } else { BASE_MVA };
            // Pairwise reactance (r folded in) back to percent on min-pair base.
            let [z12, z23, z31] = tr.z;
            let uk_h = pair_uk(z12, strn_h.min(strn_m));
            let uk_m = pair_uk(z23, strn_m.min(strn_l));
            let uk_l = pair_uk(z31, strn_l.min(strn_h));
            let (uh, um, ul) = (wh.nominal_kv, wm.nominal_kv, wl.nominal_kv);
            let nt3ag_m = safe_div(wm.shift, 30.0);
            let nt3ag_l = safe_div(wl.shift, 30.0);
            let in_service = tr.in_service;

            let fid = self.fid(tr.uid.as_deref().unwrap_or(""), "TR3");
            let type_fid = self.fid(&format!("T{fid}"), "TTR3");
            let name = self.name(tr.name.as_deref().unwrap_or(&fid));
            let outserv = i32::from(!in_service);
            line!(self.out, "{fid};C;{name};Grid;{type_fid};{outserv}");
            let (th, tm, tl) = (
                self.bus_fid[&bh].clone(),
                self.bus_fid[&bm].clone(),
                self.bus_fid[&bl].clone(),
            );
            self.cubicle(&th, 0, &fid);
            self.cubicle(&tm, 1, &fid);
            self.cubicle(&tl, 2, &fid);
            types.push((
                type_fid,
                [
                    strn_h, strn_m, strn_l, uk_h, uk_m, uk_l, uh, um, ul, nt3ag_m, nt3ag_l,
                ],
            ));
        }
        self.out.push_str("\r\n");
        line!(
            self.out,
            "$$TypTr3;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);strn3_h(r);strn3_m(r);strn3_l(r);uktr3_h(r);uktr3_m(r);uktr3_l(r);utrn3_h(r);utrn3_m(r);utrn3_l(r);nt3ag_m(r);nt3ag_l(r)"
        );
        for (fid, p) in types {
            let v: Vec<String> = p.iter().map(|&x| self.num(x)).collect();
            line!(
                self.out,
                "{fid};C;{fid};Grid;{};{};{};{};{};{};{};{};{};{};{}",
                v[0],
                v[1],
                v[2],
                v[3],
                v[4],
                v[5],
                v[6],
                v[7],
                v[8],
                v[9],
                v[10]
            );
        }
        self.out.push_str("\r\n");
    }

    fn write_couplers(&mut self) {
        if self.net.switches.is_empty() {
            return;
        }
        line!(
            self.out,
            "$$ElmCoup;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);on_off(i)"
        );
        for sw in &self.net.switches {
            let (from, to, closed) = (sw.from, sw.to, sw.closed);
            let fid = self.fid(sw.uid.as_deref().unwrap_or(""), "COUP");
            let name = fid.clone();
            let on = i32::from(closed);
            line!(self.out, "{fid};C;{name};Grid;{on}");
            let (ft, tt) = (self.bus_fid[&from].clone(), self.bus_fid[&to].clone());
            self.cubicle(&ft, 0, &fid);
            self.cubicle(&tt, 1, &fid);
        }
        self.out.push_str("\r\n");
    }

    fn write_cubicles(&mut self) {
        if self.cubicles.is_empty() {
            return;
        }
        line!(
            self.out,
            "$$StaCubic;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);obj_bus(i);obj_id(p)"
        );
        // A dedicated counter (not the shared element FID counter, which a
        // second write consumes differently once elements carry uids) keeps the
        // cubicle numbering stable across a read/write round trip.
        let cubs = std::mem::take(&mut self.cubicles);
        let mut n = 0usize;
        for c in cubs {
            n += 1;
            let mut fid = format!("CUB{n}");
            while !self.used_fids.insert(fid.clone()) {
                n += 1;
                fid = format!("CUB{n}");
            }
            line!(
                self.out,
                "{fid};C;{fid};{};{};{}",
                c.terminal,
                c.side,
                c.element
            );
        }
        self.out.push_str("\r\n");
    }

    #[expect(clippy::too_many_lines)]
    fn write_warnings(&mut self) {
        let net = self.net;
        if net.generators.iter().any(|g| g.cost.is_some()) {
            self.warnings
                .push("generator cost curves dropped: DGS carries no cost model".into());
        }
        if net.generators.iter().any(Generator::has_caps) {
            self.warnings.push(
                "generator capability/ramp columns dropped: DGS has no field for them".into(),
            );
        }
        let wide_limits = net.generators.iter().any(|g| {
            net.buses
                .iter()
                .any(|b| b.id == g.bus && b.kind == BusType::Ref)
        });
        if wide_limits {
            self.warnings.push(
                "reference generator written as ElmXnet: its P/Q limits are not represented \
                 (DGS external grids state short-circuit power, not dispatch limits)"
                    .into(),
            );
        }
        if !net.storage.is_empty() {
            self.warnings.push(format!(
                "{} storage unit(s) dropped: DGS has no storage record",
                net.storage.len()
            ));
        }
        if !net.hvdc.is_empty() {
            self.warnings.push(format!(
                "{} HVDC line(s) dropped: this DGS writer emits AC elements only",
                net.hvdc.len()
            ));
        }
        if !net.areas.is_empty() {
            self.warnings.push(format!(
                "{} area record(s) dropped: DGS has no area interchange model",
                net.areas.len()
            ));
        }
        let angle = net.branches.iter().filter(|b| b.has_angle_limits()).count();
        if angle > 0 {
            self.warnings.push(format!(
                "{angle} branch angle-difference limit(s) dropped: DGS has no field for them"
            ));
        }
        let ratings = net
            .branches
            .iter()
            .filter(|b| {
                (b.rate_b.abs() > f64::EPSILON && (b.rate_b - b.rate_a).abs() > f64::EPSILON)
                    || (b.rate_c.abs() > f64::EPSILON && (b.rate_c - b.rate_a).abs() > f64::EPSILON)
            })
            .count();
        if ratings > 0 {
            self.warnings.push(format!(
                "{ratings} branch(es) have rate_b/rate_c beyond rate_a: DGS carries one thermal \
                 rating per branch"
            ));
        }
        warn_extra_branch_rating_sets(FMT, net, &mut self.warnings);
        let vmodels = net
            .loads
            .iter()
            .filter(|l| l.voltage_model.is_some())
            .count();
        if vmodels > 0 {
            self.warnings.push(format!(
                "{vmodels} load voltage model(s) written as constant power: this DGS writer emits \
                 no TypLod"
            ));
        }
        let g_shunts = net
            .shunts
            .iter()
            .filter(|s| s.g.abs() > f64::EPSILON)
            .count();
        if g_shunts > 0 {
            self.warnings.push(format!(
                "{g_shunts} shunt conductance value(s) dropped: the DGS shunt record written here \
                 carries reactive power only"
            ));
        }
        let sw_shunts = net.shunts.iter().filter(|s| s.control.is_some()).count();
        if sw_shunts > 0 {
            self.warnings.push(format!(
                "{sw_shunts} switched shunt(s) written as fixed: the DGS shunt record written here \
                 has no controller band"
            ));
        }
        let control = net.branches.iter().filter(|b| b.control.is_some()).count();
        if control > 0 {
            self.warnings.push(format!(
                "{control} transformer(s) lost their regulating control: the DGS transformer record \
                 written here carries a fixed tap"
            ));
        }
        let xfmr_charge = net
            .branches
            .iter()
            .filter(|b| b.is_transformer() && b.legacy_total_charging_b().abs() > f64::EPSILON)
            .count();
        if xfmr_charge > 0 {
            self.warnings.push(format!(
                "{xfmr_charge} transformer(s) carry line charging that a DGS transformer cannot \
                 represent (its shunt is inductive magnetizing only); the charging was dropped"
            ));
        }
        if self.sanitized > 0 {
            self.warnings.push(format!(
                "{} name/FID(s) contained a DGS metacharacter and were sanitized to '_'",
                self.sanitized
            ));
        }
        if self.nonfinite {
            self.warnings
                .push("non-finite values written as ±1e10 sentinels (DGS has no Inf/NaN)".into());
        }
    }
}

/// Pairwise short-circuit voltage percent from an [`Impedance`] on the system
/// base, expressed on the winding-pair `base` MVA (inverse of the reader's
/// `uk/100·(100/base)`).
fn pair_uk(z: Impedance, base: f64) -> f64 {
    let zmag = (z.r * z.r + z.x * z.x).sqrt();
    let base = if base.abs() < 1e-9 { BASE_MVA } else { base };
    zmag * 100.0 * base / BASE_MVA
}
