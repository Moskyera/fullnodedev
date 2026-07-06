

include!{"batch_rocksdb.rs"}


pub struct DiskKV {
    rdb: rocksdb::DB,
}


impl DiskKV {

    pub fn open(dir: &Path) -> Self {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        Self { rdb: rocksdb::DB::open(&opts, dir).unwrap() }
    }

    fn write_options() -> rocksdb::WriteOptions {
        let mut opts = rocksdb::WriteOptions::default();
        opts.set_sync(db_sync_enabled());
        opts
    }

}


impl DiskDB for DiskKV {

    fn remove(&self, k: &[u8]) {
        let opts = Self::write_options();
        self.rdb.delete_opt(k, &opts).unwrap();
    }

    fn save(&self, k: &[u8], v: &[u8]) {
        let opts = Self::write_options();
        self.rdb.put_opt(k, v, &opts).unwrap();
    }

    fn read(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.rdb.get(k).unwrap().map(|a|a.to_vec())
    }

    fn write(&self, memkv: &dyn MemDB) {
        let wb = Membatch::from_memkv(memkv);
        let opts = Self::write_options();
        self.rdb.write_opt(wb.into_batch().obj, &opts).unwrap(); // must
    }

    /*
    fn write_batch(&self, batch: Box<dyn Any>) {
        let wb = batch.downcast::<Membatch>().unwrap().into_batch();
        self.ldb.apply_batch(wb.obj).unwrap(); // must
        // self.ldb.flush().unwrap();
    }
    */

    fn flush(&self) -> Rerr {
        self.rdb.flush().map_err(|e| e.to_string())
    }

    fn for_each(&self, each: &mut dyn FnMut(&[u8], &[u8])->bool) -> Rerr{
        let rdbiter = self.rdb.iterator(rocksdb::IteratorMode::Start);
        for item in rdbiter {
            let (k, v) = item.map_err(|e| e.to_string())?;
            if !each(k.as_ref(), v.as_ref()) {
                break
            }
        }
        Ok(())
    }

}

