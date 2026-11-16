use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// Must match flake.nix
const PROXY_PORT: u32 = 3128;
const ENV_PORT: u32 = 3129;

// Host environment variables forwarded into the guest session: none unless listed
// in AGENTVM_FORWARD_ENV (space-separated names; set by the Nix package's forwardEnv)

const GUEST_UID: u32 = 1000;
const GUEST_GID: u32 = 1000;

// Socket names the microvm.nix runner uses, relative to its working directory
const VIRTIOFS_SOCKET: &str = "agentvm-virtiofs-work.sock";
const VSOCK_SOCKET: &str = "notify.vsock";

// Must match storeImage in flake.nix
const STORE_IMAGE: &str = "nix-store-overlay.img";
// Image size when first created, in MiB; the Nix package sets AGENTVM_STORE_MIB
const DEFAULT_STORE_MIB: u64 = 20480;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let current_dir = env::current_dir()?;

    // Keep the guest's persistent home/cache (.agentvm/) out of the project's
    // tracked git history without touching the project's own .gitignore
    let _ = ignore_agentvm_dir(&current_dir);

    let runner_path =
        env::var("AGENTVM_RUNNER").map_err(|_| "AGENTVM_RUNNER environment variable not set")?;
    // AGENTVM_DEBUG=1 prints the guest boot log after the session ends

    let debug = env::var_os("AGENTVM_DEBUG").is_some();

    // Persistent writable nix store layer for the guest, kept in the project. The
    // lock stops two sessions in the same directory from corrupting it.
    let cache_dir = current_dir.join(".agentvm/.cache/devenv");
    fs::create_dir_all(&cache_dir)?;
    let store_image = cache_dir.join("store.img");
    let image_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&store_image)?;
    if unsafe { libc::flock(image_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("❌ Error: {} is in use by another agentvm session.", store_image.display());
        std::process::exit(1);
    }
    let store_mib: u64 = env::var("AGENTVM_STORE_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_STORE_MIB);
    if image_file.metadata()?.len() == 0 {
        println!("💾 Creating nix store image: {}", store_image.display());
        image_file.set_len(store_mib * 1024 * 1024)?;
        let ok = Command::new("mkfs.ext4")
            .args(["-q", "-F"])
            .arg(&store_image)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = fs::remove_file(&store_image);
            return Err("mkfs.ext4 failed".into());
        }
    }

    let run_dir = env::temp_dir().join(format!("agentvm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&run_dir);
    fs::create_dir_all(&run_dir)?;
    symlink(&store_image, run_dir.join(STORE_IMAGE))?;
    let virtiofs_socket = run_dir.join(VIRTIOFS_SOCKET);

    println!("📁 Sharing directory: {}", current_dir.display());

    // 1. Spawn virtiofsd for project directory mounting; map the guest's agentvm
    //    user (GUEST_UID/GID) onto the invoking host user
    let (host_uid, host_gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut virtiofsd = Command::new("virtiofsd")
        .arg(format!("--socket-path={}", virtiofs_socket.display()))
        .arg(format!("--shared-dir={}", current_dir.display()))
        .arg(format!("--translate-uid=map:{}:{}:1", GUEST_UID, host_uid))
        .arg(format!("--translate-gid=map:{}:{}:1", GUEST_GID, host_gid))
        .arg("--cache=always")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if !wait_for(Duration::from_secs(5), || virtiofs_socket.exists()) {
        eprintln!("❌ Error: virtiofsd did not create its socket.");
        teardown(&mut virtiofsd, &run_dir);
        std::process::exit(1);
    }

    // 2. Host-side HTTP proxy; guest -> host vsock connections on port N
    //    arrive at "<vsock socket>_N"
    let listener = UnixListener::bind(run_dir.join(format!("{}_{}", VSOCK_SOCKET, PROXY_PORT)))?;
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            thread::spawn(move || {
                let _ = handle_proxy(conn);
            });
        }
    });

    // Serve forwarded environment variables as shell exports; the guest session
    // evals them before starting claude
    let forwarded = env::var("AGENTVM_FORWARD_ENV").unwrap_or_default();
    let mut exports: String = forwarded
        .split_whitespace()
        // Names are spliced into the script the guest evals; allow identifiers only
        .filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .filter_map(|name| env::var(name).ok().map(|v| (name, v)))
        .map(|(name, v)| format!("export {}={}\n", name, shell_quote(&v)))
        .collect();
    // agentvm's own arguments become claude's arguments (e.g. --resume <id>)
    let claude_args: Vec<String> = env::args().skip(1).map(|a| shell_quote(&a)).collect();
    exports.push_str(&format!("set -- {}\n", claude_args.join(" ")));
    let env_listener = UnixListener::bind(run_dir.join(format!("{}_{}", VSOCK_SOCKET, ENV_PORT)))?;
    thread::spawn(move || {
        for mut conn in env_listener.incoming().flatten() {
            let _ = conn.write_all(exports.as_bytes());
        }
    });

    println!("⚡ Booting AgentVM...");

    // 3. Run the microvm.nix runner in the foreground: cloud-hypervisor attaches the
    //    guest console to this terminal and exits when the guest powers off
    let status = Command::new(runner_path).current_dir(&run_dir).status();

    if debug {
        let console = fs::read_to_string(run_dir.join("console.log")).unwrap_or_default();
        let lines: Vec<&str> = console.lines().collect();
        eprintln!("--- guest console (last 40 lines) ---");
        for line in &lines[lines.len().saturating_sub(40)..] {
            eprintln!("{}", line);
        }
    }
    if let Ok(s) = &status {
        if !s.success() {
            eprintln!("❌ AgentVM runner exited with status: {}", s);
        }
    }

    println!("🧹 Tearing down AgentVM and host connections...");
    teardown(&mut virtiofsd, &run_dir);
    status?;

    Ok(())
}

/// Adds `/.agentvm/` to the repo-local git excludes (.git/info/exclude) so the
/// guest's persistent home/cache directory is never staged, regardless of the
/// project's own .gitignore. A no-op outside a git repo. Uses `--git-common-dir`
/// so this also works from worktrees and submodules.
fn ignore_agentvm_dir(current_dir: &Path) -> io::Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(current_dir)
        .output()?;
    if !output.status.success() {
        return Ok(()); // not a git repo
    }
    let common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let git_dir = PathBuf::from(common_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        current_dir.join(git_dir)
    };

    let exclude_path = git_dir.join("info/exclude");
    let pattern = "/.agentvm/";
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }

    fs::create_dir_all(exclude_path.parent().unwrap())?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    writeln!(file, "{}", pattern)?;
    Ok(())
}

fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Minimal HTTP proxy: CONNECT tunnels plus plain absolute-URI requests.
fn handle_proxy(conn: UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut head = String::new();
    reader.read_line(&mut head)?;
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        headers.push_str(&line);
    }

    let mut parts = head.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let mut client = conn;

    let upstream = if method == "CONNECT" {
        let upstream = TcpStream::connect(target);
        match &upstream {
            Ok(_) => client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?,
            Err(_) => client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")?,
        }
        upstream?
    } else {
        let host = target
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");
        let addr = if host.contains(':') {
            host.to_string()
        } else {
            format!("{}:80", host)
        };
        let mut upstream = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")?;
                return Err(e);
            }
        };
        upstream.write_all(head.as_bytes())?;
        upstream.write_all(headers.as_bytes())?;
        upstream.write_all(b"\r\n")?;
        upstream
    };

    // Forward anything already buffered from the client, then splice both ways
    let mut up_writer = upstream.try_clone()?;
    up_writer.write_all(reader.buffer())?;
    let mut up_reader = upstream;
    let mut client_writer = client.try_clone()?;
    let down = thread::spawn(move || {
        let _ = io::copy(&mut up_reader, &mut client_writer);
        let _ = client_writer.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut client, &mut up_writer);
    let _ = up_writer.shutdown(Shutdown::Write);
    let _ = down.join();
    Ok(())
}

fn teardown(virtiofsd: &mut Child, run_dir: &Path) {
    let _ = virtiofsd.kill();
    let _ = virtiofsd.wait();
    let _ = fs::remove_dir_all(run_dir);
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
