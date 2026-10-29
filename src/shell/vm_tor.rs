//! The Tor client a VM's Tor mode goes through: the host's own `tor`, run by
//! the compositor outside every VM.
//!
//! It is started when a VM first switches to Tor mode and stopped when no VM
//! uses it any more (its data directory keeps the network consensus, so the
//! next start is quick). It only listens on loopback, and the VMs' gateways
//! (see [`super::vm_gateway`]) are its only clients: each logs in under its
//! own SOCKS credentials, which Tor's stream isolation turns into separate
//! circuits per VM, and per identity.
//!
//! Like every child the compositor starts, it is spawned from the main thread
//! with a parent-death signal, so a crash can't leave it running.

use std::{
    io::{BufRead, BufReader, Read},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use tracing::{info, warn};

use super::vm_gateway::TorEndpoint;

/// Least time between two attempts to start Tor.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// What the overview shows about Tor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TorStatus {
    pub running: bool,
    /// Percent of Tor's start-up done; 100 once it can build circuits.
    pub bootstrap: u8,
    /// The last thing Tor complained about.
    pub problem: Option<String>,
}

#[derive(Debug)]
pub struct TorService {
    endpoint: TorEndpoint,
    child: Option<Child>,
    bootstrap: Arc<AtomicU8>,
    problem: Arc<Mutex<Option<String>>>,
    last_attempt: Option<Instant>,
}

/// Where Tor keeps its state: `$XDG_DATA_HOME/ironland-compositor/vm/tor`.
pub fn data_dir() -> PathBuf {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    data_home.join("ironland-compositor/vm/tor")
}

/// The command line for a Tor that is only a SOCKS proxy on loopback `port`.
pub fn tor_args(data_dir: &Path, port: u16) -> Vec<String> {
    [
        "--DataDirectory",
        &data_dir.display().to_string(),
        // Streams that log in with different credentials never share a
        // circuit - that is how the VMs and their identities stay apart.
        "--SocksPort",
        &format!("127.0.0.1:{port} IsolateSOCKSAuth"),
        "--ClientOnly",
        "1",
        "--SafeLogging",
        "1",
        "--Log",
        "notice stdout",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The percentage in a Tor log line like `Bootstrapped 45% (loading_descriptors): ...`.
pub fn parse_bootstrap(line: &str) -> Option<u8> {
    let rest = &line[line.find("Bootstrapped ")? + "Bootstrapped ".len()..];
    rest[..rest.find('%')?].trim().parse().ok()
}

/// Whether a Tor log line is a complaint worth showing.
fn is_problem(line: &str) -> bool {
    line.contains("[warn]") || line.contains("[err]") || line.contains("[error]")
}

fn read_lines(pipe: impl Read + Send + 'static, mut on_line: impl FnMut(&str) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("vm-tor-log".into())
        .spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                on_line(&line);
            }
        });
    if let Err(err) = spawned {
        warn!(%err, "Could not start the Tor log reader");
    }
}

impl TorService {
    pub fn new(endpoint: TorEndpoint) -> Self {
        Self {
            endpoint,
            child: None,
            bootstrap: Arc::new(AtomicU8::new(0)),
            problem: Arc::new(Mutex::new(None)),
            last_attempt: None,
        }
    }

    pub fn endpoint(&self) -> TorEndpoint {
        self.endpoint.clone()
    }

    fn set_problem(&self, text: Option<String>) {
        if let Ok(mut problem) = self.problem.lock() {
            *problem = text;
        }
    }

    /// Whether Tor is running; notices one that exited.
    pub fn running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                warn!(%status, "Tor exited");
                if self.problem.lock().is_ok_and(|p| p.is_none()) {
                    self.set_problem(Some(format!("tor exited ({status})")));
                }
                self.child = None;
                self.endpoint.set(None);
                false
            }
            Err(_) => false,
        }
    }

    pub fn status(&mut self) -> TorStatus {
        TorStatus {
            running: self.running(),
            bootstrap: self.bootstrap.load(Ordering::Relaxed),
            problem: self.problem.lock().ok().and_then(|p| p.clone()),
        }
    }

    /// Starts Tor unless it is running. Must be called from the main thread
    /// (see the module doc). Tries at most every [`RETRY_AFTER`], so a Tor
    /// that keeps failing isn't restarted in a loop.
    pub fn ensure_running(&mut self, binary: &str, dir: &Path) -> Result<(), String> {
        if self.running() {
            return Ok(());
        }
        if self
            .last_attempt
            .is_some_and(|at| at.elapsed() < RETRY_AFTER)
        {
            return Err(self
                .problem
                .lock()
                .ok()
                .and_then(|p| p.clone())
                .unwrap_or_else(|| "tor keeps failing".into()));
        }
        self.last_attempt = Some(Instant::now());
        self.set_problem(None);
        self.bootstrap.store(0, Ordering::Relaxed);

        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        // Tor refuses a data directory others can read.
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let port = super::vm::free_loopback_port().map_err(|e| format!("finding a port: {e}"))?;

        let mut command = Command::new(binary);
        command
            .args(tor_args(dir, port))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::vm::die_with_parent(&mut command);
        // Tor drops its privileges and forks nothing, but give it its own
        // group anyway so a stray signal to ours doesn't reach it.
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("starting {binary}: {e}"))?;

        let bootstrap = self.bootstrap.clone();
        let problem = self.problem.clone();
        if let Some(stdout) = child.stdout.take() {
            read_lines(stdout, move |line| {
                if let Some(percent) = parse_bootstrap(line) {
                    bootstrap.store(percent, Ordering::Relaxed);
                } else if is_problem(line)
                    && let Ok(mut problem) = problem.lock()
                {
                    *problem = Some(line.split("] ").last().unwrap_or(line).to_string());
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let problem = self.problem.clone();
            read_lines(stderr, move |line| {
                if let Ok(mut problem) = problem.lock() {
                    *problem = Some(line.split("] ").last().unwrap_or(line).to_string());
                }
            });
        }
        info!(port, binary, "Started Tor");
        self.endpoint.set(Some(port));
        self.child = Some(child);
        Ok(())
    }

    /// Stops Tor if it is running.
    pub fn stop(&mut self) {
        self.endpoint.set(None);
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            // Tor shuts down within a moment of SIGTERM (it waits a few
            // seconds if it still has clients); don't wait here.
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
            info!("Stopped Tor");
        }
        self.bootstrap.store(0, Ordering::Relaxed);
    }
}

impl Drop for TorService {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn bootstrap_progress_is_read_from_tors_log() {
        assert_eq!(
            parse_bootstrap(
                "Oct 29 17:05:23.000 [notice] Bootstrapped 45% (loading_descriptors): Loading relay descriptors"
            ),
            Some(45)
        );
        assert_eq!(
            parse_bootstrap("[notice] Bootstrapped 100% (done): Done"),
            Some(100)
        );
        assert_eq!(parse_bootstrap("[notice] Bootstrapped 5%"), Some(5));
        assert_eq!(parse_bootstrap("[notice] Opening Socks listener"), None);
        assert_eq!(parse_bootstrap("Bootstrapped lots% of it"), None);
    }

    #[test]
    fn tor_is_only_a_loopback_socks_proxy_with_isolation() {
        let args = tor_args(Path::new("/data/tor"), 9150);
        let joined = args.join(" ");
        assert!(joined.contains("--DataDirectory /data/tor"), "{joined}");
        assert!(
            joined.contains("--SocksPort 127.0.0.1:9150 IsolateSOCKSAuth"),
            "{joined}"
        );
        assert!(joined.contains("--ClientOnly 1"), "{joined}");
        // No other listeners: nothing is asked for besides the SOCKS port.
        assert!(!joined.contains("ControlPort"), "{joined}");
        assert!(!joined.contains("TransPort"), "{joined}");
        assert!(!joined.contains("0.0.0.0"), "{joined}");
    }

    /// A stand-in `tor`: prints progress like the real one, then waits.
    fn fake_tor_binary(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ivtor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tor");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        for _ in 0..100 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out");
    }

    #[test]
    fn the_service_starts_tor_follows_its_progress_and_stops_it() {
        let home = std::env::temp_dir().join(format!("ivtor-home-{}", std::process::id()));
        let dir = home.join("tor");
        let binary = fake_tor_binary(
            "ok",
            "echo '[notice] Bootstrapped 10% (conn): Connecting'; echo '[notice] Bootstrapped 100% (done): Done'; sleep 30",
        );
        let endpoint = TorEndpoint::default();
        let mut service = TorService::new(endpoint.clone());
        assert_eq!(service.status(), TorStatus::default());

        service
            .ensure_running(binary.to_str().unwrap(), &dir)
            .unwrap();
        assert!(endpoint.get().is_some());
        wait_for(|| service.status().bootstrap == 100);
        let status = service.status();
        assert!(status.running);
        assert_eq!(status.problem, None);
        assert!(dir.exists());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "Tor refuses a data directory others can read"
        );

        // Starting again while it runs changes nothing.
        let port = endpoint.get();
        service
            .ensure_running(binary.to_str().unwrap(), &dir)
            .unwrap();
        assert_eq!(endpoint.get(), port);

        service.stop();
        assert_eq!(endpoint.get(), None);
        assert!(!service.running());
        assert_eq!(service.status().bootstrap, 0);
        let _ = std::fs::remove_dir_all(binary.parent().unwrap());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_tor_that_exits_is_noticed_and_not_restarted_in_a_loop() {
        let binary = fake_tor_binary("bad", "echo '[err] Reading config failed' >&2; exit 1");
        let endpoint = TorEndpoint::default();
        let mut service = TorService::new(endpoint.clone());
        let dir = binary.parent().unwrap().join("data");
        service
            .ensure_running(binary.to_str().unwrap(), &dir)
            .unwrap();
        wait_for(|| !service.running());
        assert_eq!(endpoint.get(), None, "a dead Tor has no port");
        let problem = service.status().problem.unwrap();
        assert!(problem.contains("Reading config failed"), "{problem}");
        // Asking again at once is refused with the reason, not retried.
        let err = service
            .ensure_running(binary.to_str().unwrap(), &dir)
            .unwrap_err();
        assert!(err.contains("Reading config failed"), "{err}");
        let _ = std::fs::remove_dir_all(binary.parent().unwrap());
    }

    #[test]
    fn a_missing_tor_binary_is_a_clear_error() {
        let mut service = TorService::new(TorEndpoint::default());
        let dir = std::env::temp_dir().join(format!("ivtor-missing-{}", std::process::id()));
        let err = service
            .ensure_running("/nonexistent/tor", &dir)
            .unwrap_err();
        assert!(err.contains("/nonexistent/tor"), "{err}");
        assert!(!service.running());
        let _ = std::fs::remove_dir_all(dir);
    }
}
