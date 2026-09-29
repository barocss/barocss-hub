//! Binary snapshot of a [`Replica`] (Issue #33, Wiki Appendix A.2.3).
//!
//! Compact little-endian encoding of everything a replica holds after
//! replaying a closed op set: tree state, parent setters, the applied-op
//! log with undo records and outcomes, the D-014 view and revive triples.
//! Replica ids are interned in a table; counters and lengths are LEB128.
//! There is no checksum here: the caller frames and checksums the bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{BlobId, Kind, Node, NodeId, Op, OpId, Outcome, Payload, Replica, State, Undo, Work};

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
            _ => return Err("bad payload tag".into()),
        };
        Ok(Op { id, deps, payload })
    }
}

const MAGIC: &[u8] = b"RSNAP1";

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
        let mut out = Vec::with_capacity(w.buf.len() + 16 * w.table.len() + 16);
        out.extend_from_slice(MAGIC);
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
        let body = bytes
            .strip_prefix(MAGIC)
            .ok_or("bad replica snapshot magic")?;
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
        })
    }
}
