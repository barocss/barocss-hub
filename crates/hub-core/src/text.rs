//! Text content CRDT (Repository-Semantics §7, Wiki Text-CRDT, Issue #53).
//!
//! One sequence CRDT, FugueMax (7.3.2), over the UTF-8 characters of a
//! file's content, with element ids `(OpId, byte offset)` so one op carries
//! whole runs (7.3.1). Each `SetContent` (or the `Create`'s empty content)
//! starts an *epoch* whose text is seeded virtually: the blob's bytes are one
//! run created by the epoch op itself (7.4.2). [`crate::tree::Payload::TextEdit`]
//! ops edit one epoch; all edits of the winning epoch merge (7.4.3).
//!
//! Pure core: no disk, wire or CLI. Blob bytes come through [`BlobSource`].
//!
//! * [`TextDoc`]: the FugueMax tree of one epoch, stored as run fragments
//!   in a chunked list, integrated incrementally in any causal order.
//! * [`TextEval`]: the text part of [`crate::tree::Evaluation`]: per-node
//!   content, the 7.4.3 content conflicts, `concurrent-edit` notices (7.8).
//! * [`diff_line`]: the pure `hub-diff-line/1` helper (7.5.2) and
//!   [`author_text_edit`], which turns a save into a `TextEdit` against the
//!   author's base (7.5.1: `seen`, anchor mapping at `deps`).
//!
//! Receivers determine an insert's side from the tree, not from the
//! author's list: the run is a left child of `right` iff `right` is reached
//! from `left` by one right edge followed only by left edges (i.e. `right`
//! is the leftmost element of `left`'s right subtree, which is exactly the
//! case "`left` had right children" of 7.3.2 when `right` is `left`'s
//! successor in the author's full list at `deps`); otherwise it is a right
//! child of `left`. For conforming authors this is the 7.3.2 rule; for any
//! op it is a pure function of the op set.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::tree::{
    BlobId, BlobStore, Causality, Conflict, Kind, NodeId, Op, OpId, Outcome, Payload,
};

/// Element id (7.3.1): the op that inserted the run and the byte offset of
/// the character's first byte within that op's inserted bytes (for an
/// epoch op: within its blob). Ordered by `op` in `<`, then `off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElemId {
    pub op: OpId,
    pub off: u32,
}

/// One inserted run (7.4.1): `left` = `None` is `START`, `right` = `None` is
/// `END`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ins {
    pub left: Option<ElemId>,
    pub right: Option<ElemId>,
    pub bytes: Vec<u8>,
}

/// The `TextEdit` payload (7.4.1). `del` = id ranges `(first element,
/// byte length)`; the runs of `ins` are concatenated for addressing: run
/// `k`'s bytes start at offset `sum(len(ins[0..k]))` within the op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub node: NodeId,
    pub epoch: OpId,
    pub seen: BTreeSet<OpId>,
    pub del: Vec<(ElemId, u32)>,
    pub ins: Vec<Ins>,
}

impl TextEdit {
    /// Lines the edit inserts: `\n`s in the inserted runs, plus one for a
    /// run that ends without one (Issue #56 `log`/`show` summary).
    pub fn lines_added(&self) -> usize {
        self.ins.iter().map(|i| split_lines(&i.bytes).len()).sum()
    }

    /// Total inserted bytes (the op's element offsets are `0..ins_len`).
    pub fn ins_len(&self) -> u64 {
        self.ins.iter().map(|i| i.bytes.len() as u64).sum()
    }
}

/// `del` and `ins` of a diffed save.
pub type DiffEdit = (Vec<(ElemId, u32)>, Vec<Ins>);

/// Split one save's `TextEdit` into a chain of ops whose records each
/// satisfy `size(op) <= limit` (Issue #54, Text-CRDT 7.4.1 "chained
/// saves"). `first` is the id of op 1 (later ops take `counter + 1, +2,
/// ...` of the same replica), `deps` its deps. Op `k+1` has `deps =
/// {k}` (so `deps*(k+1) = deps*(k) ∪ {k}`) and `seen = {k}` (op 1 keeps
/// `edit.seen`).
///
/// Split points: whole `del` entries and `ins` runs first (in payload
/// order); a run too long for one op is cut at a UTF-8 character
/// boundary, preferring the end of a line; its continuation's `left` is
/// the last element of the previous piece (its successor at the new deps
/// is the run's original `right`, so anchor adjacency, 7.12 #17, holds
/// within the chain). The edit's anchors must be elements of its base
/// (as `diff_line` produces). `size` is the caller's encoded record
/// length; returns `None` if a single smallest piece cannot fit.
pub fn split_text_edit(
    first: OpId,
    deps: BTreeSet<OpId>,
    edit: TextEdit,
    size: &dyn Fn(&Op) -> usize,
    limit: usize,
) -> Option<Vec<Op>> {
    let mk = |id: OpId,
              deps: &BTreeSet<OpId>,
              seen: &BTreeSet<OpId>,
              del: Vec<(ElemId, u32)>,
              ins: Vec<Ins>| Op {
        id,
        deps: deps.clone(),
        payload: Payload::TextEdit(TextEdit {
            node: edit.node,
            epoch: edit.epoch,
            seen: seen.clone(),
            del,
            ins,
        }),
    };
    let whole = mk(first, &deps, &edit.seen, edit.del.clone(), edit.ins.clone());
    if size(&whole) <= limit {
        return Some(vec![whole]);
    }
    let mut out: Vec<Op> = Vec::new();
    let mut id = first;
    let mut deps = deps;
    let mut seen = edit.seen.clone();
    let mut del: Vec<(ElemId, u32)> = Vec::new();
    let mut ins: Vec<Ins> = Vec::new();
    let fits =
        |id: OpId,
         deps: &BTreeSet<OpId>,
         seen: &BTreeSet<OpId>,
         del: &[(ElemId, u32)],
         ins: &[Ins]| { size(&mk(id, deps, seen, del.to_vec(), ins.to_vec())) <= limit };
    // Close the current op and start the next one of the chain.
    let flush = |out: &mut Vec<Op>,
                 id: &mut OpId,
                 deps: &mut BTreeSet<OpId>,
                 seen: &mut BTreeSet<OpId>,
                 del: &mut Vec<(ElemId, u32)>,
                 ins: &mut Vec<Ins>| {
        out.push(mk(
            *id,
            deps,
            seen,
            std::mem::take(del),
            std::mem::take(ins),
        ));
        // deps* = deps*(k) ∪ {k}: `{k}` alone names the same causal past
        // (k's own deps are in it) and keeps each record small.
        *deps = [*id].into();
        *seen = [*id].into();
        *id = NodeId::new(id.counter + 1, id.replica.0);
    };
    for d in edit.del.iter().copied() {
        let mut t = del.clone();
        t.push(d);
        if !fits(id, &deps, &seen, &t, &ins) {
            if del.is_empty() && ins.is_empty() {
                return None;
            }
            flush(&mut out, &mut id, &mut deps, &mut seen, &mut del, &mut ins);
            if !fits(id, &deps, &seen, &[d], &[]) {
                return None;
            }
        }
        del.push(d);
    }
    for run in edit.ins.iter().cloned() {
        let mut run = run;
        loop {
            let mut t = ins.clone();
            t.push(run.clone());
            if fits(id, &deps, &seen, &del, &t) {
                ins.push(run);
                break;
            }
            // Longest prefix that fits (at a char boundary), then prefer
            // the last line end inside it.
            let bounds: Vec<usize> = (1..=run.bytes.len())
                .filter(|&i| i == run.bytes.len() || !is_cont(run.bytes[i]))
                .collect();
            let ok = |m: usize| {
                let mut t = ins.clone();
                t.push(Ins {
                    left: run.left,
                    right: run.right,
                    bytes: run.bytes[..m].to_vec(),
                });
                fits(id, &deps, &seen, &del, &t)
            };
            let (mut lo, mut hi) = (0usize, bounds.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if ok(bounds[mid - 1]) {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if lo == 0 {
                if del.is_empty() && ins.is_empty() {
                    return None;
                }
                flush(&mut out, &mut id, &mut deps, &mut seen, &mut del, &mut ins);
                continue;
            }
            let mut m = bounds[lo - 1];
            if let Some(nl) = run.bytes[..m].iter().rposition(|&b| b == b'\n') {
                if nl + 1 >= m / 2 {
                    m = nl + 1;
                }
            }
            let start: u32 = ins.iter().map(|i| i.bytes.len() as u32).sum();
            let mut last = m - 1;
            while is_cont(run.bytes[last]) {
                last -= 1;
            }
            ins.push(Ins {
                left: run.left,
                right: run.right,
                bytes: run.bytes[..m].to_vec(),
            });
            let left = Some(ElemId {
                op: id,
                off: start + last as u32,
            });
            flush(&mut out, &mut id, &mut deps, &mut seen, &mut del, &mut ins);
            run = Ins {
                left,
                right: run.right,
                bytes: run.bytes[m..].to_vec(),
            };
        }
    }
    if !del.is_empty() || !ins.is_empty() {
        out.push(mk(id, &deps, &seen, del, ins));
    }
    Some(out)
}

/// Name of the line diff (7.5.2), reported by a server that diffs.
pub const DIFF_NAME: &str = "hub-diff-line/1";
/// 7.5.2 cost bound: above this many edited lines, delete all, insert all.
pub const DIFF_MAX_D: usize = 20_000;

/// Read access to blob bytes (the CLI supplies its store; tests a map).
/// The empty blob is always available. Issue #54: a source may read on
/// demand (the CLI's `.hub/blobs`), so bytes come back as a `Cow`.
pub trait BlobSource {
    fn blob_bytes(&self, id: &BlobId) -> Option<Cow<'_, [u8]>>;
}

impl BlobSource for BlobStore {
    fn blob_bytes(&self, id: &BlobId) -> Option<Cow<'_, [u8]>> {
        self.get(id).map(Cow::Borrowed)
    }
}

impl BlobSource for BTreeMap<BlobId, Vec<u8>> {
    fn blob_bytes(&self, id: &BlobId) -> Option<Cow<'_, [u8]>> {
        self.get(id).map(|v| Cow::Borrowed(v.as_slice()))
    }
}

impl BlobSource for HashMap<BlobId, Vec<u8>> {
    fn blob_bytes(&self, id: &BlobId) -> Option<Cow<'_, [u8]>> {
        self.get(id).map(|v| Cow::Borrowed(v.as_slice()))
    }
}

/// No blob bytes at all (only the empty blob resolves).
pub struct NoBlobs;

impl BlobSource for NoBlobs {
    fn blob_bytes(&self, _: &BlobId) -> Option<Cow<'_, [u8]>> {
        None
    }
}

pub(crate) fn fetch<'a>(src: &'a dyn BlobSource, id: &BlobId) -> Option<Cow<'a, [u8]>> {
    if *id == BlobId::empty() {
        return Some(Cow::Borrowed(&[]));
    }
    src.blob_bytes(id)
}

/// Informational notice (7.8): derived, never blocks, not a conflict.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Notice {
    /// Overlapping concurrent `TextEdit`s of one node's winning epoch,
    /// merged; `lines` = first and last (1-based) line of head text they
    /// touch.
    ConcurrentEdit {
        node: NodeId,
        ops: Vec<OpId>,
        lines: (u32, u32),
    },
}

/// Content of a node that has at least one valid `TextEdit` (7.4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeText {
    /// The winning epoch (epoch of the greatest content op).
    pub epoch: OpId,
    /// Head content id (also set as the node's `content` in the state).
    pub blob: BlobId,
    /// Head bytes; `None` only when the epoch blob is unavailable
    /// (7.4.2 deferred check, thin clients).
    pub bytes: Option<Vec<u8>>,
}

/// One epoch's text state, or a deferred one (blob not available).
#[derive(Clone, Debug)]
pub enum DocSlot {
    Doc(Box<TextDoc>),
    Deferred,
    /// Issue #54 (snapshot v2): a doc decoded from a snapshot that
    /// references its seed by blob id; the seed bytes are read on first
    /// use ([`load_slot`]). Never produced by `evaluate`.
    Lazy(Box<TextDoc>, BlobId),
}

/// Load a [`DocSlot::Lazy`] doc's seed from `blobs` (checked against the
/// blob id and the recorded length). `Err(blob)` if it is missing or
/// does not match; other slots are left as they are.
pub(crate) fn load_slot(slot: &mut DocSlot, blobs: &dyn BlobSource) -> Result<(), BlobId> {
    let DocSlot::Lazy(d, blob) = slot else {
        return Ok(());
    };
    let blob = *blob;
    let Some(b) = fetch(blobs, &blob) else {
        return Err(blob);
    };
    if b.len() != d.op_bytes[0].1 as usize || BlobId::of(&b) != blob {
        return Err(blob);
    }
    let mut arena = Vec::with_capacity(b.len() + d.arena.len());
    arena.extend_from_slice(&b);
    arena.extend_from_slice(&d.arena);
    let mut doc = std::mem::replace(d, Box::new(TextDoc::new(d.epoch, b"")));
    doc.arena = arena;
    *slot = DocSlot::Doc(doc);
    Ok(())
}

/// Text part of an evaluation. Empty when the op set has no `TextEdit`.
#[derive(Clone, Debug, Default)]
pub struct TextEval {
    /// Per `(node, epoch)` with at least one structurally applicable edit.
    pub docs: BTreeMap<(NodeId, OpId), DocSlot>,
    /// Valid (Applied) `TextEdit`s.
    pub valid: BTreeSet<OpId>,
    pub content: BTreeMap<NodeId, NodeText>,
    pub notices: BTreeSet<Notice>,
    /// Bytes of derived blobs (head text, text-epoch conflict blobs); the
    /// CLI stores them (T2/T3).
    pub blobs: BTreeMap<BlobId, Vec<u8>>,
}

impl TextEval {
    pub fn doc(&self, node: NodeId, epoch: OpId) -> Option<&TextDoc> {
        match self.docs.get(&(node, epoch)) {
            Some(DocSlot::Doc(d)) => Some(d),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- TextDoc

/// A fast non-cryptographic hasher for the doc's internal indices (Issue
/// #54: snapshot open time; keys are local ids, not attacker-chosen in a
/// way that matters for a local cache).
#[derive(Clone, Copy, Default)]
pub(crate) struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn write(&mut self, b: &[u8]) {
        for c in b.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(w));
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_u32(&mut self, v: u32) {
        self.write_u64(v as u64)
    }
    fn write_u128(&mut self, v: u128) {
        self.write_u64(v as u64);
        self.write_u64((v >> 64) as u64);
    }
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64)
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FxMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<Fx>>;

pub(crate) const NIL: u32 = u32::MAX;
const START_F: u32 = 0;
const CHUNK: usize = 128;
/// Tag of an inline one-deleter set in `Frag::del`.
pub(crate) const ONE: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    L,
    R,
}

/// A fragment of a run: elements `(op, off..off+len)`, a chain. Left
/// children hang on its first element, right children on its last; inner
/// elements have exactly the chain child (split on demand). Compact (7.11
/// memory): ops are interned indices, children are sorted sibling lists
/// (`first_l` / `first_r` / `next`), deleter sets are interned.
#[derive(Clone, Debug)]
pub(crate) struct Frag {
    pub(crate) op: u32,
    pub(crate) off: u32,
    pub(crate) len: u32,
    pub(crate) parent: u32,
    pub(crate) chunk: u32,
    pub(crate) first_l: u32,
    pub(crate) first_r: u32,
    pub(crate) next: u32,
    /// The run's right origin (FugueMax right-sibling key): op index and
    /// offset, `ro_op == NIL` for END.
    pub(crate) ro_op: u32,
    pub(crate) ro_off: u32,
    /// Interned set of deleting ops (0 = none).
    pub(crate) del: u32,
    pub(crate) side: Side,
}

#[derive(Clone, Copy)]
enum Place {
    Before(u32),
    After(u32),
}

/// FugueMax tree of one epoch (7.3), runs as fragments in a chunked list
/// (an order-statistic structure over runs, 7.11). Holds every integrated
/// op; versions are filters ([`TextDoc::materialize`], 7.3.4).
#[derive(Clone, Debug)]
pub struct TextDoc {
    pub(crate) epoch: OpId,
    pub(crate) ops: Vec<OpId>,
    pub(crate) op_ix: FxMap<OpId, u32>,
    /// Per op index: start in `arena` and byte length.
    pub(crate) op_bytes: Vec<(u64, u32)>,
    pub(crate) arena: Vec<u8>,
    /// Per op index: its fragment at offset 0, or `NIL`.
    pub(crate) op_first: Vec<u32>,
    /// Ops split into several fragments: `(start offset, fragment)`.
    pub(crate) split: FxMap<u32, Vec<(u32, u32)>>,
    pub(crate) frags: Vec<Frag>,
    /// Interned sorted deleter sets (op indices) of two or more ops;
    /// `delsets[0]` is empty. A one-op set is stored inline in
    /// `Frag::del` as `ONE | op index`.
    pub(crate) delsets: Vec<Vec<u32>>,
    pub(crate) delset_ix: FxMap<Vec<u32>, u32>,
    pub(crate) chunks: Vec<Vec<u32>>,
    pub(crate) order: Vec<u32>,
    pub(crate) rank: Vec<u32>,
}

fn char_len(b: u8) -> u32 {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

fn is_cont(b: u8) -> bool {
    b & 0xC0 == 0x80
}

impl TextDoc {
    /// The epoch's seed (7.4.2): `seed` as one run created by `epoch`.
    pub fn new(epoch: OpId, seed: &[u8]) -> Self {
        let start = Frag {
            op: 0,
            off: 0,
            len: 0,
            parent: NIL,
            chunk: NIL,
            first_l: NIL,
            first_r: NIL,
            next: NIL,
            ro_op: NIL,
            ro_off: 0,
            del: 0,
            side: Side::R,
        };
        let mut d = TextDoc {
            epoch,
            ops: Vec::new(),
            op_ix: FxMap::default(),
            op_bytes: Vec::new(),
            arena: Vec::new(),
            op_first: Vec::new(),
            split: FxMap::default(),
            frags: vec![start],
            delsets: vec![Vec::new()],
            delset_ix: FxMap::default(),
            chunks: vec![Vec::new()],
            order: vec![0],
            rank: vec![0],
        };
        d.delset_ix.insert(Vec::new(), 0);
        let ix = d.add_op(epoch, seed);
        if !seed.is_empty() {
            d.insert_run(ix, 0, seed.len() as u32, None, None);
        }
        d
    }

    fn add_op(&mut self, id: OpId, bytes: &[u8]) -> u32 {
        let ix = self.ops.len() as u32;
        self.ops.push(id);
        self.op_ix.insert(id, ix);
        self.op_bytes
            .push((self.arena.len() as u64, bytes.len() as u32));
        self.arena.extend_from_slice(bytes);
        self.op_first.push(NIL);
        ix
    }

    pub fn epoch(&self) -> OpId {
        self.epoch
    }

    /// Length of the epoch's seed.
    pub fn seed_len(&self) -> u32 {
        self.op_bytes[0].1
    }

    /// The epoch's seed bytes (its blob, 7.4.2).
    pub fn seed(&self) -> &[u8] {
        self.op_slice(0)
    }

    /// Whether `op` has been integrated (or is the epoch).
    pub fn has_op(&self, op: OpId) -> bool {
        self.op_ix.contains_key(&op)
    }

    /// Number of fragments (runs after splits).
    pub fn runs(&self) -> usize {
        self.frags.len() - 1
    }

    fn op_slice(&self, ix: u32) -> &[u8] {
        let (s, n) = self.op_bytes[ix as usize];
        &self.arena[s as usize..s as usize + n as usize]
    }

    fn bytes_of(&self, op: OpId) -> Option<&[u8]> {
        self.op_ix.get(&op).map(|&ix| self.op_slice(ix))
    }

    /// Bytes of a `del` range `(first element, byte length)` (Issue #56:
    /// `log`/`show` summaries); `None` if not an integrated range.
    pub fn range_bytes(&self, first: ElemId, len: u32) -> Option<&[u8]> {
        self.bytes_of(first.op)?
            .get(first.off as usize..first.off as usize + len as usize)
    }

    /// `e` is an element of this epoch: a character start of an
    /// integrated op's bytes.
    pub fn is_elem(&self, e: ElemId) -> bool {
        self.bytes_of(e.op)
            .and_then(|b| b.get(e.off as usize))
            .is_some_and(|&b| !is_cont(b))
    }

    fn range_ok(&self, e: ElemId, len: u32) -> bool {
        let Some(b) = self.bytes_of(e.op) else {
            return false;
        };
        let end = e.off as u64 + len as u64;
        len > 0
            && self.is_elem(e)
            && end <= b.len() as u64
            && (end == b.len() as u64 || !is_cont(b[end as usize]))
    }

    /// 7.4.1/7.4.2 validity of `edit` against this epoch: UTF-8 runs,
    /// referenced elements exist and their ops satisfy `in_deps`, delete
    /// ranges on character boundaries. (Epoch and `seen` are checked by
    /// the caller.)
    pub fn check(&self, edit: &TextEdit, in_deps: &dyn Fn(OpId) -> bool) -> bool {
        if edit.ins_len() > u32::MAX as u64 {
            return false;
        }
        let elem_ok = |e: &Option<ElemId>| match e {
            None => true,
            Some(e) => self.is_elem(*e) && in_deps(e.op),
        };
        edit.ins.iter().all(|i| {
            !i.bytes.is_empty()
                && std::str::from_utf8(&i.bytes).is_ok()
                && elem_ok(&i.left)
                && elem_ok(&i.right)
        }) && edit
            .del
            .iter()
            .all(|&(e, len)| in_deps(e.op) && self.range_ok(e, len))
    }

    /// Integrate a validated op (any causal order).
    pub fn apply(&mut self, id: OpId, edit: &TextEdit) {
        let mut all = Vec::with_capacity(edit.ins_len() as usize);
        for i in &edit.ins {
            all.extend_from_slice(&i.bytes);
        }
        let ix = self.add_op(id, &all);
        let mut off = 0u32;
        for i in &edit.ins {
            let n = i.bytes.len() as u32;
            self.insert_run(ix, off, n, i.left, i.right);
            off += n;
        }
        for &(e, len) in &edit.del {
            self.delete(ix, e, len);
        }
    }

    fn find(&self, e: ElemId) -> Option<u32> {
        let ix = *self.op_ix.get(&e.op)?;
        let f = match self.split.get(&ix) {
            None => self.op_first[ix as usize],
            Some(v) => {
                let i = v.partition_point(|&(o, _)| o <= e.off);
                v.get(i.checked_sub(1)?)?.1
            }
        };
        if f == NIL {
            return None;
        }
        let fr = &self.frags[f as usize];
        (e.off >= fr.off && e.off < fr.off + fr.len).then_some(f)
    }

    fn index_add(&mut self, ix: u32, off: u32, f: u32) {
        if off == 0 {
            self.op_first[ix as usize] = f;
            return;
        }
        let first = self.op_first[ix as usize];
        let v = self.split.entry(ix).or_insert_with(|| vec![(0, first)]);
        let i = v.partition_point(|&(o, _)| o < off);
        v.insert(i, (off, f));
    }

    fn locate(&self, f: u32) -> (usize, usize) {
        let c = self.frags[f as usize].chunk as usize;
        let i = self.chunks[c].iter().position(|&x| x == f).unwrap();
        (c, i)
    }

    fn place(&mut self, p: Place, nf: u32) {
        let (c, i) = match p {
            Place::After(START_F) => (self.order[0] as usize, 0),
            Place::After(g) => {
                let (c, i) = self.locate(g);
                (c, i + 1)
            }
            Place::Before(g) => self.locate(g),
        };
        self.chunks[c].insert(i, nf);
        self.frags[nf as usize].chunk = c as u32;
        if self.chunks[c].len() > 2 * CHUNK {
            let nc = self.chunks.len() as u32;
            let tail = self.chunks[c].split_off(CHUNK);
            for &f in &tail {
                self.frags[f as usize].chunk = nc;
            }
            self.chunks.push(tail);
            let r = self.rank[c] as usize;
            self.order.insert(r + 1, nc);
            self.rank.push(0);
            for (k, &ch) in self.order.iter().enumerate().skip(r + 1) {
                self.rank[ch as usize] = k as u32;
            }
        }
    }

    /// List position of the right origin `(op index, off)` (`NIL` = END).
    fn epos(&self, ro_op: u32, ro_off: u32) -> (u32, u32, u32) {
        if ro_op == NIL {
            return (u32::MAX, u32::MAX, u32::MAX);
        }
        let e = ElemId {
            op: self.ops[ro_op as usize],
            off: ro_off,
        };
        let f = self.find(e).expect("origin exists");
        let (c, i) = self.locate(f);
        (self.rank[c], i as u32, ro_off)
    }

    fn children(&self, first: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::successors((first != NIL).then_some(first), move |&c| {
            let n = self.frags[c as usize].next;
            (n != NIL).then_some(n)
        })
    }

    /// Split `f` so that a fragment starts at `at`; returns it.
    fn split(&mut self, f: u32, at: u32) -> u32 {
        let fr = &self.frags[f as usize];
        if at == fr.off {
            return f;
        }
        debug_assert!(at > fr.off && at < fr.off + fr.len);
        let g = self.frags.len() as u32;
        let gf = Frag {
            op: fr.op,
            off: at,
            len: fr.off + fr.len - at,
            parent: f,
            chunk: NIL,
            first_l: NIL,
            first_r: fr.first_r,
            next: NIL,
            ro_op: fr.ro_op,
            ro_off: fr.ro_off,
            del: fr.del,
            side: Side::R,
        };
        let op = fr.op;
        let moved: Vec<u32> = self.children(gf.first_r).collect();
        for c in moved {
            self.frags[c as usize].parent = g;
        }
        self.frags.push(gf);
        let fm = &mut self.frags[f as usize];
        fm.len = at - fm.off;
        fm.first_r = g;
        self.index_add(op, at, g);
        self.place(Place::After(f), g);
        g
    }

    fn last_char_start(&self, op: OpId, end: u32) -> u32 {
        let b = self.bytes_of(op).expect("op in doc");
        let mut p = end as usize - 1;
        while p > 0 && is_cont(b[p]) {
            p -= 1;
        }
        p as u32
    }

    fn leftmost(&self, mut f: u32) -> u32 {
        while self.frags[f as usize].first_l != NIL {
            f = self.frags[f as usize].first_l;
        }
        f
    }

    fn rightmost(&self, mut f: u32) -> u32 {
        while let Some(c) = self.children(self.frags[f as usize].first_r).last() {
            f = c;
        }
        f
    }

    fn fid(&self, f: u32) -> ElemId {
        let fr = &self.frags[f as usize];
        ElemId {
            op: self.ops[fr.op as usize],
            off: fr.off,
        }
    }

    fn insert_run(
        &mut self,
        op: u32,
        off: u32,
        len: u32,
        left: Option<ElemId>,
        right: Option<ElemId>,
    ) {
        if let Some(l) = left {
            let f = self.find(l).expect("left exists");
            let e = l.off + char_len(self.bytes_of(l.op).unwrap()[l.off as usize]);
            let fr = &self.frags[f as usize];
            if e < fr.off + fr.len {
                self.split(f, e);
            }
        }
        if let Some(r) = right {
            let f = self.find(r).expect("right exists");
            self.split(f, r.off);
        }
        let fl = match left {
            None => START_F,
            Some(l) => self.find(l).unwrap(),
        };
        let fr = right.map(|r| self.find(r).unwrap());
        // Side (7.3.2): left child of `right` iff `right` is the leftmost
        // element of `left`'s right subtree.
        let under = fr.is_some_and(|fr| {
            let mut f = fr;
            while self.frags[f as usize].side == Side::L {
                f = self.frags[f as usize].parent;
            }
            self.frags[f as usize].parent == fl
        });
        let n = self.frags.len() as u32;
        let nid = ElemId {
            op: self.ops[op as usize],
            off,
        };
        let (parent, side) = match (under, fr) {
            (true, Some(fr)) => (fr, Side::L),
            _ => (fl, Side::R),
        };
        let (ro_op, ro_off) = match right {
            None => (NIL, 0),
            Some(r) => (self.op_ix[&r.op], r.off),
        };
        self.frags.push(Frag {
            op,
            off,
            len,
            parent,
            chunk: NIL,
            first_l: NIL,
            first_r: NIL,
            next: NIL,
            ro_op,
            ro_off,
            del: 0,
            side,
        });
        self.index_add(op, off, n);
        // Find the sibling `n` goes before (`None` = last), then place.
        let (first, before) = if side == Side::R {
            // Right siblings: right origin later in the list first, then
            // ElemId ascending (FugueMax).
            let pn = self.epos(ro_op, ro_off);
            let first = self.frags[fl as usize].first_r;
            let before = self.children(first).find(|&s| {
                let sf = &self.frags[s as usize];
                let ps = self.epos(sf.ro_op, sf.ro_off);
                pn > ps || (pn == ps && nid < self.fid(s))
            });
            (first, before)
        } else {
            // Left siblings: ElemId ascending.
            let first = self.frags[parent as usize].first_l;
            let before = self.children(first).find(|&s| nid < self.fid(s));
            (first, before)
        };
        let p = match (before, side) {
            (Some(s), _) => Place::Before(self.leftmost(s)),
            (None, Side::R) if first == NIL => Place::After(fl),
            (None, Side::R) => Place::After(self.rightmost(fl)),
            (None, Side::L) => Place::Before(parent),
        };
        // Link `n` into the sibling list before `before`.
        let prev = self
            .children(first)
            .take_while(|&s| Some(s) != before)
            .last();
        self.frags[n as usize].next = before.unwrap_or(NIL);
        match prev {
            Some(pv) => self.frags[pv as usize].next = n,
            None if side == Side::R => self.frags[fl as usize].first_r = n,
            None => self.frags[parent as usize].first_l = n,
        }
        self.place(p, n);
    }

    /// The deleter set `d` (op indices).
    fn del_list(&self, d: u32) -> Vec<u32> {
        if d & ONE != 0 {
            vec![d & !ONE]
        } else {
            self.delsets[d as usize].clone()
        }
    }

    fn any_del(&self, d: u32, f: impl Fn(u32) -> bool) -> bool {
        if d & ONE != 0 {
            f(d & !ONE)
        } else {
            self.delsets[d as usize].iter().any(|&x| f(x))
        }
    }

    fn add_deleter(&mut self, set: u32, by: u32) -> u32 {
        if set == 0 {
            return ONE | by;
        }
        let mut v = self.del_list(set);
        let Err(i) = v.binary_search(&by) else {
            return set;
        };
        v.insert(i, by);
        if let Some(&k) = self.delset_ix.get(&v) {
            return k;
        }
        let k = self.delsets.len() as u32;
        assert!(k < ONE, "too many deleter sets");
        self.delsets.push(v.clone());
        self.delset_ix.insert(v, k);
        k
    }

    fn delete(&mut self, by: u32, e: ElemId, len: u32) {
        let end = e.off + len;
        let mut f = self.find(e).expect("deleted element exists");
        f = self.split(f, e.off);
        loop {
            let fr = &self.frags[f as usize];
            if fr.off + fr.len > end {
                self.split(f, end);
            }
            let set = self.frags[f as usize].del;
            self.frags[f as usize].del = self.add_deleter(set, by);
            let fr = &self.frags[f as usize];
            let next = fr.off + fr.len;
            if next >= end {
                break;
            }
            f = self
                .find(ElemId {
                    op: e.op,
                    off: next,
                })
                .expect("range inside op");
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Frag> + '_ {
        self.order
            .iter()
            .flat_map(move |&c| self.chunks[c as usize].iter())
            .map(move |&f| &self.frags[f as usize])
    }

    fn slice(&self, f: &Frag) -> &[u8] {
        &self.op_slice(f.op)[f.off as usize..(f.off + f.len) as usize]
    }

    fn visible_at(&self, f: &Frag, in_v: &dyn Fn(OpId) -> bool) -> bool {
        in_v(self.ops[f.op as usize]) && !self.any_del(f.del, |d| in_v(self.ops[d as usize]))
    }

    /// Per fragment in list order: `(op, deleting ops, bytes)` (notices).
    fn frag_views(&self) -> impl Iterator<Item = (OpId, Vec<OpId>, &[u8])> + '_ {
        self.iter().map(move |f| {
            let dels = self
                .del_list(f.del)
                .into_iter()
                .map(|d| self.ops[d as usize])
                .collect();
            (self.ops[f.op as usize], dels, self.slice(f))
        })
    }

    /// 7.3.4: bytes, in list order, of elements whose op satisfies `in_v`
    /// and that no delete satisfying `in_v` covers.
    pub fn materialize(&self, in_v: &dyn Fn(OpId) -> bool) -> Vec<u8> {
        let mut out = Vec::new();
        for f in self.iter() {
            if self.visible_at(f, in_v) {
                out.extend_from_slice(self.slice(f));
            }
        }
        out
    }

    /// Head text: every integrated op.
    pub fn materialize_head(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for f in self.iter() {
            if f.del == 0 {
                out.extend_from_slice(self.slice(f));
            }
        }
        out
    }

    /// Visible text at `in_v` as runs `(first element, bytes)`, contiguous
    /// offsets of one op merged (the diff base, 7.5.1).
    pub fn visible_runs(&self, in_v: &dyn Fn(OpId) -> bool) -> Vec<(ElemId, Vec<u8>)> {
        let mut out: Vec<(ElemId, Vec<u8>)> = Vec::new();
        for f in self.iter() {
            if !self.visible_at(f, in_v) {
                continue;
            }
            let op = self.ops[f.op as usize];
            if let Some((id, b)) = out.last_mut() {
                if id.op == op && id.off + b.len() as u32 == f.off {
                    b.extend_from_slice(self.slice(f));
                    continue;
                }
            }
            out.push((ElemId { op, off: f.off }, self.slice(f).to_vec()));
        }
        out
    }

    /// Anchor mapping (7.5.1): the element after `left` (`None` = START)
    /// in the full list (tombstones included) of the elements whose op
    /// satisfies `in_deps`; `None` = END.
    pub fn successor_at(
        &self,
        left: Option<ElemId>,
        in_deps: &dyn Fn(OpId) -> bool,
    ) -> Option<ElemId> {
        // Start right after `left`'s fragment (or at the front for START).
        let (r0, i0) = match left {
            None => (0, 0),
            Some(l) => {
                let f = self.find(l)?;
                let fr = &self.frags[f as usize];
                let e = l.off + char_len(self.bytes_of(l.op).unwrap()[l.off as usize]);
                if e < fr.off + fr.len {
                    return Some(ElemId { op: l.op, off: e });
                }
                let (c, i) = self.locate(f);
                (self.rank[c] as usize, i + 1)
            }
        };
        for (k, &c) in self.order.iter().enumerate().skip(r0) {
            let from = if k == r0 { i0 } else { 0 };
            for &f in self.chunks[c as usize].iter().skip(from) {
                let fr = &self.frags[f as usize];
                if in_deps(self.ops[fr.op as usize]) {
                    return Some(self.fid(f));
                }
            }
        }
        None
    }

    /// Every element in list order with its deleting ops (tests, oracle
    /// comparison): `(id, byte length, deleters)`.
    pub fn elements(&self) -> Vec<(ElemId, u32, Vec<OpId>)> {
        let mut out = Vec::new();
        for f in self.iter() {
            let b = self.op_slice(f.op);
            let op = self.ops[f.op as usize];
            let mut dels: Vec<OpId> = self
                .del_list(f.del)
                .into_iter()
                .map(|d| self.ops[d as usize])
                .collect();
            dels.sort();
            let mut p = f.off;
            while p < f.off + f.len {
                let n = char_len(b[p as usize]);
                out.push((ElemId { op, off: p }, n, dels.clone()));
                p += n;
            }
        }
        out
    }

    /// Element before the run starting at `first` and after the run
    /// ending at element `last`, in the full list (7.8 touch neighbours).
    fn neighbours(&self, first: ElemId, last: ElemId) -> (Option<ElemId>, Option<ElemId>) {
        let ff = self.find(first).unwrap();
        let prev = if first.off > self.frags[ff as usize].off {
            Some(ElemId {
                op: first.op,
                off: self.last_char_start(first.op, first.off),
            })
        } else {
            self.adjacent(ff, false).map(|g| {
                let gf = &self.frags[g as usize];
                let op = self.ops[gf.op as usize];
                ElemId {
                    op,
                    off: self.last_char_start(op, gf.off + gf.len),
                }
            })
        };
        let lf = self.find(last).unwrap();
        let lfr = &self.frags[lf as usize];
        let e = last.off + char_len(self.bytes_of(last.op).unwrap()[last.off as usize]);
        let next = if e < lfr.off + lfr.len {
            Some(ElemId {
                op: last.op,
                off: e,
            })
        } else {
            self.adjacent(lf, true).map(|g| self.fid(g))
        };
        (prev, next)
    }

    fn adjacent(&self, f: u32, forward: bool) -> Option<u32> {
        let (c, i) = self.locate(f);
        let r = self.rank[c] as usize;
        if forward {
            if let Some(&g) = self.chunks[c].get(i + 1) {
                return Some(g);
            }
            self.order[r + 1..]
                .iter()
                .find_map(|&ch| self.chunks[ch as usize].first().copied())
        } else {
            if i > 0 {
                return Some(self.chunks[c][i - 1]);
            }
            self.order[..r]
                .iter()
                .rev()
                .find_map(|&ch| self.chunks[ch as usize].last().copied())
        }
    }
}

// ------------------------------------------------------- evaluation pass

/// The epoch op of `node` named by a `TextEdit`, if it is an Applied
/// `Create` of the File `node` or an Applied `SetContent` of `node`
/// (7.4.2); returns its blob.
pub(crate) fn epoch_blob(
    ops: &BTreeMap<OpId, Op>,
    outcomes: &BTreeMap<OpId, Outcome>,
    node: NodeId,
    epoch: OpId,
) -> Option<BlobId> {
    if outcomes.get(&epoch) != Some(&Outcome::Applied) {
        return None;
    }
    match &ops.get(&epoch)?.payload {
        Payload::Create {
            kind: Kind::File, ..
        } if epoch == node => Some(BlobId::empty()),
        Payload::SetContent { node: n, blob } if *n == node => Some(*blob),
        _ => None,
    }
}

/// Validity and integration of every structurally applicable `TextEdit`
/// (in `order`, a causal order of all op ids; `<` when `None`). Returns
/// the text evaluation (docs, valid set) and the invalid ops.
pub(crate) fn text_pass(
    ops: &BTreeMap<OpId, Op>,
    outcomes: &BTreeMap<OpId, Outcome>,
    causal: &Causality,
    blobs: &dyn BlobSource,
    order: Option<&[OpId]>,
) -> (TextEval, Vec<OpId>) {
    let mut tv = TextEval::default();
    let mut invalid = Vec::new();
    let sorted: Vec<OpId>;
    let order = match order {
        Some(o) => o,
        None => {
            sorted = ops.keys().copied().collect();
            &sorted
        }
    };
    for &id in order {
        let Some(op) = ops.get(&id) else { continue };
        let Payload::TextEdit(t) = &op.payload else {
            continue;
        };
        if outcomes.get(&id) != Some(&Outcome::Applied) {
            continue;
        }
        let eb = epoch_blob(ops, outcomes, t.node, t.epoch);
        let in_deps = |x: OpId| causal.precedes(x, id);
        if text_step(id, t, eb, &in_deps, &mut tv.docs, blobs) {
            tv.valid.insert(id);
        } else {
            invalid.push(id);
        }
    }
    invalid.sort();
    (tv, invalid)
}

/// 7.4.1/7.4.2 validity of one structurally Applied `TextEdit` `id` and,
/// if valid, its integration into its epoch's doc (created from the epoch
/// blob on first use). `epoch_blob` = [`epoch_blob`] of its epoch;
/// `in_deps(x)` = `x ∈ deps*(id)`. Validity depends only on the op's
/// causal past, so any causal order of calls gives the same docs (Issue
/// #54: the incremental [`crate::tree::Replica`] calls it per new op).
pub(crate) fn text_step(
    id: OpId,
    t: &TextEdit,
    epoch_blob: Option<BlobId>,
    in_deps: &dyn Fn(OpId) -> bool,
    docs: &mut BTreeMap<(NodeId, OpId), DocSlot>,
    blobs: &dyn BlobSource,
) -> bool {
    let ok = (|| {
        let blob = epoch_blob?;
        if !in_deps(t.epoch) || !t.seen.iter().all(|&s| in_deps(s)) {
            return None;
        }
        let slot = docs
            .entry((t.node, t.epoch))
            .or_insert_with(|| match fetch(blobs, &blob) {
                Some(b) if std::str::from_utf8(&b).is_ok() => {
                    DocSlot::Doc(Box::new(TextDoc::new(t.epoch, &b)))
                }
                Some(_) => DocSlot::Deferred,
                None => DocSlot::Deferred,
            });
        match slot {
            DocSlot::Doc(d) => {
                if !d.check(t, in_deps) {
                    return None;
                }
                d.apply(id, t);
            }
            // Not loaded (its seed is missing or damaged): the caller has
            // recorded the error; the op is not integrated.
            DocSlot::Lazy(..) => return None,
            DocSlot::Deferred => {
                // A blob that is present but not UTF-8 is a bytes
                // epoch: no TextEdit on it is valid.
                if fetch(blobs, &blob).is_some()
                    || !t
                        .ins
                        .iter()
                        .all(|i| !i.bytes.is_empty() && std::str::from_utf8(&i.bytes).is_ok())
                {
                    return None;
                }
            }
        }
        Some(())
    })();
    ok.is_some()
}

fn text_of(op: &Op) -> Option<&TextEdit> {
    match &op.payload {
        Payload::TextEdit(t) => Some(t),
        _ => None,
    }
}

/// Content op `c`'s epoch (a SetContent is its own epoch).
fn epoch_of(op: &Op) -> OpId {
    match &op.payload {
        Payload::TextEdit(t) => t.epoch,
        _ => op.id,
    }
}

/// 7.4.3 content and conflicts plus 7.8 notices for every node with a
/// valid `TextEdit`. Fills `tv.content`, `tv.notices`, `tv.blobs`;
/// returns the `content` conflicts of those nodes.
pub(crate) fn derive(
    ops: &BTreeMap<OpId, Op>,
    outcomes: &BTreeMap<OpId, Outcome>,
    causal: &Causality,
    blobs: &dyn BlobSource,
    tv: &mut TextEval,
) -> Vec<Conflict> {
    let nodes: BTreeSet<NodeId> = tv
        .valid
        .iter()
        .map(|v| text_of(&ops[v]).unwrap().node)
        .collect();
    // Content ops per text node, ascending (one pass, Issue #54).
    let mut by_node: BTreeMap<NodeId, Vec<OpId>> = BTreeMap::new();
    for (id, o) in ops {
        let n = match &o.payload {
            Payload::SetContent { node, .. } if outcomes[id] == Outcome::Applied => *node,
            Payload::TextEdit(t) if tv.valid.contains(id) => t.node,
            _ => continue,
        };
        if nodes.contains(&n) {
            by_node.entry(n).or_default().push(*id);
        }
    }
    let mut conflicts = Vec::new();
    for (node, cs) in by_node {
        let d = derive_node(node, &cs, &|x| &ops[&x], causal, blobs, &tv.docs);
        tv.content.insert(node, d.content);
        tv.notices.extend(d.notices);
        tv.blobs.extend(d.blobs);
        conflicts.extend(d.conflicts);
    }
    conflicts
}

/// What [`derive_node`] derives for one text node.
pub(crate) struct NodeDerived {
    pub content: NodeText,
    pub conflicts: Vec<Conflict>,
    pub notices: Vec<Notice>,
    pub blobs: Vec<(BlobId, Vec<u8>)>,
}

/// 7.4.3/7.8 for one node: `cs` = its content ops (Applied `SetContent`s
/// and valid `TextEdit`s), ascending; `get` resolves any op id named by
/// them (content ops and epochs).
pub(crate) fn derive_node<'o>(
    node: NodeId,
    cs: &[OpId],
    get: &dyn Fn(OpId) -> &'o Op,
    causal: &Causality,
    blobs: &dyn BlobSource,
    docs: &BTreeMap<(NodeId, OpId), DocSlot>,
) -> NodeDerived {
    let mut conflicts = Vec::new();
    let mut out_blobs = Vec::new();
    let doc = |e: OpId| match docs.get(&(node, e)) {
        Some(DocSlot::Doc(d)) => Some(&**d),
        _ => None,
    };
    let w = *cs.last().unwrap();
    let we = epoch_of(get(w));
    let epoch_blob_id = |e: OpId| match &get(e).payload {
        Payload::SetContent { blob, .. } => *blob,
        _ => BlobId::empty(),
    };
    // Blob of a content op (7.4.3): a SetContent's blob; for a text
    // op, its epoch materialized at deps*(x) ∪ {x}.
    let mut op_blob = |x: OpId| -> BlobId {
        match &get(x).payload {
            Payload::SetContent { blob, .. } => *blob,
            Payload::TextEdit(t) => match doc(t.epoch) {
                Some(d) => {
                    let b = d.materialize(&|o| o == x || causal.precedes(o, x));
                    let id = BlobId::of(&b);
                    out_blobs.push((id, b));
                    id
                }
                None => epoch_blob_id(t.epoch),
            },
            _ => unreachable!(),
        }
    };
    let (blob, bytes) = match docs.get(&(node, we)) {
        Some(DocSlot::Doc(d)) => {
            let b = d.materialize_head();
            let id = BlobId::of(&b);
            (id, Some(b))
        }
        // (A lazy doc is loaded by the replica before deriving.)
        Some(DocSlot::Deferred | DocSlot::Lazy(..)) => (epoch_blob_id(we), None),
        None => {
            let id = epoch_blob_id(we);
            (id, fetch(blobs, &id).map(|b| b.to_vec()))
        }
    };
    // Winner blob = the merged head of the winning epoch (Issue #53
    // repair R2): "keep winner" never drops concurrent merged edits.
    let winner_blob = blob;
    for &e in &cs[..cs.len() - 1] {
        if epoch_of(get(e)) == we {
            continue;
        }
        let over = cs.iter().any(|&c| {
            c != e
                && causal.precedes(e, c)
                && match &get(c).payload {
                    Payload::TextEdit(t) => t.seen.iter().any(|&s| s == e || causal.precedes(e, s)),
                    _ => true,
                }
        });
        if !over {
            let loser_blob = op_blob(e);
            conflicts.push(Conflict::Content {
                node,
                winner: w,
                loser: e,
                winner_blob,
                loser_blob,
            });
        }
    }
    let mut ns = Vec::new();
    if let Some(d) = doc(we) {
        let group: Vec<OpId> = cs
            .iter()
            .copied()
            .filter(|c| text_of(get(*c)).is_some_and(|t| t.epoch == we))
            .collect();
        ns = notices(get, causal, d, node, &group, cs);
    }
    if let (Some(DocSlot::Doc(_)), Some(b)) = (docs.get(&(node, we)), &bytes) {
        out_blobs.push((blob, b.clone()));
    }
    NodeDerived {
        content: NodeText {
            epoch: we,
            blob,
            bytes,
        },
        conflicts,
        notices: ns,
        blobs: out_blobs,
    }
}

/// Touch set of a text op (7.8) as byte ranges `(op, start, end)`.
fn touch(d: &TextDoc, id: OpId, t: &TextEdit) -> Vec<(OpId, u32, u32)> {
    let mut v = Vec::new();
    let pt = |e: ElemId| (e.op, e.off, e.off + 1);
    for &(e, len) in &t.del {
        v.push((e.op, e.off, e.off + len));
    }
    let mut off = 0u32;
    for i in &t.ins {
        for e in [i.left, i.right].into_iter().flatten() {
            v.push(pt(e));
        }
        let n = i.bytes.len() as u32;
        let first = ElemId { op: id, off };
        let last = ElemId {
            op: id,
            off: d.last_char_start(id, off + n),
        };
        let (p, q) = d.neighbours(first, last);
        v.extend(p.map(pt));
        v.extend(q.map(pt));
        off += n;
    }
    v
}

fn find_root(p: &mut [usize], mut x: usize) -> usize {
    while p[x] != x {
        p[x] = p[p[x]];
        x = p[x];
    }
    x
}

/// `concurrent-edit` notices of one node's winning epoch (7.8, D-053
/// E-T2: on; identical concurrent edits are not flagged).
fn notices<'o>(
    get: &dyn Fn(OpId) -> &'o Op,
    causal: &Causality,
    d: &TextDoc,
    node: NodeId,
    group: &[OpId],
    content_ops: &[OpId],
) -> Vec<Notice> {
    if group.len() < 2 {
        return Vec::new();
    }
    let mut pts: Vec<(OpId, u32, u32, usize)> = Vec::new();
    for (k, &g) in group.iter().enumerate() {
        for (o, s, e) in touch(d, g, text_of(get(g)).unwrap()) {
            pts.push((o, s, e, k));
        }
    }
    pts.sort();
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut active: Vec<(OpId, u32, usize)> = Vec::new();
    for &(o, s, e, k) in &pts {
        active.retain(|&(ao, ae, _)| ao == o && ae > s);
        for &(_, _, ak) in &active {
            if ak != k {
                pairs.insert((ak.min(k), ak.max(k)));
            }
        }
        active.push((o, e, k));
    }
    let mut parent: Vec<usize> = (0..group.len()).collect();
    for (a, b) in pairs {
        let (ta, tb) = (
            text_of(get(group[a])).unwrap(),
            text_of(get(group[b])).unwrap(),
        );
        if ta.del == tb.del && ta.ins == tb.ins {
            continue;
        }
        if causal.concurrent(group[a], group[b]) {
            let (ra, rb) = (find_root(&mut parent, a), find_root(&mut parent, b));
            parent[ra.max(rb)] = ra.min(rb);
        }
    }
    let mut comps: BTreeMap<usize, Vec<OpId>> = BTreeMap::new();
    for (k, &g) in group.iter().enumerate() {
        let r = find_root(&mut parent, k);
        comps.entry(r).or_default().push(g);
    }
    let mut out = Vec::new();
    for (_, g) in comps {
        if g.len() < 2 {
            continue;
        }
        let cleared = content_ops
            .iter()
            .any(|&c| g.iter().all(|&x| causal.precedes(x, c)));
        if cleared {
            continue;
        }
        let set: BTreeSet<OpId> = g.iter().copied().collect();
        out.push(Notice::ConcurrentEdit {
            node,
            lines: notice_lines(d, &set),
            ops: g,
        });
    }
    out
}

/// First and last head line (1-based) touched by `set`'s inserted and
/// deleted fragments; a hidden fragment counts at the line of the next
/// visible character.
fn notice_lines(d: &TextDoc, set: &BTreeSet<OpId>) -> (u32, u32) {
    let head = d.materialize_head();
    let nl = head.iter().filter(|&&b| b == b'\n').count() as u32;
    let total = (nl + u32::from(!head.is_empty() && *head.last().unwrap() != b'\n')).max(1);
    let mut line = 1u32;
    let (mut lo, mut hi) = (u32::MAX, 0u32);
    for (op, dels, b) in d.frag_views() {
        let mine = set.contains(&op) || dels.iter().any(|x| set.contains(x));
        let visible = dels.is_empty();
        if mine {
            let (a, z) = if visible {
                let inner = b[..b.len() - 1].iter().filter(|&&c| c == b'\n').count() as u32;
                (line, line + inner)
            } else {
                (line.min(total), line.min(total))
            };
            lo = lo.min(a);
            hi = hi.max(z);
        }
        if visible {
            line += b.iter().filter(|&&c| c == b'\n').count() as u32;
        }
    }
    if hi == 0 {
        (1, 1)
    } else {
        (lo, hi)
    }
}

// ------------------------------------------------------ hub-diff-line/1

/// Units of the line diff (7.5.2): a leading BOM is its own pseudo-line;
/// a line is the bytes up to and including `\n` (the last may lack it).
pub fn split_lines(t: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = t;
    if rest.starts_with(&[0xEF, 0xBB, 0xBF]) {
        out.push(&rest[..3]);
        rest = &rest[3..];
    }
    while !rest.is_empty() {
        let n = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |p| p + 1);
        out.push(&rest[..n]);
        rest = &rest[n..];
    }
    out
}

/// One step of the line edit script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineOp {
    /// `a[i] == b[j]`.
    Eq(usize, usize),
    /// Delete `a[i]`.
    Del(usize),
    /// Insert `b[j]`.
    Ins(usize),
}

fn myers_step(a: &[&[u8]], b: &[&[u8]], v: &mut [i64], off: i64, d: i64) -> bool {
    let (n, m) = (a.len() as i64, b.len() as i64);
    let mut k = -d;
    while k <= d {
        let i = (k + off) as usize;
        // Tie-break (7.5.2): at equal reach prefer the deletion.
        let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) {
            v[i + 1]
        } else {
            v[i - 1] + 1
        };
        let mut y = x - k;
        while x < n && y < m && a[x as usize] == b[y as usize] {
            x += 1;
            y += 1;
        }
        v[i] = x;
        if x >= n && y >= m {
            return true;
        }
        k += 2;
    }
    false
}

/// Minimal line edit script (Myers 1986 greedy forward, ties to the
/// deletion), or `None` if it needs more than `max_d` edits. Memory is
/// O((N+M)·√D): V arrays are checkpointed and recomputed per block while
/// backtracking (same result as keeping the full trace).
pub fn diff_lines(a: &[&[u8]], b: &[&[u8]], max_d: usize) -> Option<Vec<LineOp>> {
    let (n, m) = (a.len() as i64, b.len() as i64);
    let off = n + m + 1;
    let size = (2 * off + 1) as usize;
    let mut v = vec![0i64; size];
    v[(1 + off) as usize] = 0;
    let mut dd = None;
    for d in 0..=(n + m) {
        if d as usize > max_d {
            return None;
        }
        if myers_step(a, b, &mut v, off, d) {
            dd = Some(d);
            break;
        }
    }
    let dd = dd.unwrap_or(0);
    let block = ((dd as f64).sqrt() as i64).max(16);
    // V_s is only read on diagonals -s..=s: keep that window.
    let window = |v: &[i64], s: i64| v[(off - s - 1) as usize..=(off + s + 1) as usize].to_vec();
    // Checkpoints: V after step d, for d % block == 0.
    let mut cps: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut v = vec![0i64; size];
    for d in 0..dd {
        myers_step(a, b, &mut v, off, d);
        if d % block == 0 {
            cps.insert(d, window(&v, d));
        }
    }
    // Windows V_t for t in the cached block.
    let mut cache: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let get = |t: i64, cache: &mut BTreeMap<i64, Vec<i64>>| {
        if cache.contains_key(&t) {
            return;
        }
        cache.clear();
        let c = t - t % block;
        let mut w = vec![0i64; size];
        let cp = &cps[&c];
        w[(off - c - 1) as usize..=(off + c + 1) as usize].copy_from_slice(cp);
        cache.insert(c, cp.clone());
        for s in c + 1..(c + block).min(dd) {
            myers_step(a, b, &mut w, off, s);
            cache.insert(s, window(&w, s));
        }
    };
    let mut out = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=dd).rev() {
        get(d - 1, &mut cache);
        let vp = &cache[&(d - 1)];
        let k = x - y;
        let at = |k: i64| vp[(k + d) as usize];
        let ins = k == -d || (k != d && at(k - 1) < at(k + 1));
        let pk = if ins { k + 1 } else { k - 1 };
        let px = at(pk);
        let py = px - pk;
        let mx = if ins { px } else { px + 1 };
        while x > mx {
            x -= 1;
            y -= 1;
            out.push(LineOp::Eq(x as usize, y as usize));
        }
        if ins {
            out.push(LineOp::Ins(py as usize));
        } else {
            out.push(LineOp::Del(px as usize));
        }
        x = px;
        y = py;
    }
    while x > 0 {
        x -= 1;
        y -= 1;
        out.push(LineOp::Eq(x as usize, y as usize));
    }
    out.reverse();
    Some(out)
}

/// `hub-diff-line/1` (7.5.2): the `del`/`ins` of a `TextEdit` turning the
/// base text (visible runs with ids, e.g. [`TextDoc::visible_runs`]) into
/// `saved`. `succ` is the anchor mapping (7.5.1): the successor of `left`
/// in the full list at the op's `deps`. `None` for identical bytes (no-op
/// save).
pub fn diff_line(
    base: &[(ElemId, Vec<u8>)],
    saved: &[u8],
    succ: &dyn Fn(Option<ElemId>) -> Option<ElemId>,
) -> Option<DiffEdit> {
    let text: Vec<u8> = base.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    if text == saved {
        return None;
    }
    // Byte offset -> element id.
    let mut starts = Vec::with_capacity(base.len());
    let mut p = 0usize;
    for (id, b) in base {
        starts.push((p, *id));
        p += b.len();
    }
    let id_at = |p: usize| {
        let i = starts.partition_point(|&(s, _)| s <= p) - 1;
        let (s, id) = starts[i];
        ElemId {
            op: id.op,
            off: id.off + (p - s) as u32,
        }
    };
    let ranges = |p0: usize, p1: usize| {
        let mut v: Vec<(ElemId, u32)> = Vec::new();
        for (k, &(s, id)) in starts.iter().enumerate() {
            let e = starts.get(k + 1).map_or(text.len(), |x| x.0);
            let (a, z) = (p0.max(s), p1.min(e));
            if a < z {
                v.push((
                    ElemId {
                        op: id.op,
                        off: id.off + (a - s) as u32,
                    },
                    (z - a) as u32,
                ));
            }
        }
        v
    };
    let last_char = |end: usize| {
        let mut q = end - 1;
        while q > 0 && is_cont(text[q]) {
            q -= 1;
        }
        id_at(q)
    };
    let la = split_lines(&text);
    let lb = split_lines(saved);
    let mut a_off = Vec::with_capacity(la.len() + 1);
    let mut s = 0;
    for l in &la {
        a_off.push(s);
        s += l.len();
    }
    a_off.push(s);
    let script = diff_lines(&la, &lb, DIFF_MAX_D).unwrap_or_else(|| {
        let mut v: Vec<LineOp> = (0..la.len()).map(LineOp::Del).collect();
        v.extend((0..lb.len()).map(LineOp::Ins));
        v
    });
    let mut del = Vec::new();
    let mut ins = Vec::new();
    let mut i = 0;
    let mut a_pos = 0usize; // next base line
    while i < script.len() {
        if let LineOp::Eq(ai, _) = script[i] {
            a_pos = ai + 1;
            i += 1;
            continue;
        }
        let (mut d0, mut d1) = (usize::MAX, 0usize);
        let mut bytes = Vec::new();
        while i < script.len() {
            match script[i] {
                LineOp::Eq(..) => break,
                LineOp::Del(ai) => {
                    d0 = d0.min(ai);
                    d1 = d1.max(ai + 1);
                }
                LineOp::Ins(bj) => bytes.extend_from_slice(lb[bj]),
            }
            i += 1;
        }
        let left = if d0 != usize::MAX {
            let (p0, p1) = (a_off[d0], a_off[d1]);
            del.extend(ranges(p0, p1));
            a_pos = d1;
            Some(last_char(p1))
        } else if a_pos > 0 {
            Some(last_char(a_off[a_pos]))
        } else {
            None
        };
        if !bytes.is_empty() {
            ins.push(Ins {
                left,
                right: succ(left),
                bytes,
            });
        }
    }
    Some((del, ins))
}

/// Content heads of `node` in an op set (7.4.1 `seen`): its content ops
/// (Applied SetContents, valid TextEdits) that no other one succeeds.
pub fn content_heads(
    ops: &BTreeMap<OpId, Op>,
    ev: &crate::tree::Evaluation,
    node: NodeId,
) -> BTreeSet<OpId> {
    let cs: Vec<OpId> = ops
        .iter()
        .filter(|(id, o)| match &o.payload {
            Payload::SetContent { node: n, .. } => {
                *n == node && ev.outcomes[*id] == Outcome::Applied
            }
            Payload::TextEdit(t) => t.node == node && ev.text.valid.contains(id),
            _ => false,
        })
        .map(|(id, _)| *id)
        .collect();
    let causal = Causality::new(ops);
    cs.iter()
        .copied()
        .filter(|&c| !cs.iter().any(|&d| causal.precedes(c, d)))
        .collect()
}

/// The winning epoch of `node` in `ev` (7.4.3) with its blob: the epoch of
/// the greatest content op, or the node's `Create`.
pub fn winning_epoch(
    ops: &BTreeMap<OpId, Op>,
    ev: &crate::tree::Evaluation,
    node: NodeId,
) -> Option<(OpId, BlobId)> {
    if let Some(nt) = ev.text.content.get(&node) {
        let blob = match &ops[&nt.epoch].payload {
            Payload::SetContent { blob, .. } => *blob,
            _ => BlobId::empty(),
        };
        return Some((nt.epoch, blob));
    }
    let last = ops.iter().rev().find(|(id, o)| {
        matches!(&o.payload, Payload::SetContent { node: n, .. } if *n == node)
            && ev.outcomes[*id] == Outcome::Applied
    });
    match last {
        Some((id, o)) => match &o.payload {
            Payload::SetContent { blob, .. } => Some((*id, *blob)),
            _ => unreachable!(),
        },
        None => (ops.get(&node).is_some()
            && ev.state.get(&node).is_some_and(|n| n.kind == Kind::File))
        .then(|| (node, BlobId::empty())),
    }
}

/// Author a save of `node` as a `TextEdit` (7.5.1): `base_ops` is the
/// author's base (the index base `V_wd`, which is also the op's `deps`)
/// and `ev` its evaluation. The diff runs against the winning epoch's text
/// at the base; `seen` = the node's content heads at the base; each
/// `right` = the successor of `left` in the full list at the base.
/// Returns `None` for a no-op save or if the epoch text is unavailable.
pub fn author_text_edit(
    base_ops: &BTreeMap<OpId, Op>,
    ev: &crate::tree::Evaluation,
    blobs: &dyn BlobSource,
    node: NodeId,
    saved: &[u8],
) -> Option<TextEdit> {
    author_text_edit_on(ev, base_ops, ev, blobs, node, saved)
}

/// [`author_text_edit`] when the diff base differs from the op's deps
/// (7.5.1: the API's server-side diff, the hash-mismatch fallback):
/// epoch, `seen` and the diff's base text come from `base_ops` (`ev_base`),
/// the anchor mapping from the full list of `ev_deps` (the evaluation of
/// the op's deps, a superset of `base_ops`).
pub fn author_text_edit_on(
    ev_deps: &crate::tree::Evaluation,
    base_ops: &BTreeMap<OpId, Op>,
    ev_base: &crate::tree::Evaluation,
    blobs: &dyn BlobSource,
    node: NodeId,
    saved: &[u8],
) -> Option<TextEdit> {
    let (epoch, blob) = winning_epoch(base_ops, ev_base, node)?;
    let fresh;
    let doc = match ev_deps.text.docs.get(&(node, epoch)) {
        Some(DocSlot::Doc(d)) => d,
        Some(DocSlot::Deferred | DocSlot::Lazy(..)) => return None,
        None => {
            let b = fetch(blobs, &blob)?;
            std::str::from_utf8(&b).ok()?;
            fresh = TextDoc::new(epoch, &b);
            &fresh
        }
    };
    let in_base = |o: OpId| base_ops.contains_key(&o);
    let all = |_: OpId| true;
    let base = doc.visible_runs(&in_base);
    let (del, ins) = diff_line(&base, saved, &|l| doc.successor_at(l, &all))?;
    Some(TextEdit {
        node,
        epoch,
        seen: content_heads(base_ops, ev_base, node),
        del,
        ins,
    })
}

#[cfg(any(test, feature = "oracle"))]
#[path = "text_oracle.rs"]
pub mod oracle;
