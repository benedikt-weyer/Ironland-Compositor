//! Persistence for VMs started from the launcher's persistent tab (see
//! [`super::vm`]): a per-application qcow2 overlay on top of the application's own image (`vm.programs`) plus a
//! QEMU internal snapshot of the guest's disk *and memory*.
//!
//! The first launch of an application cold-boots. When its program exits
//! cleanly the compositor asks QEMU, over QMP, to `savevm` and quit; the next
//! launch of the same application starts QEMU with `-loadvm`, so the guest
//! resumes where it was (fast, no boot) with every file the previous session
//! wrote still there. Launches from the throw-away tab never touch any of this.
//!
//! Layout, one directory per application under [`state_root`]:
//!
//! ```text
//! <state_root>/<key>/disk.qcow2   overlay; backing file is the application's image
//! <state_root>/<key>/meta.json    [`PersistMeta`]: what the overlay is valid for
//! ```
//!
//! Two things invalidate what's there, and are handled differently:
//!
//! * the base image changed (rebuilt guest): the overlay's backing file no
//!   longer matches, so the whole directory is moved aside to
//!   `<key>.stale-<unix time>` (never deleted - it holds the user's data) and
//!   a new one is started;
//! * the machine configuration changed (memory, CPUs, extra QEMU args): the
//!   saved state can't be loaded into a different machine, so it is skipped
//!   and the guest cold-boots from the overlay's disk.

use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::VmSettings;

/// Name of the internal snapshot holding the saved state.
pub const STATE_TAG: &str = "ironland";

/// How long QEMU gets to write the saved state (it dumps all guest RAM).
pub const SAVE_TIMEOUT: Duration = Duration::from_secs(180);

/// What an overlay directory is valid for, stored next to it as `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistMeta {
    /// Base image the overlay is backed by, and its size and mtime when the
    /// overlay was created. A mismatch means the backing file was replaced.
    pub image: String,
    pub image_len: u64,
    pub image_mtime: u64,
    /// [`machine_fingerprint`] of the machine the saved state came from.
    pub machine: String,
    /// Whether the overlay holds a saved state ([`STATE_TAG`]) to load.
    pub has_state: bool,
}

/// What [`prepare`] found or made.
#[derive(Debug)]
pub struct Prepared {
    pub overlay: PathBuf,
    pub meta_path: PathBuf,
    pub meta: PersistMeta,
    /// Whether the launch should restore the saved state (`-loadvm`) rather
    /// than cold-boot.
    pub load_state: bool,
}

/// A directory-name-safe key for a desktop entry id: everything outside
/// `[A-Za-z0-9._-]` becomes `_`, and a leading dot is replaced so the key can
/// never be `.`/`..` or a hidden directory.
pub fn persist_key(entry_id: &str) -> String {
    let mut key: String = entry_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if key.is_empty() || key.starts_with('.') {
        key.insert(0, '_');
    }
    key
}

/// Directory holding every persistent VM's state.
pub fn state_root(settings: &VmSettings) -> PathBuf {
    if let Some(dir) = settings
        .state_dir
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        return super::vm::expand_tilde(dir);
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    data_home.join("ironland-compositor/vm/persistent")
}

/// The settings a saved state is tied to: it can only be loaded into a
/// machine configured identically.
pub fn machine_fingerprint(settings: &VmSettings) -> String {
    format!(
        "mem={};cpus={};kvm={};extra={:?}",
        settings.memory_mb,
        settings.cpus.max(1),
        settings.kvm,
        settings.extra_qemu_args
    )
}

/// `(size, mtime in unix nanoseconds)` of the base image. Nanoseconds, because
/// a rebuilt image can have the same size as the old one and be written within
/// the same second.
pub fn image_identity(image: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::metadata(image)?;
    let mtime = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Ok((meta.len(), mtime))
}

/// `"qcow2"` or `"raw"`, from the file's magic number: the overlay needs its
/// backing format spelled out, and probing is exactly what qemu refuses to do
/// for raw images.
pub fn image_format(image: &Path) -> io::Result<&'static str> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let mut file = fs::File::open(image)?;
    let n = file.read(&mut magic)?;
    Ok(if n == 4 && magic == *b"QFI\xfb" {
        "qcow2"
    } else {
        "raw"
    })
}

fn read_meta(path: &Path) -> Option<PersistMeta> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_meta(path: &Path, meta: &PersistMeta) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
    // Written then renamed, so a crash can't leave half a file behind.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(tmp, path)
}

/// Moves `dir` aside as `<dir>.stale-<unix time>`.
fn move_aside(dir: &Path) -> io::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".stale-{stamp}"));
    let target = dir.with_file_name(name);
    fs::rename(dir, &target)?;
    Ok(target)
}

/// Makes sure `dir` holds a usable overlay of `image` and decides whether the
/// launch can restore a saved state. Creates the overlay on first use; moves a
/// directory whose backing image has changed aside (see the module doc).
pub fn prepare(dir: &Path, image: &Path, settings: &VmSettings) -> Result<Prepared, String> {
    let (len, mtime) =
        image_identity(image).map_err(|e| format!("reading {}: {e}", image.display()))?;
    let overlay = dir.join("disk.qcow2");
    let meta_path = dir.join("meta.json");
    let fingerprint = machine_fingerprint(settings);

    let image_str = image.display().to_string();
    let matches_image =
        |m: &PersistMeta| m.image == image_str && m.image_len == len && m.image_mtime == mtime;

    if overlay.exists() {
        match read_meta(&meta_path) {
            Some(meta) if matches_image(&meta) => {
                let load_state = meta.has_state && meta.machine == fingerprint;
                return Ok(Prepared {
                    overlay,
                    meta_path,
                    meta,
                    load_state,
                });
            }
            // Unknown or replaced backing image: keep the data, start over.
            _ => {
                let moved =
                    move_aside(dir).map_err(|e| format!("moving a stale VM state aside: {e}"))?;
                tracing::warn!(
                    from = %dir.display(),
                    to = %moved.display(),
                    "VM base image changed; kept the old persistent state aside and starting fresh"
                );
            }
        }
    }

    fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let format = image_format(image).map_err(|e| format!("reading {}: {e}", image.display()))?;
    let output = Command::new("qemu-img")
        .args(["create", "-q", "-f", "qcow2", "-b"])
        .arg(image)
        .args(["-F", format])
        .arg(&overlay)
        .output()
        .map_err(|e| format!("running qemu-img: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "qemu-img create failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let meta = PersistMeta {
        image: image_str,
        image_len: len,
        image_mtime: mtime,
        machine: fingerprint,
        has_state: false,
    };
    write_meta(&meta_path, &meta).map_err(|e| format!("writing {}: {e}", meta_path.display()))?;
    Ok(Prepared {
        overlay,
        meta_path,
        meta,
        load_state: false,
    })
}

/// Records that the overlay now holds a state saved by a machine matching
/// `settings`.
pub fn mark_saved(
    prepared_meta: &PersistMeta,
    meta_path: &Path,
    settings: &VmSettings,
) -> io::Result<()> {
    let meta = PersistMeta {
        machine: machine_fingerprint(settings),
        has_state: true,
        ..prepared_meta.clone()
    };
    write_meta(meta_path, &meta)
}

// ---------------------------------------------------------------------------
// QMP / guest agent clients. Line-delimited JSON over a unix socket.
// ---------------------------------------------------------------------------

/// Reads lines until one has a `return` or `error` key (QMP interleaves
/// asynchronous `event` lines), returning it.
fn read_reply(reader: &mut BufReader<UnixStream>) -> Result<Value, String> {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("the socket was closed".into()),
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("return").is_some() || value.get("error").is_some() {
            return Ok(value);
        }
    }
}

fn send(stream: &mut UnixStream, command: &Value) -> Result<(), String> {
    let mut line = command.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

fn error_text(reply: &Value) -> Option<String> {
    reply.get("error").map(|e| {
        e.get("desc")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string()
    })
}

/// Speaks QMP on `stream`: saves the guest's state as [`STATE_TAG`], then tells
/// QEMU to quit and waits for it to close the connection (i.e. to be gone).
///
/// `savevm` has no native QMP command that's both stable and simple, so it goes
/// through the human monitor, whose reply is the text it printed: empty on
/// success, the error message otherwise.
pub fn qmp_save_and_quit(stream: UnixStream) -> Result<(), String> {
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;

    // Greeting, then leave capability negotiation mode.
    read_reply_or_greeting(&mut reader)?;
    send(&mut writer, &json!({"execute": "qmp_capabilities"}))?;
    let reply = read_reply(&mut reader)?;
    if let Some(err) = error_text(&reply) {
        return Err(err);
    }

    reader
        .get_ref()
        .set_read_timeout(Some(SAVE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    send(
        &mut writer,
        &json!({
            "execute": "human-monitor-command",
            "arguments": {"command-line": format!("savevm {STATE_TAG}")}
        }),
    )?;
    let reply = read_reply(&mut reader)?;
    if let Some(err) = error_text(&reply) {
        return Err(err);
    }
    let printed = reply["return"].as_str().unwrap_or("").trim();
    if !printed.is_empty() {
        return Err(printed.to_string());
    }

    // The state is on disk. Quit, then wait for the connection to drop.
    send(&mut writer, &json!({"execute": "quit"}))?;
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            // QEMU may reset the connection instead of closing it cleanly as
            // it goes away; either way it is gone.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(format!("qemu did not exit after saving: {e}")),
        }
    }
}

/// QMP sends a `{"QMP": ...}` greeting first; it has neither `return` nor
/// `error`, so read exactly one line for it.
fn read_reply_or_greeting(reader: &mut BufReader<UnixStream>) -> Result<(), String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Err("the socket was closed".into()),
        Ok(_) if line.contains("QMP") => Ok(()),
        Ok(_) => Err(format!("unexpected greeting: {}", line.trim())),
        Err(e) => Err(e.to_string()),
    }
}

/// Connects to the monitor socket at `path`, retrying briefly since QEMU
/// creates it a moment after starting.
fn connect_with_retries(path: &Path) -> Result<UnixStream, String> {
    let mut last = String::new();
    for _ in 0..50 {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("connecting to {}: {last}", path.display()))
}

/// Saves the state of the QEMU listening on the QMP socket `path` and makes it
/// quit. Blocks for as long as the save takes; call from a helper thread.
pub fn save_state_over_socket(path: &Path) -> Result<(), String> {
    qmp_save_and_quit(connect_with_retries(path)?)
}

/// Sets the guest's clock to the host's through the QEMU guest agent on the
/// socket at `path`. A guest restored from saved state otherwise runs on the
/// time it was saved at, which breaks TLS and anything else that checks dates.
///
/// Best effort by nature: it needs `qemu-guest-agent` running in the guest.
pub fn guest_set_time(path: &Path) -> Result<(), String> {
    let stream =
        UnixStream::connect(path).map_err(|e| format!("connecting to {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    // The agent may have buffered garbage from before; `guest-sync` with a
    // fresh id makes it echo the id back, after which replies are in step.
    let sync_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(1);
    send(
        &mut writer,
        &json!({"execute": "guest-sync", "arguments": {"id": sync_id}}),
    )?;
    loop {
        let reply =
            read_reply(&mut reader).map_err(|e| format!("guest agent did not answer: {e}"))?;
        if reply["return"].as_i64() == Some(sync_id) {
            break;
        }
    }

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    send(
        &mut writer,
        &json!({"execute": "guest-set-time", "arguments": {"time": now_ns}}),
    )?;
    let reply = read_reply(&mut reader).map_err(|e| format!("guest agent did not answer: {e}"))?;
    match error_text(&reply) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ironland-vm-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settings() -> VmSettings {
        VmSettings::default()
    }

    #[test]
    fn keys_are_safe_directory_names() {
        assert_eq!(
            persist_key("org.mozilla.firefox.desktop"),
            "org.mozilla.firefox.desktop"
        );
        assert_eq!(persist_key("a/b c"), "a_b_c");
        assert_eq!(persist_key(".."), "_..");
        assert_eq!(persist_key(""), "_");
        assert!(!persist_key("../../etc").contains('/'));
    }

    #[test]
    fn state_root_honours_the_config_and_xdg_data_home() {
        let mut s = settings();
        s.state_dir = Some("/srv/vms".into());
        assert_eq!(state_root(&s), PathBuf::from("/srv/vms"));
        s.state_dir = Some("  ".into());
        assert!(state_root(&s).ends_with("ironland-compositor/vm/persistent"));
    }

    #[test]
    fn machine_changes_alter_the_fingerprint() {
        let base = machine_fingerprint(&settings());
        let mut more_ram = settings();
        more_ram.memory_mb += 1;
        let mut extra = settings();
        extra.extra_qemu_args = vec!["-x".into()];
        assert_ne!(base, machine_fingerprint(&more_ram));
        assert_ne!(base, machine_fingerprint(&extra));
        // The ssh port, user, key and the like don't touch the machine.
        let mut other = settings();
        other.user = "dev".into();
        other.ssh_key = Some("/k".into());
        assert_eq!(base, machine_fingerprint(&other));
    }

    #[test]
    fn image_format_is_read_from_the_magic_number() {
        let dir = scratch("format");
        fs::write(dir.join("a.qcow2"), b"QFI\xfb rest of header").unwrap();
        fs::write(dir.join("b.img"), b"\x00\x00\x00\x00 raw disk").unwrap();
        fs::write(dir.join("c.img"), b"").unwrap();
        assert_eq!(image_format(&dir.join("a.qcow2")).unwrap(), "qcow2");
        assert_eq!(image_format(&dir.join("b.img")).unwrap(), "raw");
        assert_eq!(image_format(&dir.join("c.img")).unwrap(), "raw");
        fs::remove_dir_all(dir).unwrap();
    }

    fn have_qemu_img() -> bool {
        Command::new("qemu-img").arg("--version").output().is_ok()
    }

    #[test]
    fn prepare_creates_an_overlay_on_first_use_and_reuses_it_afterwards() {
        if !have_qemu_img() {
            return;
        }
        let dir = scratch("prepare");
        let image = dir.join("base.qcow2");
        assert!(
            Command::new("qemu-img")
                .args(["create", "-q", "-f", "qcow2"])
                .arg(&image)
                .arg("1M")
                .status()
                .unwrap()
                .success()
        );
        let vm_dir = dir.join("app");

        let first = prepare(&vm_dir, &image, &settings()).unwrap();
        assert!(first.overlay.exists());
        assert!(!first.load_state);
        assert!(!first.meta.has_state);

        // Nothing saved yet -> still a cold boot, same overlay.
        let again = prepare(&vm_dir, &image, &settings()).unwrap();
        assert!(!again.load_state);

        // Once a state is recorded it is loaded, but only for the same machine.
        mark_saved(&again.meta, &again.meta_path, &settings()).unwrap();
        assert!(prepare(&vm_dir, &image, &settings()).unwrap().load_state);
        let mut bigger = settings();
        bigger.memory_mb *= 2;
        let changed = prepare(&vm_dir, &image, &bigger).unwrap();
        assert!(
            !changed.load_state,
            "a different machine can't load the state"
        );
        assert_eq!(changed.overlay, first.overlay, "the disk is kept");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_replaced_base_image_moves_the_old_state_aside_instead_of_deleting_it() {
        if !have_qemu_img() {
            return;
        }
        let dir = scratch("stale");
        let image = dir.join("base.qcow2");
        let make = |size: &str| {
            assert!(
                Command::new("qemu-img")
                    .args(["create", "-q", "-f", "qcow2"])
                    .arg(&image)
                    .arg(size)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        make("1M");
        let vm_dir = dir.join("app");
        let first = prepare(&vm_dir, &image, &settings()).unwrap();
        fs::write(vm_dir.join("precious"), b"user data").unwrap();
        mark_saved(&first.meta, &first.meta_path, &settings()).unwrap();

        make("2M"); // "rebuilt" guest: different size
        let second = prepare(&vm_dir, &image, &settings()).unwrap();
        assert!(!second.load_state);
        assert!(!vm_dir.join("precious").exists());
        let moved: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("app.stale-"))
            .collect();
        assert_eq!(moved.len(), 1);
        assert!(moved[0].path().join("precious").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// A fake QEMU monitor: plays the server half of `qmp_save_and_quit`,
    /// recording the commands it received. `save_reply` is the text the
    /// human monitor "printed" for `savevm`.
    fn fake_qmp(
        server: UnixStream,
        save_reply: &'static str,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut writer = server.try_clone().unwrap();
            let mut reader = BufReader::new(server);
            let mut seen = Vec::new();
            writer
                .write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\n")
                .unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return seen;
                }
                let cmd: Value = serde_json::from_str(&line).unwrap();
                let name = cmd["execute"].as_str().unwrap().to_string();
                seen.push(match cmd["arguments"]["command-line"].as_str() {
                    Some(hmp) => format!("{name}:{hmp}"),
                    None => name.clone(),
                });
                match name.as_str() {
                    "quit" => {
                        // An event, as real QEMU sends one, then hang up.
                        writer
                            .write_all(b"{\"return\": {}}\n{\"event\": \"SHUTDOWN\"}\n")
                            .unwrap();
                        return seen;
                    }
                    "human-monitor-command" => {
                        // Events may precede the reply.
                        writer.write_all(b"{\"event\": \"STOP\"}\n").unwrap();
                        let reply = json!({"return": save_reply});
                        writer.write_all(format!("{reply}\n").as_bytes()).unwrap();
                    }
                    _ => writer.write_all(b"{\"return\": {}}\n").unwrap(),
                }
            }
        })
    }

    #[test]
    fn qmp_save_negotiates_saves_then_quits() {
        let (client, server) = UnixStream::pair().unwrap();
        let qemu = fake_qmp(server, "");
        qmp_save_and_quit(client).unwrap();
        assert_eq!(
            qemu.join().unwrap(),
            [
                "qmp_capabilities",
                "human-monitor-command:savevm ironland",
                "quit"
            ]
        );
    }

    #[test]
    fn a_failed_savevm_reports_qemus_message_and_does_not_quit() {
        let (client, server) = UnixStream::pair().unwrap();
        let qemu = fake_qmp(server, "Error: no block device can store vmstate");
        let err = qmp_save_and_quit(client).unwrap_err();
        assert!(err.contains("no block device"), "{err}");
        // The client hung up without sending `quit`.
        assert_eq!(
            qemu.join().unwrap().last().unwrap(),
            "human-monitor-command:savevm ironland"
        );
    }

    #[test]
    fn guest_set_time_syncs_then_sets_the_time() {
        let dir = scratch("qga");
        let path = dir.join("qga.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let agent = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut seen = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let cmd: Value = serde_json::from_str(&line).unwrap();
                seen.push(cmd.clone());
                let reply = match cmd["execute"].as_str().unwrap() {
                    "guest-sync" => json!({"return": cmd["arguments"]["id"]}),
                    _ => json!({"return": {}}),
                };
                writer.write_all(format!("{reply}\n").as_bytes()).unwrap();
            }
            seen
        });
        guest_set_time(&path).unwrap();
        let seen = agent.join().unwrap();
        assert_eq!(seen[0]["execute"], "guest-sync");
        assert_eq!(seen[1]["execute"], "guest-set-time");
        assert!(seen[1]["arguments"]["time"].as_i64().unwrap() > 1_700_000_000_000_000_000);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn guest_set_time_reports_a_silent_agent() {
        let dir = scratch("qga-silent");
        let path = dir.join("qga.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // Accepts and never answers: what a guest without the agent looks like
        // (QEMU's side of the channel is open, nobody is listening on it).
        let silent = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            std::thread::sleep(Duration::from_millis(200));
        });
        // Shorten the wait: the test only needs *an* error.
        let started = std::time::Instant::now();
        let err = guest_set_time(&path).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(6));
        silent.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
