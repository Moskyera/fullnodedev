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

/// A `;` or `#` opens a comment when it starts the line or follows whitespace, which is the
/// usual ini convention. Anywhere else it belongs to the value, so `token = a#b` keeps `a#b`.
fn strip_ini_comment(line: &str) -> &str {
    let mut prev_is_space = true; // the start of a line counts as a boundary
    for (idx, ch) in line.char_indices() {
        if (ch == ';' || ch == '#') && prev_is_space {
            return &line[..idx];
        }
        prev_is_space = ch.is_whitespace();
    }
    line
}

/// Parse ini-syntax content into the shared `IniObj` map.
///
/// This used to call `ini::configparser`, whose parser cuts every line at the FIRST `;` or `#`
/// found anywhere in it. That silently truncated any value that legitimately contains one of
/// those characters, a generated wallet password or an api token for example, and nothing was
/// printed: the node simply ran with a different, shorter secret than the operator set. The
/// same parser also read any line holding `[` and `]` as a section header, even mid value.
///
/// The rules below are the ordinary ini rules and keep everything the existing configs rely on:
/// section and key names are lower cased, values are trimmed, a key written without `=` stores
/// `None`, and keys placed before the first section land in the `default` section. Inline
/// comments still work; they now have to start the line or follow whitespace.
fn parse_ini_content(content: &str) -> Result<IniObj, String> {
    const DEFAULT_SECTION: &str = "default";
    let mut map = IniObj::new();
    let mut section = DEFAULT_SECTION.to_owned();
    for (num, line) in content.lines().enumerate() {
        let trimmed = strip_ini_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            let Some(end) = trimmed.rfind(']') else {
                return Err(format!("line {}: found opening bracket but no closing bracket", num + 1));
            };
            section = trimmed[1..end].trim().to_lowercase();
            continue;
        }
        let (key, value) = match trimmed.find('=') {
            Some(delimiter) => (
                trimmed[..delimiter].trim().to_lowercase(),
                Some(trimmed[delimiter + 1..].trim().to_owned()),
            ),
            None => (trimmed.to_lowercase(), None),
        };
        if key.is_empty() {
            return Err(format!("line {}: key cannot be empty", num + 1));
        }
        map.entry(section.clone())
            .or_insert_with(HashMap::new)
            .insert(key, value);
    }
    Ok(map)
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
    match parse_ini_content(&content) {
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
mod ini_content_tests {
    use super::*;

    fn parse(content: &str) -> IniObj {
        parse_ini_content(content).expect("content must parse")
    }

    fn val(ini: &IniObj, section: &str, key: &str) -> String {
        ini_must(ini_section(ini, section), key, "<missing>")
    }

    #[test]
    fn inline_comments_never_reach_the_value() {
        // an operator annotating a line is normal, it must not corrupt what the node reads
        let ini = parse(
            "[miner]\n\
             enable = true ; mine HAC\n\
             reward = 1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9 ; my payout wallet\n\
             [diamondminer]\n\
             enable = yes # mine HACD\n\
             bid_max = 1:248 # never bid more than this\n\
             bid_min = 12.5 ; twelve and a half\n",
        );
        // bool
        assert_eq!(ini_must_bool(ini_section(&ini, "miner"), "enable", false), true);
        assert_eq!(ini_must_bool(ini_section(&ini, "diamondminer"), "enable", false), true);
        // address
        assert_eq!(val(&ini, "miner", "reward"), "1MzNY1oA3kfgYi75zquj3SRUPYztzXHzK9");
        // amount
        assert_eq!(val(&ini, "diamondminer", "bid_max"), "1:248");
        assert_eq!(val(&ini, "diamondminer", "bid_min"), "12.5");
    }

    #[test]
    fn a_comment_symbol_inside_a_value_is_kept() {
        // a generated wallet password or api token may contain these characters, cutting the
        // value there would silently swap the secret the node actually uses
        let ini = parse(
            "[diamondminer]\n\
             bid_password = pa#ss;word\n\
             [server]\n\
             api_token = t#k;n\n",
        );
        assert_eq!(val(&ini, "diamondminer", "bid_password"), "pa#ss;word");
        assert_eq!(val(&ini, "server", "api_token"), "t#k;n");
    }

    #[test]
    fn whole_line_comments_are_dropped() {
        let ini = parse(
            "; a note\n\
             # another note\n\
             \t; an indented note\n\
             [mint]\n\
             ; difficulty_adjust_blocks = 10\n\
             chain_id = 0\n",
        );
        assert_eq!(val(&ini, "mint", "chain_id"), "0");
        assert_eq!(val(&ini, "mint", "difficulty_adjust_blocks"), "<missing>");
    }

    #[test]
    fn sections_and_keys_are_lower_cased_and_values_trimmed() {
        let ini = parse("[Miner]\n  Enable   =   TrUe   \n");
        assert_eq!(val(&ini, "miner", "enable"), "TrUe");
        assert_eq!(ini_must_bool(ini_section(&ini, "miner"), "enable", false), true);
    }

    #[test]
    fn keys_before_the_first_section_land_in_default() {
        let ini = parse("connect = 127.0.0.1:8080\nsupervene = 4\n\n[gpu]\ndebug = 0\n");
        assert_eq!(val(&ini, "default", "connect"), "127.0.0.1:8080");
        assert_eq!(val(&ini, "default", "supervene"), "4");
        assert_eq!(val(&ini, "gpu", "debug"), "0");
    }

    #[test]
    fn an_empty_value_stays_empty_and_a_bare_key_has_no_value() {
        let ini = parse("[efficiency]\nthermal_file =\nstandalone\n");
        let sec = ini_section(&ini, "efficiency");
        assert_eq!(sec.get("thermal_file"), Some(&Some(String::new())));
        assert_eq!(sec.get("standalone"), Some(&None));
        // a key present but blank must not resolve to the caller's default
        assert_eq!(ini_must(sec, "thermal_file", "fallback"), "");
    }

    #[test]
    fn brackets_inside_a_value_do_not_open_a_section() {
        let ini = parse("[server]\nnote = a [b] c\nbind = 127.0.0.1\n");
        assert_eq!(val(&ini, "server", "note"), "a [b] c");
        assert_eq!(val(&ini, "server", "bind"), "127.0.0.1");
    }

    #[test]
    fn a_repeated_section_keeps_both_keys() {
        let ini = parse("[miner]\nenable = true\n[node]\nlisten = 3033\n[miner]\nreward = abc\n");
        assert_eq!(val(&ini, "miner", "enable"), "true");
        assert_eq!(val(&ini, "miner", "reward"), "abc");
        assert_eq!(val(&ini, "node", "listen"), "3033");
    }

    #[test]
    fn a_broken_section_header_is_an_error() {
        assert!(parse_ini_content("[miner\nenable = true\n").is_err());
        assert!(parse_ini_content("[miner]\n = 5\n").is_err());
    }

    #[test]
    fn the_shipped_config_layout_still_reads() {
        let ini = parse(
            "\n\n[node]\nfast_sync = false\nnot_find_nodes = true\nlisten = 3033\n\n\
             [mint]\n; difficulty_adjust_blocks = 10\n\n\
             [server]\nenable = true\nlisten = 8080\n\
             ; Default is loopback-only. Use bind = 0.0.0.0 only with a non-empty api_token.\n\
             bind = 127.0.0.1\n; api_token =\ndiamond_form = true\n\n\
             [miner]\nenable = true\nreward = 1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v\nmessage = hvm_dev\n",
        );
        assert_eq!(val(&ini, "node", "listen"), "3033");
        assert_eq!(val(&ini, "server", "bind"), "127.0.0.1");
        assert_eq!(val(&ini, "server", "api_token"), "<missing>");
        assert_eq!(val(&ini, "miner", "reward"), "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v");
        assert_eq!(val(&ini, "miner", "message"), "hvm_dev");
        assert_eq!(ini_must_bool(ini_section(&ini, "miner"), "enable", false), true);
    }
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
