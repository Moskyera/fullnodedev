use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use basis::interface::{DiskDB, MemDB};
use sys::Ret;

#[derive(Default)]
pub struct MemDiskDB {
    data: RwLock<HashMap<Vec<u8>, Vec<u8>>>,
}

#[derive(Default)]
pub struct BTreeMemDiskDB {
    data: RwLock<BTreeMap<Vec<u8>, Vec<u8>>>,
}

#[derive(Default)]
pub struct VerifiedMemDiskDB {
    primary: MemDiskDB,
    mirror: BTreeMemDiskDB,
}

#[derive(Clone)]
pub struct VerifiedMemDiskSnapshot {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl MemDiskDB {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.data.read().unwrap().len()
    }

    fn snapshot(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.data
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn restore(&self, snap: &BTreeMap<Vec<u8>, Vec<u8>>) {
        let mut data = self.data.write().unwrap();
        data.clear();
        data.extend(snap.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
}

impl DiskDB for MemDiskDB {
    fn read(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.data.read().unwrap().get(k).cloned()
    }

    fn save(&self, k: &[u8], v: &[u8]) {
        self.data.write().unwrap().insert(k.to_vec(), v.to_vec());
    }

    fn remove(&self, k: &[u8]) {
        self.data.write().unwrap().remove(k);
    }

    fn write(&self, mem: &dyn MemDB) {
        let mut data = self.data.write().unwrap();
        mem.for_each(&mut |k, v| match v {
            Some(v) => {
                data.insert(k.to_vec(), v.to_vec());
            }
            None => {
                data.remove(k);
            }
        });
    }

    fn for_each(&self, each: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> Ret<()> {
        for (k, v) in self.data.read().unwrap().iter() {
            if !each(k, v) {
                break;
            }
        }
        Ok(())
    }
}

impl BTreeMemDiskDB {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.data.read().unwrap().len()
    }

    fn snapshot(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.data.read().unwrap().clone()
    }

    fn restore(&self, snap: &BTreeMap<Vec<u8>, Vec<u8>>) {
        *self.data.write().unwrap() = snap.clone();
    }
}

impl DiskDB for BTreeMemDiskDB {
    fn read(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.data.read().unwrap().get(k).cloned()
    }

    fn save(&self, k: &[u8], v: &[u8]) {
        self.data.write().unwrap().insert(k.to_vec(), v.to_vec());
    }

    fn remove(&self, k: &[u8]) {
        self.data.write().unwrap().remove(k);
    }

    fn write(&self, mem: &dyn MemDB) {
        let mut data = self.data.write().unwrap();
        mem.for_each(&mut |k, v| match v {
            Some(v) => {
                data.insert(k.to_vec(), v.to_vec());
            }
            None => {
                data.remove(k);
            }
        });
    }

    fn for_each(&self, each: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> Ret<()> {
        for (k, v) in self.data.read().unwrap().iter() {
            if !each(k, v) {
                break;
            }
        }
        Ok(())
    }
}

impl VerifiedMemDiskDB {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        let len = self.primary.len();
        assert_eq!(
            len,
            self.mirror.len(),
            "verified disk length mismatch: primary={}, mirror={}",
            len,
            self.mirror.len()
        );
        len
    }

    pub fn snapshot(&self) -> VerifiedMemDiskSnapshot {
        self.assert_consistent("snapshot");
        VerifiedMemDiskSnapshot {
            data: self.primary.snapshot(),
        }
    }

    pub fn restore(&self, snap: &VerifiedMemDiskSnapshot) {
        self.primary.restore(&snap.data);
        self.mirror.restore(&snap.data);
        self.assert_consistent("restore");
    }

    fn assert_consistent(&self, op: &str) {
        let primary = self.primary.snapshot();
        let mirror = self.mirror.snapshot();
        assert_eq!(
            primary,
            mirror,
            "verified disk mismatch after {op}: primary_len={}, mirror_len={}",
            primary.len(),
            mirror.len()
        );
    }
}

impl DiskDB for VerifiedMemDiskDB {
    fn read(&self, k: &[u8]) -> Option<Vec<u8>> {
        let primary = self.primary.read(k);
        let mirror = self.mirror.read(k);
        assert_eq!(
            primary,
            mirror,
            "verified disk read mismatch for key 0x{}",
            hex::encode(k)
        );
        primary
    }

    fn save(&self, k: &[u8], v: &[u8]) {
        self.primary.save(k, v);
        self.mirror.save(k, v);
        self.assert_consistent("save");
    }

    fn remove(&self, k: &[u8]) {
        self.primary.remove(k);
        self.mirror.remove(k);
        self.assert_consistent("remove");
    }

    fn write(&self, mem: &dyn MemDB) {
        self.primary.write(mem);
        self.mirror.write(mem);
        self.assert_consistent("write");
    }

    fn for_each(&self, each: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> Ret<()> {
        self.assert_consistent("for_each");
        self.mirror.for_each(each)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::component::MemKV;

    #[test]
    fn verified_disk_cross_checks_save_read_remove_and_batch_write() {
        let disk = VerifiedMemDiskDB::new();

        disk.save(b"a", b"one");
        assert_eq!(disk.read(b"a"), Some(b"one".to_vec()));

        let mut batch = MemKV::new();
        batch.put(b"b".to_vec(), b"two".to_vec());
        batch.del(b"a".to_vec());
        disk.write(&batch);

        assert_eq!(disk.read(b"a"), None);
        assert_eq!(disk.read(b"b"), Some(b"two".to_vec()));
        assert_eq!(disk.len(), 1);

        let snap = disk.snapshot();
        disk.save(b"c", b"three");
        assert_eq!(disk.len(), 2);
        disk.restore(&snap);
        assert_eq!(disk.read(b"c"), None);
        assert_eq!(disk.read(b"b"), Some(b"two".to_vec()));
    }
}
