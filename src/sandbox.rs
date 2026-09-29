//! OS-level sandbox for shell commands.
//!
//! * Linux: Landlock (no root, no containers). The harness re-executes itself
//!   as `harness __sandbox …` so the restriction is applied in a fresh,
//!   single-threaded process right before `exec`.
//! * macOS: Seatbelt via `sandbox-exec`.
//!
//! Policy: read everything, write only inside the workspace, temp dirs and
//! well-known tool caches; optionally no outbound TCP.

use std::path::{Path, PathBuf};

pub fn writable_roots(root: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        root.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev"),
    ];
    if let Ok(t) = std::env::var("TMPDIR") {
        v.push(PathBuf::from(t));
    }
    if let Some(home) = dirs::home_dir() {
        for d in [
            ".cache",
            ".cargo",
            ".rustup",
            ".npm",
            ".pnpm-store",
            ".yarn",
            ".bun",
            "go",
            ".gradle",
            ".m2",
            ".nuget",
            ".local/share/pnpm",
            ".deno",
            ".pub-cache",
            ".hex",
            ".mix",
            ".ivy2",
            ".sbt",
            ".composer",
            ".gem",
            "Library/Caches",
        ] {
            v.push(home.join(d));
        }
    }
    v.push(crate::config::data_dir());
    v.retain(|p| p.exists());
    v
}

/// Whether sandboxing is available on this platform.
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_supported()
    }
    #[cfg(target_os = "macos")]
    {
        Path::new("/usr/bin/sandbox-exec").exists()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_supported() -> bool {
    use std::sync::OnceLock;
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // Probe in a child so the current process stays unrestricted.
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        std::process::Command::new(exe)
            .args(["__sandbox", "--probe"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Build the argv that runs `bash -c script` inside the sandbox.
pub fn wrap(root: &Path, allow_network: bool, script: &str) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        let mut prof =
            String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
        for p in writable_roots(root) {
            let real = std::fs::canonicalize(&p).unwrap_or(p);
            prof.push_str(&format!(" (subpath \"{}\")", real.display()));
        }
        prof.push_str(" (subpath \"/private/tmp\") (subpath \"/private/var/folders\"))\n");
        if !allow_network {
            prof.push_str("(deny network-outbound (remote ip))\n");
        }
        return vec![
            "/usr/bin/sandbox-exec".into(),
            "-p".into(),
            prof,
            "bash".into(),
            "-c".into(),
            script.into(),
        ];
    }
    #[allow(unreachable_code)]
    {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "harness".into());
        let mut argv = vec![exe, "__sandbox".into()];
        for p in writable_roots(root) {
            argv.push("--write".into());
            argv.push(p.display().to_string());
        }
        if !allow_network {
            argv.push("--no-net".into());
        }
        argv.push("--".into());
        argv.extend(["bash".into(), "-c".into(), script.into()]);
        argv
    }
}

/// Entry point for `harness __sandbox [--probe] [--write DIR]… [--no-net] -- CMD…`.
#[cfg(target_os = "linux")]
pub fn helper_main(args: &[String]) -> ! {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };
    use std::os::unix::process::CommandExt;

    let mut write = vec![];
    let mut no_net = false;
    let mut probe = false;
    let mut i = 0;
    let mut cmd: Vec<String> = vec![];
    while i < args.len() {
        match args[i].as_str() {
            "--probe" => probe = true,
            "--no-net" => no_net = true,
            "--write" => {
                i += 1;
                if let Some(p) = args.get(i) {
                    write.push(p.clone());
                }
            }
            "--" => {
                cmd = args[i + 1..].to_vec();
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let abi = ABI::V5;
    let result = (|| -> Result<RulesetStatus, landlock::RulesetError> {
        let mut rs = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(abi))?;
        if no_net {
            rs = rs.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?;
        }
        let created = rs
            .create()?
            .add_rules(landlock::path_beneath_rules(
                ["/"],
                AccessFs::from_read(abi),
            ))?
            .add_rules(landlock::path_beneath_rules(
                &write,
                AccessFs::from_all(abi),
            ))?;
        Ok(created.restrict_self()?.ruleset)
    })();
    match result {
        Ok(RulesetStatus::FullyEnforced) | Ok(RulesetStatus::PartiallyEnforced) => {}
        Ok(RulesetStatus::NotEnforced) => {
            eprintln!("harness sandbox: Landlock is not supported by this kernel");
            std::process::exit(if probe { 1 } else { 126 });
        }
        Err(e) => {
            eprintln!("harness sandbox: {e}");
            std::process::exit(126);
        }
    }
    if probe {
        std::process::exit(0);
    }
    if cmd.is_empty() {
        eprintln!("harness sandbox: no command");
        std::process::exit(2);
    }
    let err = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
    eprintln!("harness sandbox: exec failed: {err}");
    std::process::exit(127);
}

#[cfg(not(target_os = "linux"))]
pub fn helper_main(_args: &[String]) -> ! {
    eprintln!("harness sandbox helper is only used on Linux");
    std::process::exit(1);
}
