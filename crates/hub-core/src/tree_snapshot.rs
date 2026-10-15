//! Binary snapshot of a [`Replica`] (Issue #33, Wiki Appendix A.2.3).
//!
//! Compact little-endian encoding of everything a replica holds after
//! replaying a closed op set: tree state, parent setters, the applied-op
//! log with undo records and outcomes, the D-014 view and revive triples.
//! Replica ids are interned in a table; counters and lengths are LEB128.
//! There is no checksum here: the caller frames and checksums the bytes.
//!
//! Issue #54 (snapshot v2, Text-CRDT 7.11): a replica holding a `TextEdit`
//! is encoded with magic `RSNAP2`: the v1 body (op log records gain
//! payload tag 4) followed by the text part: per `(node, epoch)` the
//! epoch's `TextDoc` (seed bytes, run fragments with their tree links and
//! deleter sets, the chunked list order), the valid/invalid sets and the
//! derived content, conflicts, notices and fold values. An op's inserted
//! bytes are not repeated: they are read back from its log record. A
//! replica without text ops keeps the v1 encoding byte for byte.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{
    BlobId, Conflict, Kind, Node, NodeId, Op, OpId, Outcome, Payload, Replica, State, TextPart,
    Undo, Work,
};
use crate::text::{DocSlot, ElemId, Frag, Ins, Notice, Side, TextDoc, TextEdit, NIL};

struct W {
    buf: Vec<u8>,
    reps: HashMap<u128, u64>,
    table: Vec<u128>,
}

impl W {
    fn u(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }
    fn b(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn id(&mut self, n: NodeId) {
        self.u(n.counter);
        let next = self.table.len() as u64;
        let ix = *self.reps.entry(n.replica.0).or_insert(next);
        if ix == next {
            self.table.push(n.replica.0);
        }
        self.u(ix);
    }
    fn oid(&mut self, n: Option<NodeId>) {
        match n {
            None => self.b(0),
            Some(n) => {
                self.b(1);
                self.id(n)
            }
        }
    }
    fn s(&mut self, s: &str) {
        self.u(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn blob(&mut self, x: &BlobId) {
        self.buf.extend_from_slice(&x.0);
    }
    fn oblob(&mut self, x: &Option<BlobId>) {
        match x {
            None => self.b(0),
            Some(x) => {
                self.b(1);
                self.blob(x)
            }
        }
    }
    fn kind(&mut self, k: Kind) {
        self.b(match k {
            Kind::Root => 0,
            Kind::Directory => 1,
            Kind::File => 2,
        })
    }
    fn state(&mut self, st: &State) {
        self.u(st.len() as u64);
        for n in st.values() {
            self.id(n.id);
            self.kind(n.kind);
            self.oid(n.parent);
            self.s(&n.name);
            self.oblob(&n.content);
        }
    }
    fn op(&mut self, o: &Op) {
        self.id(o.id);
        self.u(o.deps.len() as u64);
        for d in &o.deps {
            self.id(*d);
        }
        match &o.payload {
            Payload::Create { kind, parent, name } => {
                self.b(0);
                self.kind(*kind);
                self.id(*parent);
                self.s(name);
            }
            Payload::Move {
                node,
                new_parent,
                new_name,
            } => {
                self.b(1);
                self.id(*node);
                self.id(*new_parent);
                self.s(new_name);
            }
            Payload::Delete { node } => {
                self.b(2);
                self.id(*node);
            }
            Payload::SetContent { node, blob } => {
                self.b(3);
                self.id(*node);
                self.blob(blob);
            }
            Payload::TextEdit(t) => {
                self.b(4);
                self.id(t.node);
                self.id(t.epoch);
                self.u(t.seen.len() as u64);
                for x in &t.seen {
                    self.id(*x);
                }
                self.u(t.del.len() as u64);
                for (e, n) in &t.del {
                    self.elem(*e);
                    self.u(*n as u64);
                }
                self.u(t.ins.len() as u64);
                for i in &t.ins {
                    self.oelem(i.left);
                    self.oelem(i.right);
                    self.u(i.bytes.len() as u64);
                    self.buf.extend_from_slice(&i.bytes);
                }
            }
        }
    }
    fn elem(&mut self, e: ElemId) {
        self.id(e.op);
        self.u(e.off as u64);
    }
    fn oelem(&mut self, e: Option<ElemId>) {
        match e {
            None => self.b(0),
            Some(e) => {
                self.b(1);
                self.elem(e)
            }
        }
    }
    /// `u32` index with `NIL` as 0.
    fn nix(&mut self, v: u32) {
        self.u(if v == NIL { 0 } else { v as u64 + 1 });
    }
    fn ids(&mut self, v: &[OpId]) {
        self.u(v.len() as u64);
        for x in v {
            self.id(*x);
        }
    }
    fn doc(&mut self, d: &TextDoc) {
        let seed = d.seed();
        self.u(seed.len() as u64);
        self.buf.extend_from_slice(seed);
        self.ids(&d.ops[1..]);
        for &f in &d.op_first {
            self.nix(f);
        }
        let mut split: Vec<(&u32, &Vec<(u32, u32)>)> = d.split.iter().collect();
        split.sort();
        self.u(split.len() as u64);
        for (ix, v) in split {
            self.u(*ix as u64);
            self.u(v.len() as u64);
            for &(o, f) in v {
                self.u(o as u64);
                self.nix(f);
            }
        }
        self.u(d.frags.len() as u64);
        for f in &d.frags {
            self.u(f.op as u64);
            self.u(f.off as u64);
            self.u(f.len as u64);
            self.nix(f.parent);
            self.nix(f.chunk);
            self.nix(f.first_l);
            self.nix(f.first_r);
            self.nix(f.next);
            self.nix(f.ro_op);
            self.u(f.ro_off as u64);
            self.u(f.del as u64);
            self.b(match f.side {
                Side::L => 0,
                Side::R => 1,
            });
        }
        self.u(d.delsets.len() as u64 - 1);
        for v in &d.delsets[1..] {
            self.u(v.len() as u64);
            for &x in v {
                self.u(x as u64);
            }
        }
        self.u(d.chunks.len() as u64);
        for c in &d.chunks {
            self.u(c.len() as u64);
            for &f in c {
                self.u(f as u64);
            }
        }
        for &c in &d.order {
            self.u(c as u64);
        }
        for &r in &d.rank {
            self.u(r as u64);
        }
    }
    fn text(&mut self, t: &TextPart) {
        self.u(t.docs.len() as u64);
        for ((node, epoch), slot) in &t.docs {
            self.id(*node);
            self.id(*epoch);
            match slot {
                DocSlot::Deferred => self.b(0),
                DocSlot::Doc(d) => {
                    self.b(1);
                    self.doc(d)
                }
            }
        }
        self.ids(&t.valid.iter().copied().collect::<Vec<_>>());
        self.ids(&t.invalid.iter().copied().collect::<Vec<_>>());
        self.u(t.content.len() as u64);
        for (n, (e, b)) in &t.content {
            self.id(*n);
            self.id(*e);
            self.blob(b);
        }
        self.u(t.conflicts.len() as u64);
        for (n, cs) in &t.conflicts {
            self.id(*n);
            self.u(cs.len() as u64);
            for c in cs {
                let Conflict::Content {
                    winner,
                    loser,
                    winner_blob,
                    loser_blob,
                    ..
                } = c
                else {
                    unreachable!("text conflicts are content conflicts")
                };
                self.id(*winner);
                self.id(*loser);
                self.blob(winner_blob);
                self.blob(loser_blob);
            }
        }
        self.u(t.notices.len() as u64);
        for (n, ns) in &t.notices {
            self.id(*n);
            self.u(ns.len() as u64);
            for Notice::ConcurrentEdit { ops, lines, .. } in ns {
                self.ids(ops);
                self.u(lines.0 as u64);
                self.u(lines.1 as u64);
            }
        }
        self.u(t.fold.len() as u64);
        for (n, b) in &t.fold {
            self.id(*n);
            self.oblob(b);
        }
    }
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
    table: Vec<u128>,
}

type Res<T> = Result<T, String>;

impl R<'_> {
    fn u(&mut self) -> Res<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = *self.b.get(self.p).ok_or("truncated")?;
            self.p += 1;
            if shift >= 64 {
                return Err("varint overflow".into());
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }
    fn len(&mut self) -> Res<usize> {
        let v = self.u()?;
        if v > (self.b.len() - self.p) as u64 * 8 + 16 {
            return Err("implausible length".into());
        }
        Ok(v as usize)
    }
    fn b(&mut self) -> Res<u8> {
        let v = *self.b.get(self.p).ok_or("truncated")?;
        self.p += 1;
        Ok(v)
    }
    fn bytes(&mut self, n: usize) -> Res<&[u8]> {
        let e = self.p.checked_add(n).ok_or("truncated")?;
        let s = self.b.get(self.p..e).ok_or("truncated")?;
        self.p = e;
        Ok(s)
    }
    fn id(&mut self) -> Res<NodeId> {
        let c = self.u()?;
        let ix = self.u()? as usize;
        let r = *self.table.get(ix).ok_or("bad replica index")?;
        Ok(NodeId::new(c, r))
    }
    fn oid(&mut self) -> Res<Option<NodeId>> {
        match self.b()? {
            0 => Ok(None),
            1 => Ok(Some(self.id()?)),
            _ => Err("bad option tag".into()),
        }
    }
    fn s(&mut self) -> Res<String> {
        let n = self.len()?;
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| "bad utf-8".into())
    }
    fn blob(&mut self) -> Res<BlobId> {
        let mut o = [0u8; 32];
        o.copy_from_slice(self.bytes(32)?);
        Ok(BlobId(o))
    }
    fn oblob(&mut self) -> Res<Option<BlobId>> {
        match self.b()? {
            0 => Ok(None),
            1 => Ok(Some(self.blob()?)),
            _ => Err("bad option tag".into()),
        }
    }
    fn kind(&mut self) -> Res<Kind> {
        Ok(match self.b()? {
            0 => Kind::Root,
            1 => Kind::Directory,
            2 => Kind::File,
            _ => return Err("bad kind".into()),
        })
    }
    fn state(&mut self) -> Res<State> {
        let n = self.len()?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let id = self.id()?;
            let node = Node {
                id,
                kind: self.kind()?,
                parent: self.oid()?,
                name: self.s()?,
                content: self.oblob()?,
            };
            if v.last().is_some_and(|(l, _): &(NodeId, Node)| *l >= id) {
                return Err("state not sorted".into());
            }
            v.push((id, node));
        }
        Ok(v.into_iter().collect())
    }
    fn op(&mut self) -> Res<Op> {
        let id = self.id()?;
        let nd = self.len()?;
        let mut deps = BTreeSet::new();
        for _ in 0..nd {
            deps.insert(self.id()?);
        }
        let payload = match self.b()? {
            0 => Payload::Create {
                kind: self.kind()?,
                parent: self.id()?,
                name: self.s()?,
            },
            1 => Payload::Move {
                node: self.id()?,
                new_parent: self.id()?,
                new_name: self.s()?,
            },
            2 => Payload::Delete { node: self.id()? },
            3 => Payload::SetContent {
                node: self.id()?,
                blob: self.blob()?,
            },
            4 => {
                let node = self.id()?;
                let epoch = self.id()?;
                let ns = self.len()?;
                let mut seen = BTreeSet::new();
                for _ in 0..ns {
                    seen.insert(self.id()?);
                }
                let nd = self.len()?;
                let mut del = Vec::with_capacity(nd);
                for _ in 0..nd {
                    del.push((self.elem()?, self.u32()?));
                }
                let ni = self.len()?;
                let mut ins = Vec::with_capacity(ni);
                for _ in 0..ni {
                    let left = self.oelem()?;
                    let right = self.oelem()?;
                    let n = self.len()?;
                    ins.push(Ins {
                        left,
                        right,
                        bytes: self.bytes(n)?.to_vec(),
                    });
                }
                Payload::TextEdit(TextEdit {
                    node,
                    epoch,
                    seen,
                    del,
                    ins,
                })
            }
            _ => return Err("bad payload tag".into()),
        };
        Ok(Op { id, deps, payload })
    }
    fn u32(&mut self) -> Res<u32> {
        u32::try_from(self.u()?).map_err(|_| "u32 overflow".into())
    }
    fn elem(&mut self) -> Res<ElemId> {
        Ok(ElemId {
            op: self.id()?,
            off: self.u32()?,
        })
    }
    fn oelem(&mut self) -> Res<Option<ElemId>> {
        match self.b()? {
            0 => Ok(None),
            1 => Ok(Some(self.elem()?)),
            _ => Err("bad option tag".into()),
        }
    }
    /// A `NIL`-able index below `n`.
    fn nix(&mut self, n: usize) -> Res<u32> {
        match self.u()? {
            0 => Ok(NIL),
            v if v - 1 < n as u64 => Ok((v - 1) as u32),
            _ => Err("index out of range".into()),
        }
    }
    /// A plain index below `n`.
    fn ix(&mut self, n: usize) -> Res<u32> {
        let v = self.u()?;
        if v < n as u64 {
            Ok(v as u32)
        } else {
            Err("index out of range".into())
        }
    }
    fn ids(&mut self) -> Res<Vec<OpId>> {
        let n = self.len()?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.id()?);
        }
        Ok(v)
    }
    /// One epoch doc; `bytes_of(op)` = an integrated op's inserted bytes
    /// (from its log record).
    fn doc(&mut self, epoch: OpId, bytes_of: &dyn Fn(OpId, &mut Vec<u8>) -> bool) -> Res<TextDoc> {
        let n = self.len()?;
        let seed_at = self.p;
        self.bytes(n)?;
        let mut ops = vec![epoch];
        ops.extend(self.ids()?);
        // One copy of the seed, op bytes appended in place (open time, B2).
        let mut arena = Vec::with_capacity(n + 32 * ops.len());
        arena.extend_from_slice(&self.b[seed_at..seed_at + n]);
        let mut op_bytes = Vec::with_capacity(ops.len());
        op_bytes.push((0u64, n as u32));
        let mut op_ix = crate::text::FxMap::default();
        op_ix.reserve(ops.len());
        op_ix.insert(epoch, 0u32);
        for (i, o) in ops.iter().enumerate().skip(1) {
            let start = arena.len();
            if !bytes_of(*o, &mut arena) {
                return Err("doc op not in the log".into());
            }
            op_bytes.push((start as u64, (arena.len() - start) as u32));
            if op_ix.insert(*o, i as u32).is_some() {
                return Err("duplicate doc op".into());
            }
        }
        let nops = ops.len();
        // Indices are checked against counts read later, so read raw first.
        let mut op_first_raw = Vec::with_capacity(nops);
        for _ in 0..nops {
            op_first_raw.push(self.u()?);
        }
        let nsplit = self.len()?;
        let mut split_raw = Vec::with_capacity(nsplit);
        for _ in 0..nsplit {
            let ix = self.ix(nops)?;
            let k = self.len()?;
            let mut v = Vec::with_capacity(k);
            for _ in 0..k {
                v.push((self.u32()?, self.u()?));
            }
            split_raw.push((ix, v));
        }
        let nf = self.len()?;
        let mut frags_raw = Vec::with_capacity(nf);
        for _ in 0..nf {
            let op = self.ix(nops)?;
            let off = self.u32()?;
            let len = self.u32()?;
            let links = [self.u()?, self.u()?, self.u()?, self.u()?, self.u()?];
            let ro_op = self.nix(nops)?;
            let ro_off = self.u32()?;
            let del = self.u32()?;
            let side = match self.b()? {
                0 => Side::L,
                1 => Side::R,
                _ => return Err("bad side".into()),
            };
            frags_raw.push((op, off, len, links, ro_op, ro_off, del, side));
        }
        let nd = self.len()?;
        let mut delsets = vec![Vec::new()];
        for _ in 0..nd {
            let k = self.len()?;
            let mut v = Vec::with_capacity(k);
            for _ in 0..k {
                v.push(self.ix(nops)?);
            }
            delsets.push(v);
        }
        let nc = self.len()?;
        let mut chunks = Vec::with_capacity(nc);
        for _ in 0..nc {
            let k = self.len()?;
            let mut v = Vec::with_capacity(k);
            for _ in 0..k {
                v.push(self.ix(nf)?);
            }
            chunks.push(v);
        }
        let mut order = Vec::with_capacity(nc);
        for _ in 0..nc {
            order.push(self.ix(nc)?);
        }
        let mut rank = Vec::with_capacity(nc);
        for _ in 0..nc {
            rank.push(self.ix(nc)?);
        }
        let fix = |v: u64, n: usize| -> Res<u32> {
            match v {
                0 => Ok(NIL),
                v if v - 1 < n as u64 => Ok((v - 1) as u32),
                _ => Err("index out of range".into()),
            }
        };
        let op_first = op_first_raw
            .into_iter()
            .map(|v| fix(v, nf))
            .collect::<Res<Vec<u32>>>()?;
        let mut split = crate::text::FxMap::default();
        for (ix, v) in split_raw {
            let v = v
                .into_iter()
                .map(|(o, f)| Ok((o, fix(f, nf)?)))
                .collect::<Res<Vec<(u32, u32)>>>()?;
            split.insert(ix, v);
        }
        let ndel = delsets.len();
        let mut frags = Vec::with_capacity(nf);
        for (op, off, len, l, ro_op, ro_off, del, side) in frags_raw {
            let end = off as u64 + len as u64;
            if end > op_bytes[op as usize].1 as u64 {
                return Err("fragment outside its op".into());
            }
            if del & crate::text::ONE != 0 {
                if (del & !crate::text::ONE) as usize >= nops {
                    return Err("bad deleter".into());
                }
            } else if del as usize >= ndel {
                return Err("bad deleter set".into());
            }
            frags.push(Frag {
                op,
                off,
                len,
                parent: fix(l[0], nf)?,
                chunk: fix(l[1], nc)?,
                first_l: fix(l[2], nf)?,
                first_r: fix(l[3], nf)?,
                next: fix(l[4], nf)?,
                ro_op,
                ro_off,
                del,
                side,
            });
        }
        if frags.is_empty() || chunks.is_empty() {
            return Err("empty doc".into());
        }
        let delset_ix = delsets
            .iter()
            .enumerate()
            .map(|(i, v)| (v.clone(), i as u32))
            .collect();
        Ok(TextDoc {
            epoch,
            ops,
            op_ix,
            op_bytes,
            arena,
            op_first,
            split,
            frags,
            delsets,
            delset_ix,
            chunks,
            order,
            rank,
        })
    }
    fn text(&mut self, log: &[(Op, Undo, Outcome)]) -> Res<TextPart> {
        // One index over the log's text ops (a binary search per doc op
        // costs a cache miss per step at 1M ops, B2).
        let mut by_id: crate::text::FxMap<OpId, &TextEdit> = crate::text::FxMap::default();
        for (o, _, _) in log {
            if let Payload::TextEdit(t) = &o.payload {
                by_id.insert(o.id, t);
            }
        }
        let bytes_of = |id: OpId, out: &mut Vec<u8>| -> bool {
            match by_id.get(&id) {
                Some(t) => {
                    for x in &t.ins {
                        out.extend_from_slice(&x.bytes);
                    }
                    true
                }
                None => false,
            }
        };
        let mut t = TextPart {
            any: true,
            ..TextPart::default()
        };
        let nd = self.len()?;
        for _ in 0..nd {
            let node = self.id()?;
            let epoch = self.id()?;
            let slot = match self.b()? {
                0 => DocSlot::Deferred,
                1 => DocSlot::Doc(Box::new(self.doc(epoch, &bytes_of)?)),
                _ => return Err("bad doc tag".into()),
            };
            t.docs.insert((node, epoch), slot);
        }
        t.valid = self.ids()?.into_iter().collect();
        t.invalid = self.ids()?.into_iter().collect();
        let n = self.len()?;
        for _ in 0..n {
            let node = self.id()?;
            let e = self.id()?;
            let b = self.blob()?;
            t.content.insert(node, (e, b));
        }
        let n = self.len()?;
        for _ in 0..n {
            let node = self.id()?;
            let k = self.len()?;
            let mut v = Vec::with_capacity(k);
            for _ in 0..k {
                v.push(Conflict::Content {
                    node,
                    winner: self.id()?,
                    loser: self.id()?,
                    winner_blob: self.blob()?,
                    loser_blob: self.blob()?,
                });
            }
            t.conflicts.insert(node, v);
        }
        let n = self.len()?;
        for _ in 0..n {
            let node = self.id()?;
            let k = self.len()?;
            let mut v = Vec::with_capacity(k);
            for _ in 0..k {
                let ops = self.ids()?;
                let lines = (self.u32()?, self.u32()?);
                v.push(Notice::ConcurrentEdit { node, ops, lines });
            }
            t.notices.insert(node, v);
        }
        let n = self.len()?;
        for _ in 0..n {
            let node = self.id()?;
            t.fold.insert(node, self.oblob()?);
        }
        Ok(t)
    }
}

const MAGIC: &[u8] = b"RSNAP1";
/// Issue #54: a replica with text ops (snapshot v2 payload).
const MAGIC2: &[u8] = b"RSNAP2";

impl Replica {
    /// Encode this replica (Issue #33). `None` if ops are pending (a
    /// snapshot only covers a closed, fully applied op set).
    pub fn encode_snapshot(&self) -> Option<Vec<u8>> {
        if !self.pending.is_empty() {
            return None;
        }
        let mut w = W {
            buf: Vec::with_capacity(self.log.len() * 96 + 1024),
            reps: HashMap::new(),
            table: Vec::new(),
        };
        w.state(&self.work.state);
        w.u(self.work.setter.len() as u64);
        for (n, s) in &self.work.setter {
            w.id(*n);
            w.id(*s);
        }
        w.u(self.log.len() as u64);
        for (o, u, out) in &self.log {
            w.op(o);
            match u {
                Undo::None => w.b(0),
                Undo::RemoveNode(n) => {
                    w.b(1);
                    w.id(*n)
                }
                Undo::Restore {
                    node,
                    parent,
                    name,
                    setter,
                } => {
                    w.b(2);
                    w.id(*node);
                    w.oid(*parent);
                    w.s(name);
                    w.id(*setter);
                }
                Undo::Content { node, prev } => {
                    w.b(3);
                    w.id(*node);
                    w.oblob(prev);
                }
            }
            match out {
                Outcome::Applied => w.b(0),
                Outcome::Invalid => w.b(1),
                Outcome::Skipped { winner } => {
                    w.b(2);
                    w.id(*winner)
                }
            }
        }
        match &self.view {
            None => w.b(0),
            Some(v) => {
                w.b(1);
                w.state(v)
            }
        }
        w.u(self.revived.len() as u64);
        for (a, b, c) in &self.revived {
            w.id(*a);
            w.id(*b);
            w.id(*c);
        }
        let text = !self.text.is_empty();
        if text {
            w.text(&self.text);
        }
        let mut out = Vec::with_capacity(w.buf.len() + 16 * w.table.len() + 16);
        out.extend_from_slice(if text { MAGIC2 } else { MAGIC });
        let mut head = W {
            buf: Vec::new(),
            reps: HashMap::new(),
            table: Vec::new(),
        };
        head.u(w.table.len() as u64);
        out.extend_from_slice(&head.buf);
        for r in &w.table {
            out.extend_from_slice(&r.to_le_bytes());
        }
        out.extend_from_slice(&w.buf);
        Some(out)
    }

    /// Decode [`Replica::encode_snapshot`] output. Any malformed input,
    /// trailing bytes or an unsorted/duplicated op log is an error.
    pub fn decode_snapshot(bytes: &[u8]) -> Result<Replica, String> {
        let (body, text) = match bytes.strip_prefix(MAGIC) {
            Some(b) => (b, false),
            None => (
                bytes
                    .strip_prefix(MAGIC2)
                    .ok_or("bad replica snapshot magic")?,
                true,
            ),
        };
        let mut r = R {
            b: body,
            p: 0,
            table: Vec::new(),
        };
        let nt = r.len()?;
        for _ in 0..nt {
            let mut a = [0u8; 16];
            a.copy_from_slice(r.bytes(16)?);
            r.table.push(u128::from_le_bytes(a));
        }
        let state = r.state()?;
        let ns = r.len()?;
        let mut setter = Vec::with_capacity(ns);
        for _ in 0..ns {
            let n = r.id()?;
            let s = r.id()?;
            if setter.last().is_some_and(|(l, _): &(NodeId, OpId)| *l >= n) {
                return Err("setter not sorted".into());
            }
            setter.push((n, s));
        }
        let setter: BTreeMap<NodeId, OpId> = setter.into_iter().collect();
        let nl = r.len()?;
        let mut log = Vec::with_capacity(nl);
        let mut known = Vec::with_capacity(nl);
        for _ in 0..nl {
            let o = r.op()?;
            let u = match r.b()? {
                0 => Undo::None,
                1 => Undo::RemoveNode(r.id()?),
                2 => Undo::Restore {
                    node: r.id()?,
                    parent: r.oid()?,
                    name: r.s()?,
                    setter: r.id()?,
                },
                3 => Undo::Content {
                    node: r.id()?,
                    prev: r.oblob()?,
                },
                _ => return Err("bad undo tag".into()),
            };
            let out = match r.b()? {
                0 => Outcome::Applied,
                1 => Outcome::Invalid,
                2 => Outcome::Skipped { winner: r.id()? },
                _ => return Err("bad outcome tag".into()),
            };
            if known.last().is_some_and(|l| *l >= o.id) {
                return Err("op log not in < order".into());
            }
            known.push(o.id);
            log.push((o, u, out));
        }
        let view = match r.b()? {
            0 => None,
            1 => Some(r.state()?),
            _ => return Err("bad option tag".into()),
        };
        let nr = r.len()?;
        let mut revived = BTreeSet::new();
        for _ in 0..nr {
            revived.insert((r.id()?, r.id()?, r.id()?));
        }
        let text = if text {
            let x = r.text(&log)?;
            x
        } else {
            if log
                .iter()
                .any(|(o, _, _)| matches!(o.payload, Payload::TextEdit(_)))
            {
                return Err("text ops in a v1 snapshot".into());
            }
            TextPart::default()
        };
        if r.p != body.len() {
            return Err("trailing bytes".into());
        }
        Ok(Replica {
            work: Work { state, setter },
            log,
            known: known.into_iter().collect(),
            pending: BTreeMap::new(),
            waiting: HashMap::new(),
            missing: HashMap::new(),
            view,
            revived,
            text,
        })
    }
}
