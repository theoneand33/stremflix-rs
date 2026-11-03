//! One window per data directory: the first instance listens on loopback, and later launches
//! (with or without a `stremio://` link) hand it over and exit. The port and a random token sit in a file
//! only this user can read, so other local users can't push links.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

const FILE: &str = "instance";
const MAX_LINE: u64 = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(2);

/// Hand `link` to a running instance; true if it took it.
pub fn forward(dir: &Path, link: &str) -> bool {
    let Ok(s) = std::fs::read_to_string(dir.join(FILE)) else { return false };
    let Some((port, token)) = s.trim().split_once(' ') else { return false };
    let Ok(port) = port.parse::<u16>() else { return false };
    let Ok(mut c) = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), TIMEOUT) else { return false };
    let _ = c.set_read_timeout(Some(TIMEOUT));
    let mut reply = String::new();
    // A stale file may point at another program's port: only the instance answers "ok".
    writeln!(c, "{token} {}", link.replace(['\r', '\n'], "")).is_ok() && (&c).take(8).read_to_string(&mut reply).is_ok() && reply == "ok\n"
}

/// Accept links from later launches on a background thread; each goes to `on_link`.
pub fn listen(dir: &Path, on_link: impl Fn(String) + Send + 'static) -> std::io::Result<()> {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let token = token();
    write_private(&dir.join(FILE), &format!("{} {token}", l.local_addr()?.port()))?;
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let _ = c.set_read_timeout(Some(TIMEOUT));
            let mut line = String::new();
            if BufReader::new((&c).take(MAX_LINE)).read_line(&mut line).is_err() {
                continue;
            }
            if let Some(link) = line.trim_end_matches(['\r', '\n']).strip_prefix(token.as_str()).and_then(|r| r.strip_prefix(' ')) {
                on_link(link.to_owned());
                let _ = (&c).write_all(b"ok\n");
            }
        }
    });
    Ok(())
}

/// Unpredictable token from std's OS-seeded SipHash keys (no RNG dependency).
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let r = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", r(), r())
}

fn write_private(path: &Path, s: &str) -> std::io::Result<()> {
    // Publish by rename so a concurrent `forward` never reads a half-written file.
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    // `mode` only applies to a new file: tighten one left over with looser permissions.
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(s.as_bytes())?;
    std::fs::rename(tmp, path)
}

/// Make this exe the `stremio://` handler for the current user, unless another app (Stremio) owns it.
#[cfg(windows)]
pub fn register_scheme() {
    use std::os::windows::process::CommandExt;
    const NO_WINDOW: u32 = 0x0800_0000;
    let reg = |args: &[&str]| std::process::Command::new("reg").args(args).creation_flags(NO_WINDOW).output();
    let Ok(exe) = std::env::current_exe() else { return };
    let cmd = format!("\"{}\" \"%1\"", exe.display());
    // HKCR merges the machine-wide and per-user keys.
    if let Ok(out) = reg(&["query", r"HKCR\stremio\shell\open\command", "/ve"]) {
        let cur = String::from_utf8_lossy(&out.stdout);
        if out.status.success() && (cur.contains(&cmd) || !cur.contains("stremflix-rs")) {
            return;
        }
    }
    let key = r"HKCU\Software\Classes\stremio";
    for args in [
        &["add", key, "/ve", "/d", "URL:Stremio", "/f"][..],
        &["add", key, "/v", "URL Protocol", "/d", "", "/f"],
        &["add", &format!(r"{key}\shell\open\command"), "/ve", "/d", &cmd, "/f"],
    ] {
        if !reg(args).is_ok_and(|o| o.status.success()) {
            eprintln!("error: registering stremio:// links failed");
            return;
        }
    }
}
