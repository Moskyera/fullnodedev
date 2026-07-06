

include!{"batch_sled.rs"}


pub struct DiskKV {
    ldb: sled::Db,
}


impl DiskKV {

    pub fn open(dir: &Path) -> Self {
        let mut cfg = sled::Config::new().path(dir);
        if db_sled_small_machine_enabled() {
            cfg = cfg
                .cache_capacity(32 * 1024 * 1024) // 32MB
                .mode(sled::Mode::LowSpace)
                .flush_every_ms(Some(1000));
        }
        Self { ldb: cfg.open().unwrap() }
    }

}


impl DiskDB for DiskKV {

    fn remove(&self, k: &[u8]) {
        self.ldb.remove(k).unwrap();
        if db_sync_enabled() {
            self.ldb.flush().unwrap();
        }
    }

    fn save(&self, k: &[u8], v: &[u8]) {
        self.ldb.insert(k, v).unwrap();
        if db_sync_enabled() {
            self.ldb.flush().unwrap();
        }
    }

    fn read(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.ldb.get(k).unwrap().map(|a|a.to_vec())
    }

    fn write(&self, memkv: &dyn MemDB) {
        let wb = Membatch::from_memkv(memkv);
        self.ldb.apply_batch(wb.into_batch().obj).unwrap(); // must
        if db_sync_enabled() {
            self.ldb.flush().unwrap();
        }
    }

    /*
    fn write_batch(&self, batch: Box<dyn Any>) {
        let wb = batch.downcast::<Membatch>().unwrap().into_batch();
        self.ldb.apply_batch(wb.obj).unwrap(); // must
        // self.ldb.flush().unwrap();
    }
    */

    fn flush(&self) -> Rerr {
        self.ldb.flush().map(|_| ()).map_err(|e| e.to_string())
    }

    fn for_each(&self, each: &mut dyn FnMut(&[u8], &[u8])->bool) -> Rerr {
        for item in self.ldb.iter() {
            let (k, v) = item.map_err(|e| e.to_string())?;
            if !each(k.as_ref(), v.as_ref()) {
                break
            }
        }
        Ok(())
    }

}
