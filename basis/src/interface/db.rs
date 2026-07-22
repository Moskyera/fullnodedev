

pub trait MemDB : Send + Sync {
    fn new() -> Self where Self: Sized { never!() }
    fn len(&self) -> usize { 0 }
    fn del(&mut self, _: Vec<u8>) {}
    fn put(&mut self, _: Vec<u8>, _: Vec<u8>) {}
    fn get(&self, _: &Vec<u8>) -> Option<&Option<Vec<u8>>> { None }
    fn for_each(&self, _:&mut dyn FnMut(&[u8], Option<&[u8]>)) {}
}


pub trait MemBatch {
    fn new() -> Self where Self: Sized { never!() }
    fn from_memkv(_: &dyn MemDB) -> Self where Self: Sized { never!() }
    fn del(&mut self, _: &[u8]) {}
    fn put(&mut self, _: &[u8], _: &[u8]) {}
}


pub trait DiskDB : Send + Sync {
    // fn open(dir: &Path) -> Self where Self: Sized;
    fn read(&self, _: &[u8]) -> Option<Vec<u8>> { None }
    fn save(&self, _: &[u8], _: &[u8] ) {}
    fn remove(&self, _: &[u8]) {}
    fn write(&self, _: &dyn MemDB) {} // dyn MemDB
    // fn write_batch(&self, _: Box<dyn Any>) {} // dyn MemBatch
    fn flush(&self) -> Ret<()> { Ok(()) }
    fn entry_count(&self) -> Ret<usize> {
        let mut count = 0usize;
        self.for_each(&mut |_, _| {
            count += 1;
            true
        })?;
        Ok(count)
    }
    fn dump_entries(&self) -> Ret<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut entries = Vec::new();
        self.for_each(&mut |key, value| {
            entries.push((key.to_vec(), value.to_vec()));
            true
        })?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }
    // debug
    fn for_each(&self, _: &mut dyn FnMut(&[u8], &[u8])->bool) -> Ret<()>;
}


