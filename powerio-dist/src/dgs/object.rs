//! Layer 2: the object index and cubicle/coupler topology.
//!
//! Topology is not on the element rows. A `StaCubic` cubicle names the
//! terminal it sits in (`fold_id`), the element it connects (`obj_id`), and
//! which terminal of that element (`obj_bus`); an open `StaSwitch` in a
//! cubicle breaks the connection. `ElmTerm` rows become buses in file order;
//! closed `ElmCoup` couplers fuse the two terminals they join, open ones
//! become switches.
//!
//! Beyond the balanced reader's topology this layer also records the
//! connected phase of a cubicle (`cPhInfo` = `L1`/`L2`/`L3`), which the
//! phase-domain mapping needs to place a single-phase element on the right
//! conductor.

use std::collections::HashMap;

use super::scan::{DgsDoc, DgsRow, DgsTable, int, ptr, text};

/// One terminal connection of an element, from a cubicle.
pub(crate) struct Conn {
    /// The `obj_bus` side index (0 = from/HV, 1 = to/LV, 2 = tertiary).
    pub(crate) side: i64,
    /// Position of the connected terminal in `ElmTerm` file order.
    pub(crate) term_pos: usize,
    /// False when a `StaSwitch` in the cubicle is open.
    pub(crate) closed: bool,
    /// The pinned phase conductor (`1`/`2`/`3`) from the cubicle's `cPhInfo`,
    /// when the export states it; `None` leaves the mapping to default.
    pub(crate) phase: Option<u8>,
}

/// Disjoint-set union over terminal positions; the smallest index in a set is
/// its root, so a fused bus keeps the first terminal's id.
pub(crate) struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    pub(crate) fn find(&mut self, x: usize) -> usize {
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

    pub(crate) fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            let (lo, hi) = (ra.min(rb), ra.max(rb));
            self.parent[hi] = lo;
        }
    }
}

/// The resolved topology derived from the cubicle and switch tables.
pub(crate) struct Topology<'a> {
    /// `ElmTerm` rows in file order.
    pub(crate) terminals: Vec<&'a DgsRow>,
    /// Element FID to its terminal connections.
    pub(crate) conns: HashMap<String, Vec<Conn>>,
    /// Union-find over terminal positions (couplers fuse closed ones).
    pub(crate) uf: UnionFind,
    /// Terminal position to its surviving bus index (its set root, 0-based).
    pub(crate) bus_of_pos: Vec<usize>,
}

/// Map a `cPhInfo` cell such as `L1`, `L2`, `L3` (or a bare `1`/`2`/`3`) to a
/// conductor index. Any other spelling leaves the phase unpinned.
fn parse_phase(info: &str) -> Option<u8> {
    let t = info.trim();
    let digits: String = t.chars().filter(char::is_ascii_digit).collect();
    match digits.as_str() {
        "1" => Some(1),
        "2" => Some(2),
        "3" => Some(3),
        _ => None,
    }
}

/// Build the cubicle/switch topology for a document. Union-find starts with
/// every terminal in its own set; couplers fuse closed pairs in layer 3.
pub(crate) fn topology(doc: &DgsDoc) -> Topology<'_> {
    let terminals: Vec<&DgsRow> = doc.rows("ElmTerm").iter().collect();
    let term_table = doc.table("ElmTerm");
    let mut term_pos = HashMap::with_capacity(terminals.len());
    if let Some(tt) = term_table {
        for (pos, r) in terminals.iter().enumerate() {
            term_pos.insert(tt.id_of(r), pos);
        }
    }

    // Cubicle to closed-state, from StaSwitch (fold_id -> cubicle FID).
    let mut cubicle_closed: HashMap<String, bool> = HashMap::new();
    if let Some(sw) = doc.table("StaSwitch") {
        for r in &sw.rows {
            if let Some(cub) = ptr(sw, r, "fold_id") {
                let closed = int(sw, r, "on_off", 1, doc.decimal).unwrap_or(1) != 0;
                let entry = cubicle_closed.entry(cub.to_string()).or_insert(true);
                *entry = *entry && closed;
            }
        }
    }

    // Element FID -> connections, from StaCubic.
    let mut conns: HashMap<String, Vec<Conn>> = HashMap::new();
    if let Some(cub) = doc.table("StaCubic") {
        for r in &cub.rows {
            let side = int(cub, r, "obj_bus", -1, doc.decimal).unwrap_or(-1);
            let Some(obj) = ptr(cub, r, "obj_id") else {
                continue;
            };
            if side < 0 || obj.starts_with("##") {
                continue; // spare cubicle or external element
            }
            let Some(term) = ptr(cub, r, "fold_id").and_then(|f| term_pos.get(f).copied()) else {
                continue;
            };
            let closed = cubicle_closed.get(&cub.id_of(r)).copied().unwrap_or(true);
            let phase = text(cub, r, "cPhInfo").and_then(parse_phase);
            conns.entry(obj.to_string()).or_default().push(Conn {
                side,
                term_pos: term,
                closed,
                phase,
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

/// Build the FID → (table index, row index) object index across the document.
pub(crate) fn index_objects(doc: &DgsDoc) -> HashMap<&str, (usize, usize)> {
    let mut by_id: HashMap<&str, (usize, usize)> = HashMap::new();
    for (ti, t) in doc.tables.iter().enumerate() {
        for (ri, r) in t.rows.iter().enumerate() {
            let id = t.id_cell(r);
            if !id.is_empty() {
                by_id.insert(id, (ti, ri));
            }
        }
    }
    by_id
}

/// Resolve a pointer to the table and row it names.
pub(crate) fn resolve<'a>(
    doc: &'a DgsDoc,
    by_id: &HashMap<&'a str, (usize, usize)>,
    key: &str,
) -> Option<(&'a DgsTable, &'a DgsRow)> {
    let (ti, ri) = by_id.get(key)?;
    let t = &doc.tables[*ti];
    Some((t, &t.rows[*ri]))
}
