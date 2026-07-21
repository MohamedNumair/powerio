//! Layer 1: the DGS table scanner and typed cell accessors.
//!
//! A DGS file is a flat list of class tables. Each table opens with a header
//! line `$$Class;attr(type);attr(type);…` and is followed by semicolon
//! separated data rows; lines starting with `*` are comments. Column types
//! are `i` (integer), `r`/`d` (real), `a[:N]` (string), and `p` (object
//! pointer). The three ASCII generations differ only in shape: v5 keys rows by
//! a numeric `ID`, v6/v7 by a string `FID` plus an `OP` operation flag
//! (`C`/`M` create, `U` update, `D` delete, `I` ignore). Pointer values that
//! start with `##` are foreign keys into a project that is not in the file.
//!
//! This is a self-contained re-implementation of the scanner in the balanced
//! reader (`powerio/src/format/dgs.rs`); powerio-dist keeps its zero
//! internal-crate dependency, so the two scanners share behavior, not code.

use std::collections::{HashMap, HashSet};

use super::FMT;
use crate::error::{Error, Result};

/// One data row: its source line (for error context) and one cell per column,
/// `None` for an empty or `$empty$` cell.
pub(crate) struct DgsRow {
    pub(crate) line_no: usize,
    pub(crate) cells: Vec<Option<String>>,
}

/// One class table. Columns are kept as bare names; the declared type only
/// gates the unknown-type warning at scan time, and numeric parsing is on
/// demand.
pub(crate) struct DgsTable {
    pub(crate) class: String,
    pub(crate) columns: Vec<String>,
    /// Column name to index, for the typed accessors.
    index: HashMap<String, usize>,
    /// The identity column: `ID`/`FID`, else column 0.
    id_col: usize,
    op_col: Option<usize>,
    pub(crate) rows: Vec<DgsRow>,
}

impl DgsTable {
    pub(crate) fn col(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    pub(crate) fn id_of(&self, row: &DgsRow) -> String {
        self.id_cell(row).to_string()
    }

    /// The identity cell as a borrowed, trimmed slice (for the object index).
    pub(crate) fn id_cell<'a>(&self, row: &'a DgsRow) -> &'a str {
        row.cells
            .get(self.id_col)
            .and_then(|c| c.as_deref())
            .unwrap_or("")
            .trim()
    }
}

/// The scanned document plus the configuration read from `General`.
pub(crate) struct DgsDoc {
    pub(crate) tables: Vec<DgsTable>,
    by_class: HashMap<String, usize>,
    pub(crate) decimal: char,
}

impl DgsDoc {
    pub(crate) fn table(&self, class: &str) -> Option<&DgsTable> {
        self.by_class.get(class).map(|&i| &self.tables[i])
    }

    pub(crate) fn rows(&self, class: &str) -> &[DgsRow] {
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
pub(crate) fn scan(source: &str, warnings: &mut Vec<String>) -> Result<DgsDoc> {
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
/// pick up the decimal separator and attribute mode.
fn read_config(tables: &[DgsTable], warnings: &mut Vec<String>) -> Result<char> {
    let general = tables
        .iter()
        .find(|t| t.class == "General")
        .ok_or(Error::FormatRead {
            format: FMT,
            message: "no General table; not a DGS export".into(),
        })?;
    let (Some(descr), Some(val)) = (general.col("Descr"), general.col("Val")) else {
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
            // The normalized matrix/vector tables repeat one FID across every
            // (row, col) entry of a matrix, so the per-table id-uniqueness rule
            // does not apply to them.
            let shared_id = matches!(table.class.as_str(), "Matrix" | "VecDouble");
            if !shared_id && !id.is_empty() && !ids.insert(id.to_string()) {
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
pub(crate) fn parse_num(s: &str, decimal: char) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if decimal == ',' {
        let swapped: String = t.chars().map(|c| if c == ',' { '.' } else { c }).collect();
        swapped.parse::<f64>().ok()
    } else {
        t.parse::<f64>().ok()
    }
}

fn bad_field(field: &str, tok: &str, line_no: usize) -> Error {
    Error::FormatRead {
        format: FMT,
        message: format!("{field} value {tok:?} is not a number at line {line_no}"),
    }
}

/// The raw text of a cell, or `None` for a missing column or empty cell.
pub(crate) fn cell<'a>(t: &'a DgsTable, r: &'a DgsRow, name: &str) -> Option<&'a str> {
    let i = t.col(name)?;
    r.cells.get(i)?.as_deref()
}

/// A string attribute that is not a `##` foreign key.
pub(crate) fn text<'a>(t: &'a DgsTable, r: &'a DgsRow, name: &str) -> Option<&'a str> {
    cell(t, r, name).filter(|s| !s.starts_with("##"))
}

/// A raw pointer attribute (may be a `##` external reference).
pub(crate) fn ptr<'a>(t: &'a DgsTable, r: &'a DgsRow, name: &str) -> Option<&'a str> {
    cell(t, r, name).map(str::trim).filter(|s| !s.is_empty())
}

pub(crate) fn num(
    t: &DgsTable,
    r: &DgsRow,
    name: &str,
    default: f64,
    decimal: char,
) -> Result<f64> {
    match cell(t, r, name) {
        None => Ok(default),
        Some(s) if s.starts_with("##") => Ok(default),
        Some(s) => parse_num(s, decimal).ok_or_else(|| bad_field(name, s, r.line_no)),
    }
}

pub(crate) fn int(
    t: &DgsTable,
    r: &DgsRow,
    name: &str,
    default: i64,
    decimal: char,
) -> Result<i64> {
    Ok(num(t, r, name, default as f64, decimal)?.round() as i64)
}
