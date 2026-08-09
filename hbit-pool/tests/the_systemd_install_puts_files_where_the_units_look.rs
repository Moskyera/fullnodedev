//! Does the documented install actually put the binaries where the units run
//! them from?
//!
//! It did not. `deploy/README.md` installed every binary into `/opt/hbit/` while
//! both unit files execute out of `/opt/hbit/bin/`; it built a binary called
//! `fullnode` while the node unit runs one called `hacash`; and it created
//! `/etc/hbit` without ever writing the `hacash.config.ini` the node unit passes
//! as its only argument. An operator following that section got a machine where
//! systemd retried two units for ever and nothing ever started.
//!
//! None of that is catchable by compiling anything, which is exactly why it
//! survived: the README and the unit files are two documents that have to agree
//! and nothing made them. This test is what makes them.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Every absolute path a unit file executes: `ExecStart=` and `ExecStartPre=`,
/// taking the program word only.
fn executed_paths(unit: &str) -> Vec<String> {
    unit.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("ExecStart=") || l.starts_with("ExecStartPre="))
        .filter_map(|l| l.split_once('=').map(|(_, v)| v))
        .filter_map(|v| v.split_whitespace().next())
        .map(|p| p.trim_start_matches('-').to_string())
        .filter(|p| p.starts_with('/'))
        .collect()
}

/// Every file the README's install commands create, as a destination path.
/// Handles both `install ... SRC DIR/` and `install ... SRC DEST`.
fn installed_paths(readme: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in readme.lines().map(str::trim) {
        if !line.contains("install ") || !line.contains("/opt/hbit") && !line.contains("/etc/hbit")
        {
            continue;
        }
        let Some(last) = line.split_whitespace().last() else {
            continue;
        };
        if !last.starts_with('/') {
            continue;
        }
        let src = line.split_whitespace().rev().nth(1).unwrap_or("");
        if last.ends_with('/') {
            // install SRC DIR/ -> DIR/basename(SRC)
            let base = src.rsplit('/').next().unwrap_or(src);
            out.push(format!("{last}{base}"));
        } else {
            out.push(last.to_string());
        }
    }
    out
}

#[test]
fn every_binary_a_unit_runs_is_one_the_readme_installs_there() {
    let readme = read("deploy/README.md");
    let installed = installed_paths(&readme);
    assert!(
        !installed.is_empty(),
        "no install commands were found in deploy/README.md; this test must not pass by \
         finding nothing"
    );

    for unit_rel in [
        "deploy/systemd/hacash-node.service",
        "deploy/systemd/hbit-pool.service",
    ] {
        let unit = read(unit_rel);
        for exec in executed_paths(&unit) {
            assert!(
                installed.contains(&exec),
                "{unit_rel} executes {exec}, which the systemd section of deploy/README.md \
                 never puts there. It installs: {installed:#?}"
            );
        }
    }
}

#[test]
fn the_node_unit_is_given_a_config_the_readme_creates() {
    // The node takes its config path as its ONLY argument and exits without it.
    // Nothing in the install ever wrote that file.
    let unit = read("deploy/systemd/hacash-node.service");
    let arg = unit
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("ExecStart="))
        .and_then(|l| l.split_once('='))
        .map(|(_, v)| v)
        .and_then(|v| v.split_whitespace().nth(1))
        .expect("the node unit must pass a config path");
    assert!(
        arg.starts_with('/'),
        "expected an absolute config path, got {arg}"
    );

    let readme = read("deploy/README.md");
    assert!(
        installed_paths(&readme).contains(&arg.to_string()),
        "hacash-node.service starts the node with {arg}, and the systemd section of \
         deploy/README.md never creates it. The node exits immediately on every start."
    );
}

#[test]
fn the_firewall_recipe_allows_ssh_before_it_turns_the_firewall_on() {
    // ufw's default incoming policy is deny. Enabling it with no SSH rule locks
    // the operator out of the machine they are in the middle of configuring.
    let readme = read("deploy/README.md");
    let enable = readme
        .find("ufw enable")
        .expect("the README should still document enabling the firewall");
    let before = &readme[..enable];
    let ssh = before
        .rfind("ufw allow OpenSSH")
        .or_else(|| before.rfind("ufw allow 22"));
    assert!(
        ssh.is_some(),
        "deploy/README.md enables ufw without allowing SSH first, which locks the operator out"
    );
}

#[test]
fn the_documentation_link_in_each_unit_points_at_a_file_the_install_places() {
    let readme = read("deploy/README.md");
    let installed = installed_paths(&readme);
    for unit_rel in [
        "deploy/systemd/hacash-node.service",
        "deploy/systemd/hbit-pool.service",
    ] {
        let unit = read(unit_rel);
        for doc in unit
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("Documentation=file:"))
            .filter_map(|l| l.strip_prefix("Documentation=file:"))
        {
            assert!(
                installed.contains(&doc.to_string()),
                "{unit_rel} documents itself at {doc}, which nothing installs. \
                 `systemctl status` then names a file that is not on the machine."
            );
        }
    }
}

/// The `<share_bits>` argument of the `hbit-pool-server` command a file starts.
///
/// Works on all three shapes the repository ships it in: a systemd `ExecStart=`
/// line, a shell command in a script, and a docker-compose YAML argument list
/// with one argument per line. Comments are dropped first, because every one of
/// these files also TALKS about the arguments nearby.
fn share_bits_in_file(text: &str) -> Option<u32> {
    let code: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .collect::<Vec<_>>()
        .join("\n");
    let tokens: Vec<String> = code
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c| c == '"' || c == '\'' || c == ',')
                .to_string()
        })
        // YAML list markers, and the "=" that glues ExecStart to its program.
        .filter(|t| !t.is_empty() && t != "-")
        .map(|t| match t.split_once('=') {
            Some((k, v)) if k.starts_with("ExecStart") => v.to_string(),
            _ => t,
        })
        .collect();
    // Every occurrence, not the first: the usage text names the program twice,
    // once with placeholders (<node> <wallet_file> ...) and once with a real
    // example. Only the example has a number in the share_bits position.
    tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.ends_with("hbit-pool-server"))
        // <node> <wallet_file> <listen> <share_bits>
        .find_map(|(at, _)| tokens.get(at + 4)?.parse::<u32>().ok())
}

#[test]
fn every_shipped_command_line_uses_the_same_share_size() {
    // This is a payout-fairness parameter, not a throughput knob: it decides how
    // much history the 4096-share window holds, and therefore who is still in it
    // when a block is found. The repository shipped 24 in the systemd unit, 20
    // in docker-compose and 20 in the VPS setup script, and the program's own
    // help recommended 24 - the value the compose file carried a written
    // argument against.
    //
    // The one true value is DEFAULT_SHARE_BITS, quoted in the pool's own usage
    // text. Everything an operator can copy has to agree with it.
    // The authority is the pool's own help text, asked of the real binary, so
    // the constant and the shipped files are compared through the program rather
    // than through a number repeated in this test.
    let bin = repo_root()
        .join("target/debug")
        .join(format!("hbit-pool-server{}", std::env::consts::EXE_SUFFIX));
    let out = std::process::Command::new(&bin)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("run {} --help: {e}", bin.display()));
    assert!(out.status.success(), "--help must succeed");
    let want = share_bits_in_file(&String::from_utf8_lossy(&out.stdout))
        .expect("the pool's own usage must carry a working example command");

    let mut seen: Vec<(String, u32)> = Vec::new();
    for rel in [
        "deploy/systemd/hbit-pool.service",
        "deploy/docker-compose.yml",
        "scripts/hbit-vps-setup.sh",
    ] {
        if let Some(bits) = share_bits_in_file(&read(rel)) {
            seen.push((rel.to_string(), bits));
        }
    }
    assert_eq!(
        seen.len(),
        3,
        "all three shipped command lines must be readable; found {seen:?}"
    );
    assert!(
        !seen.is_empty(),
        "no shipped command line was found; this test must not pass by finding nothing"
    );
    for (file, bits) in &seen {
        assert_eq!(
            *bits, want,
            "{file} starts the pool with share_bits {bits}, but the project's value is {want}. \
             A pool started with a different share size keeps a different amount of payout \
             history, so this is a fairness setting and not a preference."
        );
    }
}

#[test]
fn no_unit_or_readme_still_points_at_a_script_that_does_not_exist() {
    // Both units used to say "deploy/install.sh does it". There is no such file,
    // so the one instruction an operator was given led nowhere.
    let root = repo_root();
    for rel in [
        "deploy/systemd/hacash-node.service",
        "deploy/systemd/hbit-pool.service",
        "deploy/README.md",
    ] {
        let text = read(rel);
        for line in text.lines() {
            if let Some(at) = line.find("deploy/install.sh") {
                assert!(
                    root.join("deploy/install.sh").exists(),
                    "{rel} refers to deploy/install.sh, which does not exist: {}",
                    &line[at..]
                );
            }
        }
    }
}
