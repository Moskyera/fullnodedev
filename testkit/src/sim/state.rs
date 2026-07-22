use basis::component::MemMap;
use basis::interface::{DiskDB, State};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use crate::sim::disk::{VerifiedMemDiskDB, VerifiedMemDiskSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateBackendKind {
    IndependentMem,
    ProductionStateInst,
}

pub struct TxStateFork {
    pub state: Box<dyn State>,
    _parent: Arc<Box<dyn State>>,
}

pub struct ChainState {
    kind: StateBackendKind,
    state: Box<dyn State>,
    disk: Option<Arc<dyn DiskDB>>,
    verified_disk: Option<Arc<VerifiedMemDiskDB>>,
}

pub struct ChainStateSnapshot {
    state: Box<dyn State>,
    verified_disk: Option<VerifiedMemDiskSnapshot>,
}

impl ChainState {
    pub fn new(kind: StateBackendKind) -> Self {
        match kind {
            StateBackendKind::IndependentMem => Self::independent_mem(),
            StateBackendKind::ProductionStateInst => Self::production_state_inst_verified_disk(),
        }
    }

    pub fn independent_mem() -> Self {
        Self {
            kind: StateBackendKind::IndependentMem,
            state: Box::new(ForkableMemState::default()),
            disk: None,
            verified_disk: None,
        }
    }

    pub fn production_state_inst_verified_disk() -> Self {
        let disk = Arc::new(VerifiedMemDiskDB::new());
        Self {
            kind: StateBackendKind::ProductionStateInst,
            state: Box::new(chain::StateInst::build(disk.clone(), None)),
            disk: Some(disk.clone()),
            verified_disk: Some(disk),
        }
    }

    pub fn production_state_inst(disk: Arc<dyn DiskDB>) -> Self {
        Self {
            kind: StateBackendKind::ProductionStateInst,
            state: Box::new(chain::StateInst::build(disk.clone(), None)),
            disk: Some(disk),
            verified_disk: None,
        }
    }

    pub fn kind(&self) -> StateBackendKind {
        self.kind
    }

    pub fn disk(&self) -> Option<Arc<dyn DiskDB>> {
        self.disk.clone()
    }

    pub fn disk_entry_count(&self) -> Option<usize> {
        let disk = self.disk.as_ref()?;
        Some(disk.entry_count().expect("testkit disk entry_count failed"))
    }

    pub fn disk_entries(&self) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let disk = self.disk.as_ref()?;
        Some(
            disk.dump_entries()
                .expect("testkit disk dump_entries failed"),
        )
    }

    pub fn effective_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.state
            .effective_entries()
            .expect("testkit state effective_entries failed")
    }

    pub fn state(&self) -> &dyn State {
        self.state.as_ref()
    }

    pub fn clone_state(&self) -> Box<dyn State> {
        self.state.clone_state()
    }

    pub fn fork_for_tx(&self) -> TxStateFork {
        let parent: Arc<Box<dyn State>> = self.state.clone_state().into();
        let state = parent.fork_sub(Arc::downgrade(&parent));
        TxStateFork {
            state,
            _parent: parent,
        }
    }

    pub fn commit(&mut self, state: Box<dyn State>) {
        self.state.merge_sub(state);
        if self.kind == StateBackendKind::ProductionStateInst {
            self.state.write_to_disk();
            if let Some(disk) = self.disk.clone() {
                disk.flush().expect("testkit disk flush failed");
                self.state = Box::new(chain::StateInst::build(disk, None));
            }
        }
    }

    pub fn snapshot(&self) -> ChainStateSnapshot {
        ChainStateSnapshot {
            state: self.state.clone_state(),
            verified_disk: self.verified_disk.as_ref().map(|disk| disk.snapshot()),
        }
    }

    pub fn restore(&mut self, snap: ChainStateSnapshot) {
        if let (Some(disk), Some(disk_snap)) = (&self.verified_disk, &snap.verified_disk) {
            disk.restore(disk_snap);
        }
        self.state = snap.state;
    }
}

#[derive(Default, Clone)]
pub struct ForkableMemState {
    parent: Weak<Box<dyn State>>,
    mem: MemMap,
}

impl ForkableMemState {
    pub fn from_mem(mem: MemMap) -> Self {
        Self {
            parent: Weak::<Box<dyn State>>::new(),
            mem,
        }
    }
}

impl State for ForkableMemState {
    fn fork_sub(&self, p: Weak<Box<dyn State>>) -> Box<dyn State> {
        Box::new(Self {
            parent: p,
            mem: MemMap::default(),
        })
    }

    fn merge_sub(&mut self, sta: Box<dyn State>) {
        self.mem.extend(sta.as_mem().clone());
    }

    fn detach(&mut self) {
        self.parent = Weak::<Box<dyn State>>::new();
    }

    fn clone_state(&self) -> Box<dyn State> {
        Box::new(self.clone())
    }

    fn as_mem(&self) -> &MemMap {
        &self.mem
    }

    fn overlay_entries(&self) -> sys::Ret<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        let mut entries: Vec<_> = self
            .mem
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    fn effective_entries(&self) -> sys::Ret<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        if let Some(parent) = self.parent.upgrade() {
            for (key, value) in parent.effective_entries()? {
                out.insert(key, value);
            }
        }
        for (key, value) in self.mem.iter() {
            match value {
                Some(value) => {
                    out.insert(key.clone(), value.clone());
                }
                None => {
                    out.remove(key);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    fn get(&self, k: Vec<u8>) -> Option<Vec<u8>> {
        if let Some(v) = self.mem.get(&k) {
            return v.clone();
        }
        if let Some(parent) = self.parent.upgrade() {
            return parent.get(k);
        }
        None
    }

    fn set(&mut self, k: Vec<u8>, v: Vec<u8>) {
        self.mem.insert(k, Some(v));
    }

    fn del(&mut self, k: Vec<u8>) {
        self.mem.insert(k, None);
    }
}

#[derive(Default, Clone)]
pub struct FlatMemState {
    mem: MemMap,
}

impl FlatMemState {
    /// Construct a `FlatMemState` from an existing `MemMap` (used by `MemChain`
    /// to rebuild the persistent state after a forked tx).
    pub fn from_mem(mem: MemMap) -> Self {
        Self { mem }
    }
}

impl State for FlatMemState {
    fn fork_sub(&self, _: Weak<Box<dyn State>>) -> Box<dyn State> {
        Box::new(Self {
            mem: MemMap::default(),
        })
    }

    fn merge_sub(&mut self, sta: Box<dyn State>) {
        self.mem.extend(sta.as_mem().clone());
    }

    fn detach(&mut self) {}

    fn clone_state(&self) -> Box<dyn State> {
        Box::new(self.clone())
    }

    fn as_mem(&self) -> &MemMap {
        &self.mem
    }

    fn overlay_entries(&self) -> sys::Ret<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        let mut entries: Vec<_> = self
            .mem
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    fn effective_entries(&self) -> sys::Ret<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut entries = Vec::new();
        for (key, value) in self.overlay_entries()? {
            if let Some(value) = value {
                entries.push((key, value));
            }
        }
        Ok(entries)
    }

    fn get(&self, k: Vec<u8>) -> Option<Vec<u8>> {
        self.mem.get(&k).and_then(|v| v.clone())
    }

    fn set(&mut self, k: Vec<u8>, v: Vec<u8>) {
        self.mem.insert(k, Some(v));
    }

    fn del(&mut self, k: Vec<u8>) {
        self.mem.insert(k, None);
    }
}
