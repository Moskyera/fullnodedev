use std::path::*;
use std::sync::*;
use std::thread::{JoinHandle, spawn};

use basis::config::*;
use basis::interface::*;
use sys::*;

/***************************************/

pub type FctDiskDb = Arc<dyn Fn(&PathBuf) -> Box<dyn DiskDB> + Send + Sync>;
pub type FctTxPool = Arc<dyn Fn(&EngineConf) -> Ret<Box<dyn TxPool>> + Send + Sync>;
pub type FctMinter = Arc<dyn Fn(&IniObj) -> Ret<Box<dyn Minter>> + Send + Sync>;
pub type FctEngine = Arc<
    dyn Fn(FctDiskDb, Arc<EngineConf>, Arc<dyn Minter>, Arc<dyn Scaner>) -> Ret<Box<dyn Engine>>
        + Send
        + Sync,
>;
pub type FctHNoder =
    Arc<dyn Fn(&IniObj, Arc<dyn TxPool>, Arc<dyn Engine>) -> Ret<Box<dyn HNoder>> + Send + Sync>;
pub type FctServer = Arc<dyn Fn(&IniObj, Arc<dyn HNoder>) -> Ret<Box<dyn Server>> + Send + Sync>;
pub type FctExtendApp = Arc<dyn Fn(Worker, Arc<dyn HNoder>) + Send + Sync>;
pub type ScanerReadyHook = Arc<dyn Fn(&dyn Scaner, &IniObj) -> Rerr + Send + Sync>;

/***************************************/

pub struct NilScaner {}
impl Scaner for NilScaner {}

/***************************************/

pub struct FullnodeRuntime {
    exiter: Exiter,
    minter: Arc<dyn Minter>,
    scaner: Arc<dyn Scaner>,
    hnoder: Arc<dyn HNoder>,
    server: Arc<dyn Server>,
    extapp: Vec<FctExtendApp>,
}

impl FullnodeRuntime {
    pub fn run(self) -> Rerr {
        let exiter = self.exiter.clone();
        let mut tasks: Vec<JoinHandle<()>> = vec![];
        let mut spawn_task = |run: Box<dyn FnOnce(Worker) + Send>| {
            let worker = exiter.worker();
            tasks.push(spawn(move || run(worker)));
        };

        let minter = self.minter.clone();
        spawn_task(Box::new(move |worker| minter.start(worker)));
        let server = self.server.clone();
        spawn_task(Box::new(move |worker| server.start(worker)));
        let scanr1 = self.scaner.clone();
        spawn_task(Box::new(move |worker| scanr1.start(worker)));
        let scanr2 = self.scaner.clone();
        spawn_task(Box::new(move |worker| scanr2.serve(worker)));
        let hnoder = self.hnoder.clone();
        spawn_task(Box::new(move |worker| hnoder.start(worker)));
        for app in self.extapp {
            let hnoder = self.hnoder.clone();
            spawn_task(Box::new(move |worker| app(worker, hnoder)));
        }

        // wait ctrl+c (or other exit signal), then start active shutdown.
        if exiter.wait_exit_or_done() {
            self.hnoder.exit();
        }
        exiter.wait();

        let mut panic_count = 0;
        for handle in tasks {
            if handle.join().is_err() {
                panic_count += 1;
            }
        }
        if panic_count > 0 {
            return errf!("{} thread panicked", panic_count);
        }
        wlogln!("[Exit] Hacash fullnode closed.");
        Ok(())
    }
}

/***************************************/

pub struct FullnodeBuilder {
    pub exiter: Exiter,
    cnfini: IniObj,
    datdir: PathBuf,
    engcnf: Arc<EngineConf>,
    nodcnf: Arc<NodeConf>,
    install_ctrlc: bool,
    diskdb: Option<FctDiskDb>,
    txpool: Option<FctTxPool>,
    minter: Option<FctMinter>,
    engine: Option<FctEngine>,
    hnoder: Option<FctHNoder>,
    server: Option<FctServer>,
    scaner: Option<Box<dyn Scaner>>,
    extapp: Vec<FctExtendApp>,
}

impl FullnodeBuilder {
    pub fn from_config_path(inipath: &str) -> Ret<Self> {
        let cnfini = load_config(inipath.to_owned());
        if cnfini.is_empty() {
            return errf!("config '{}' is empty or failed to load", inipath);
        }
        Ok(Self::from_ini(cnfini))
    }

    pub fn from_ini(cnfini: IniObj) -> Self {
        let datdir = get_mainnet_data_dir(&cnfini);
        let engcnf = Arc::new(EngineConf::new(&cnfini));
        let nodcnf = Arc::new(NodeConf::new(&cnfini));
        Self {
            exiter: Exiter::new(),
            cnfini,
            datdir,
            engcnf,
            nodcnf,
            install_ctrlc: false,
            diskdb: None,
            txpool: None,
            minter: None,
            engine: None,
            hnoder: None,
            server: None,
            scaner: None,
            extapp: vec![],
        }
    }

    pub fn install_ctrlc(&mut self, enable: bool) -> &mut Self {
        self.install_ctrlc = enable;
        self
    }

    pub fn ini(&self) -> &IniObj {
        &self.cnfini
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.datdir
    }

    pub fn engine_conf(&self) -> Arc<EngineConf> {
        self.engcnf.clone()
    }

    pub fn node_conf(&self) -> Arc<NodeConf> {
        self.nodcnf.clone()
    }

    pub fn diskdb<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&PathBuf) -> Box<dyn DiskDB> + Send + Sync + 'static,
    {
        self.diskdb = Some(Arc::new(f));
        self
    }

    pub fn txpool<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&EngineConf) -> Ret<Box<dyn TxPool>> + Send + Sync + 'static,
    {
        self.txpool = Some(Arc::new(f));
        self
    }

    pub fn minter<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&IniObj) -> Ret<Box<dyn Minter>> + Send + Sync + 'static,
    {
        self.minter = Some(Arc::new(f));
        self
    }

    pub fn scaner(&mut self, scn: Box<dyn Scaner>) -> &mut Self {
        self.scaner = Some(scn);
        self
    }

    pub fn engine<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(FctDiskDb, Arc<EngineConf>, Arc<dyn Minter>, Arc<dyn Scaner>) -> Ret<Box<dyn Engine>>
            + Send
            + Sync
            + 'static,
    {
        self.engine = Some(Arc::new(f));
        self
    }

    pub fn hnoder<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&IniObj, Arc<dyn TxPool>, Arc<dyn Engine>) -> Ret<Box<dyn HNoder>>
            + Send
            + Sync
            + 'static,
    {
        self.hnoder = Some(Arc::new(f));
        self
    }

    pub fn server<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&IniObj, Arc<dyn HNoder>) -> Ret<Box<dyn Server>> + Send + Sync + 'static,
    {
        self.server = Some(Arc::new(f));
        self
    }

    pub fn app<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(Worker, Arc<dyn HNoder>) + Send + Sync + 'static,
    {
        self.extapp.push(Arc::new(f));
        self
    }

    pub fn build(mut self) -> Ret<FullnodeRuntime> {
        if self.install_ctrlc {
            let exiter = self.exiter.clone();
            ctrlc::set_handler(move || {
                exiter.exit();
            })
            .map_err(|e| format!("failed to install ctrlc handler: {}", e))?;
        }

        let errn = |name| format!("fullnode builder missing {}", name);

        let diskdb = self.diskdb.take().ok_or(errn("diskdb"))?;
        let txpool = self.txpool.take().ok_or(errn("txpool"))?;
        let minter = self.minter.take().ok_or(errn("minter"))?;
        let engine = self.engine.take().ok_or(errn("engine"))?;
        let hnoder = self.hnoder.take().ok_or(errn("hnoder"))?;
        let server = self.server.take().ok_or(errn("server"))?;
        let mut scaner = self.scaner.take().ok_or(errn("scaner"))?;

        scaner.init(&self.cnfini)?;

        let errb = |name, e| format!("build {} failed: {}", name, e);
        let scaner: Arc<dyn Scaner> = scaner.into();
        let txpool: Arc<dyn TxPool> = txpool(self.engcnf.as_ref())
            .map_err(|e| errb("txpool", e))?
            .into();
        let minter: Arc<dyn Minter> = minter(&self.cnfini).map_err(|e| errb("minter", e))?.into();
        let engine: Arc<dyn Engine> =
            engine(diskdb, self.engcnf.clone(), minter.clone(), scaner.clone())
                .map_err(|e| errb("engine", e))?
                .into();
        minter.bind_engine(engine.clone());
        let hnoder: Arc<dyn HNoder> = hnoder(&self.cnfini, txpool, engine)
            .map_err(|e| errb("hnoder", e))?
            .into();
        let server: Arc<dyn Server> = server(&self.cnfini, hnoder.clone())
            .map_err(|e| errb("server", e))?
            .into();

        Ok(FullnodeRuntime {
            exiter: self.exiter,
            minter,
            scaner,
            hnoder,
            server,
            extapp: self.extapp,
        })
    }

    pub fn run(self) -> Rerr {
        self.build()?.run()
    }
}
