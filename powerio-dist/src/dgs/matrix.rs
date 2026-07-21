//! DGS matrix/vector attribute plumbing.
//!
//! DGS carries a matrix-valued attribute two ways, and this module reads both:
//!
//! 1. Normalized (DGS 7.0): a `$$Matrix` table (`FID;MatRow;MatCol;Val`) and a
//!    `$$VecDouble` table (`FID;VecIndex;Val`). A matrix cell on the element or
//!    type row holds an FID that indexes these rows.
//! 2. Denormalized: `<base>:SIZEROW`/`<base>:SIZECOL` plus indexed cells
//!    (`<base>:r:c`, or the flattened `<base>:k`) on the row itself. The scanner
//!    keeps `:`-suffixed column names verbatim, so the group is reassembled by
//!    base name.
//!
//! [`matrix_attr`] tries the denormalized group first, then the normalized FID
//! reference. A triangular input is symmetric-completed; a full asymmetric
//! matrix is returned verbatim.

use std::collections::HashMap;

use super::scan::{DgsDoc, DgsRow, DgsTable, cell, parse_num};
use crate::model::Mat;

/// The normalized `$$Matrix`/`$$VecDouble` tables, keyed by FID.
pub(crate) struct MatrixRegistry {
    matrices: HashMap<String, Vec<(usize, usize, f64)>>,
    // The vector half of the normalized plumbing is parsed per the DGS 7.0
    // spec; it feeds the geometry/cable tiers (per-conductor GMR, coordinates)
    // once those are mapped, so it is retained ahead of its first consumer.
    #[allow(dead_code)]
    vectors: HashMap<String, Vec<(usize, f64)>>,
}

impl MatrixRegistry {
    pub(crate) fn build(doc: &DgsDoc) -> Self {
        let d = doc.decimal;
        let mut matrices: HashMap<String, Vec<(usize, usize, f64)>> = HashMap::new();
        if let Some(t) = doc.table("Matrix") {
            for r in &t.rows {
                let fid = t.id_of(r);
                // The DGS 7.0 spec names the column `MatColumn`; `MatCol`
                // also appears in the wild, so accept both.
                let (Some(row), Some(col), Some(val)) = (
                    idx(t, r, "MatRow"),
                    idx(t, r, "MatCol").or_else(|| idx(t, r, "MatColumn")),
                    cell(t, r, "Val").and_then(|s| parse_num(s, d)),
                ) else {
                    continue;
                };
                matrices.entry(fid).or_default().push((row, col, val));
            }
        }
        let mut vectors: HashMap<String, Vec<(usize, f64)>> = HashMap::new();
        if let Some(t) = doc.table("VecDouble") {
            for r in &t.rows {
                let fid = t.id_of(r);
                let (Some(i), Some(val)) = (
                    idx(t, r, "VecIndex"),
                    cell(t, r, "Val").and_then(|s| parse_num(s, d)),
                ) else {
                    continue;
                };
                vectors.entry(fid).or_default().push((i, val));
            }
        }
        MatrixRegistry { matrices, vectors }
    }

    /// The normalized matrix an FID names, symmetric-completed if triangular.
    fn normalized(&self, fid: &str) -> Option<Mat> {
        let entries = self.matrices.get(fid.trim())?;
        let n = entries
            .iter()
            .map(|&(r, c, _)| r.max(c) + 1)
            .max()
            .unwrap_or(0);
        if n == 0 {
            return None;
        }
        let mut m = vec![vec![0.0; n]; n];
        for &(r, c, v) in entries {
            if r < n && c < n {
                m[r][c] = v;
            }
        }
        Some(complete_symmetry(m))
    }

    /// The normalized vector an FID names. Retained for the geometry/cable
    /// tiers (see the `vectors` field note).
    #[allow(dead_code)]
    pub(crate) fn vector(&self, fid: &str) -> Option<Vec<f64>> {
        let entries = self.vectors.get(fid.trim())?;
        let n = entries.iter().map(|&(i, _)| i + 1).max().unwrap_or(0);
        let mut v = vec![0.0; n];
        for &(i, val) in entries {
            if i < n {
                v[i] = val;
            }
        }
        Some(v)
    }
}

fn idx(t: &DgsTable, r: &DgsRow, name: &str) -> Option<usize> {
    let s = cell(t, r, name)?.trim();
    s.parse::<i64>().ok().and_then(|i| usize::try_from(i).ok())
}

/// Read a matrix-valued attribute `base` off `r`: the denormalized column group
/// first, then the normalized FID reference in the `base` cell.
pub(crate) fn matrix_attr(
    reg: &MatrixRegistry,
    t: &DgsTable,
    r: &DgsRow,
    base: &str,
    decimal: char,
) -> Option<Mat> {
    if let Some(m) = denormalized(t, r, base, decimal) {
        return Some(m);
    }
    let fid = cell(t, r, base)?;
    reg.normalized(fid)
}

/// Reassemble a `<base>:…` denormalized column group into a matrix.
fn denormalized(t: &DgsTable, r: &DgsRow, base: &str, decimal: char) -> Option<Mat> {
    let prefix = format!("{base}:");
    let mut size_row: Option<usize> = None;
    let mut size_col: Option<usize> = None;
    let mut entries: Vec<(Vec<usize>, f64)> = Vec::new();
    for (i, col) in t.columns.iter().enumerate() {
        let Some(suffix) = col.strip_prefix(&prefix) else {
            continue;
        };
        let Some(raw) = r.cells.get(i).and_then(|c| c.as_deref()) else {
            continue;
        };
        match suffix {
            "SIZEROW" => size_row = raw.trim().parse().ok(),
            "SIZECOL" => size_col = raw.trim().parse().ok(),
            other => {
                let coords: Vec<usize> = other
                    .split(':')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                if let Some(v) = parse_num(raw, decimal) {
                    entries.push((coords, v));
                }
            }
        }
    }
    let nrow = size_row?;
    if nrow == 0 {
        return None;
    }
    let ncol = size_col.unwrap_or(nrow);
    let mut m = vec![vec![0.0; ncol]; nrow];
    for (coords, v) in entries {
        let (row, col) = match coords.as_slice() {
            [r, c] => (*r, *c),
            [k] => (*k / ncol, *k % ncol),
            _ => continue,
        };
        if row < nrow && col < ncol {
            m[row][col] = v;
        }
    }
    Some(complete_symmetry(m))
}

/// Mirror a strictly lower- or upper-triangular matrix into a full symmetric
/// one; a matrix with entries on both sides is returned verbatim (its
/// asymmetry is intended, e.g. an untransposed cable). The mirror swaps
/// `m[i][j]`/`m[j][i]`, so index loops are the natural form.
#[allow(clippy::needless_range_loop)]
fn complete_symmetry(mut m: Mat) -> Mat {
    let n = m.len();
    if n == 0 || m.iter().any(|row| row.len() != n) {
        return m;
    }
    let lower = (0..n).all(|i| (i + 1..n).all(|j| m[i][j] == 0.0));
    let upper = (0..n).all(|i| (0..i).all(|j| m[i][j] == 0.0));
    if lower && !upper {
        for i in 0..n {
            for j in i + 1..n {
                m[i][j] = m[j][i];
            }
        }
    } else if upper && !lower {
        for i in 0..n {
            for j in 0..i {
                m[i][j] = m[j][i];
            }
        }
    }
    m
}
