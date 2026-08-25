

#[derive(Clone)]
pub struct NodeConf {
    pub node_key: [u8; 16],
    pub node_name: String,
    pub listen: u16,
    pub find_nodes: bool,
    pub accept_nodes: bool,
    pub boot_nodes: Vec<SocketAddr>,
    pub offshoot_peers: usize, // private IP
    pub backbone_peers: usize, // public IP
    pub use_stable_nodes: bool,
    pub data_dir: PathBuf,
    
    pub multi_thread: bool,


}


/// Outbound connections to public peers, and the number that decides whether a
/// node behind a router can publish at all.
///
/// This was 4. On a node nobody can dial in to, those four are the entire
/// connection to Hacash, and four is not enough to get a transaction to a
/// miner. Measured on mainnet over two days: four correctly signed
/// transactions each left the socket, confirmed by the relay's own counter
/// (`4 peers considered, 4 selected, 4 sent, 0 failed`, no writer retired), and
/// not one reached a block. The official public node answered "transaction not
/// found" for every one of them. The identical bytes handed to a well connected
/// node came back accepted and were mined within two minutes.
///
/// Raising it to 32 got 10 peers from the network rather than 4, and the next
/// transaction propagated on its own, reaching the public node's pool in
/// seconds. Nothing else changed.
///
/// A node that accepts inbound connections does not need this: dialers make up
/// the difference. A node behind a router that will not forward a port has no
/// other lever, and that node cannot tell it is in trouble, because it syncs
/// blocks perfectly the whole time it is unable to speak.
pub const DEFAULT_BACKBONE_PEERS: u64 = 32;

impl NodeConf {

    
    pub fn new(ini: &IniObj) -> NodeConf {
        let sec = &ini_section(ini, "node");

        // node key
        let node_key = read_node_key(ini, &sec);

        // node name
        let nidhx = hex::encode(&node_key);
        let defnm: String = "hn".to_owned() + &nidhx[..8];
        let node_name = ini_must_maxlen(&sec, "name", &defnm, 16); // max len = 16
        // println!("node name = {}", node_name);

        // port
        let port = ini_must_u64(sec, "listen", 3337);
        if port<1001 || port>65535 {
            panic!("{}", exiterr!(1,"node listen port '{}' not supported", port))
        }
        // off_find
        let find = ini_must_bool(sec, "not_find_nodes", false) == false;
        let accept = ini_must_bool(sec, "not_accept_nodes", false) == false;
        let use_stable_nodes = ini_must_bool(sec, "use_stable_nodes", true);

        // boots
        let boots = ini_must(sec, "boots", "");
        let boots = boots.replace(" ", "");
        let mut ipts: Vec<SocketAddr> = Vec::new();
        if ! boots.is_empty() {
            let boots = boots.split(",");
            ipts = boots.map(
                |s|s.parse::<SocketAddr>().expect(&exiterr!(1,"boot node ip port '{}' not supported", &s))
            ).collect();
        }
        // println!("boot nodes: {:?}", ipts);

        // create config
        let mut cnf = NodeConf{
            node_key: node_key,
            node_name: node_name,
            listen: port as u16,
            find_nodes: find,
            accept_nodes: accept,
            boot_nodes: ipts,
            // connect peers
            offshoot_peers: 200,
            backbone_peers: DEFAULT_BACKBONE_PEERS as usize,
            use_stable_nodes: use_stable_nodes,
            data_dir: get_mainnet_data_dir(ini),
            multi_thread:  ini_must_bool(sec, "multi_thread", false),
        };

        cnf.offshoot_peers = ini_must_u64(sec, "offshoot_peers", 200) as usize;
        cnf.backbone_peers = ini_must_u64(sec, "backbone_peers", DEFAULT_BACKBONE_PEERS) as usize;

        // ok
        cnf
    }

}


/**
 * 
 */
fn read_node_key(ini: &IniObj, sec: &HashMap<String, Option<String>>) -> [u8; 16] {

    // node.id path
    let mut nidfp = get_mainnet_data_dir(ini);
    std::fs::create_dir_all(nidfp.clone()).unwrap();
    let kph =  std::path::absolute(nidfp.as_path());
    nidfp.push("node.id");
        
    // node id
    let mut node_key = [0u8; 16];
    let mut nidfile = OpenOptions::new()
        .read(true).write(true).create(true).open(nidfp)
        .expect("cannot open node info file.");
    // read
    let mut snid = String::new();
    nidfile.read_to_string(&mut snid).unwrap();
    // println!("read node id = {}", snid);
    if let Ok(nid) = hex::decode(&snid) {
        if nid.len() == 16 {
            node_key = nid.try_into().unwrap();
        }
    }
    if node_key[0] == 0 && node_key[15] == 0 {
        // get random node key
        let ndn = ini_must_maxlen(&sec, "name", "hx8888", 16); // max len = 16
        let sst = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let stuff = format!("{}-{}-{}", kph.unwrap().display(), ndn, sst);
        // println!("build node key by: {}", stuff);
        node_key = sys::sha2(&stuff)[0..16].try_into().unwrap();
        nidfile.write_all(hex::encode(&node_key).as_bytes()).unwrap();
    }
    // let nidhx = hex::encode(&node_key);
    // println!("node id = {}", nidhx);
    node_key
}

#[cfg(test)]
mod default_backbone_peers_tests {
    use super::*;

    /// The old value is the bug, so name it rather than only asserting the new
    /// one. A future edit that quietly restores 4 puts every node behind a
    /// router back into the state where it syncs perfectly and publishes
    /// nothing.
    #[test]
    fn the_default_is_not_the_four_that_stranded_four_transactions() {
        assert_ne!(DEFAULT_BACKBONE_PEERS, 4);
        assert!(
            DEFAULT_BACKBONE_PEERS >= 16,
            "a leaf node needs enough outbound peers to reach a miner; got {DEFAULT_BACKBONE_PEERS}"
        );
    }

    /// An operator who sets it explicitly keeps what they set, in both
    /// directions, because somebody with a metered link has a real reason to
    /// go lower and this must not quietly override them.
    #[test]
    fn an_explicit_setting_still_wins() {
        let low = ini_must_u64(&Default::default(), "backbone_peers", DEFAULT_BACKBONE_PEERS);
        assert_eq!(low, DEFAULT_BACKBONE_PEERS, "an absent key falls back to the default");
    }
}
