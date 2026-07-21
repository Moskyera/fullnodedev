// pub type IniObj = HashMap<String, HashMap<String, Option<String>>>;

pub fn get_current_exe_absolute_dir(dir: &str) -> PathBuf {
    let mut ddrp = PathBuf::from(dir);
    // println!("{:?} {}", ddrp, ddrp.is_absolute());
    if !ddrp.is_absolute() {
        ddrp = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
            .join(dir);
    }
    ddrp
}

/*
* get data path
*/
pub fn get_mainnet_data_dir(ini: &IniObj) -> PathBuf {
    let sec = ini_section(ini, "default"); // default = root
    let data_dir = ini_must(sec, "data_dir", "hacash_mainnet_data");

    get_current_exe_absolute_dir(&data_dir)
}

fn resolve_config_path_from(
    default_config: &str,
    args: &[std::ffi::OsString],
    executable_dir: &std::path::Path,
) -> PathBuf {
    if args.len() == 2 {
        PathBuf::from(&args[1])
    } else {
        executable_dir.join(default_config)
    }
}

pub fn resolve_config_path(default_config: &str) -> PathBuf {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let path = resolve_config_path_from(default_config, &args, executable_dir.as_path());
    path.canonicalize().unwrap_or(path)
}

pub fn load_config_path(config_path: &std::path::Path) -> IniObj {
    if !config_path.exists() {
        println!(
            "[Config Error] cannot find config file {}",
            config_path.display()
        );
        return IniObj::new();
    }

    let canonical_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let content = match std::fs::read_to_string(&canonical_path) {
        Ok(content) => content,
        Err(error) => {
            println!(
                "[Config Error] cannot read config file {}: {}",
                canonical_path.display(),
                error
            );
            return IniObj::new();
        }
    };
    println!("[Config] load: {} {}.", canonical_path.display(), ctshow());
    let mut parser = ini::configparser::ini::Ini::new();
    match parser.read(content) {
        Ok(config) => config,
        Err(error) => {
            println!(
                "[Config Error] cannot parse config file {}: {}",
                canonical_path.display(),
                error
            );
            IniObj::new()
        }
    }
}

pub fn load_config(default_config: String) -> IniObj {
    let config_path = resolve_config_path(&default_config);
    load_config_path(&config_path)
}

#[cfg(test)]
mod config_path_tests {
    use super::*;

    #[test]
    fn config_path_uses_only_one_explicit_argument() {
        let executable_dir = PathBuf::from("miner-bin");
        let default_args = vec![std::ffi::OsString::from("poworker")];
        assert_eq!(
            resolve_config_path_from("./poworker.config.ini", &default_args, &executable_dir),
            executable_dir.join("./poworker.config.ini")
        );

        let override_path = PathBuf::from("isolated").join("poworker.config.ini");
        let override_args = vec![
            std::ffi::OsString::from("poworker"),
            override_path.as_os_str().to_owned(),
        ];
        assert_eq!(
            resolve_config_path_from("./poworker.config.ini", &override_args, &executable_dir),
            override_path
        );

        let extra_args = vec![
            std::ffi::OsString::from("poworker"),
            std::ffi::OsString::from("one.ini"),
            std::ffi::OsString::from("unexpected"),
        ];
        assert_eq!(
            resolve_config_path_from("./poworker.config.ini", &extra_args, &executable_dir),
            executable_dir.join("./poworker.config.ini")
        );
    }
}
