
pub struct StateInst {
    disk: Arc<dyn DiskDB>,
    parent: Weak<Box<dyn State>>,
    mem: MemKV,
}

impl StateInst {
    pub fn build(d: Arc<dyn DiskDB>, p: Option<Arc<Box<dyn State>>>) -> Self {
        let p = match p {
            Some(p) => Arc::downgrade(&p),
            _ => Weak::new()
        };
        Self { disk: d, parent: p, mem: MemKV::new() }
    }
}

impl State for StateInst {
    fn disk(&self) -> Arc<dyn DiskDB> { self.disk.clone() }

    fn fork_sub(&self, p: Weak<Box<dyn State>>) -> Box<dyn State> {
        Box::new(Self { disk: self.disk.clone(), mem: MemKV::new(), parent: p })
    }

    fn merge_sub(&mut self, sta: Box<dyn State>) {
        self.mem.memry.extend(sta.as_mem().clone())
    }

    fn detach(&mut self) { self.parent = Weak::<Box<dyn State>>::new(); }

    fn clone_state(&self) -> Box<dyn State> {
        Box::new(Self { disk: self.disk.clone(), parent: self.parent.clone(), mem: self.mem.clone() })
    }

    fn as_mem(&self) -> &MemMap { &self.mem.memry }

    fn overlay_entries(&self) -> Ret<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        let mut entries: Vec<_> = self
            .mem
            .memry
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    fn effective_entries(&self) -> Ret<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        if let Some(parent) = self.parent.upgrade() {
            for (key, value) in parent.effective_entries()? {
                out.insert(key, value);
            }
        } else {
            for (key, value) in self.disk.dump_entries()? {
                out.insert(key, value);
            }
        }
        for (key, value) in self.mem.memry.iter() {
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

    fn write_to_disk(&self) { self.disk.write(&self.mem); }

    fn get(&self, k: Vec<u8>) -> Option<Vec<u8>> {
        if let Some(v) = self.mem.get(&k) { return v.clone(); }
        if let Some(parent) = self.parent.upgrade() { return parent.get(k); }
        self.disk.read(&k)
    }

    fn set(&mut self, k: Vec<u8>, v: Vec<u8>) { self.mem.put(k, v) }

    fn del(&mut self, k: Vec<u8>) { self.mem.del(k) }
}

