//! Log file (rotated) and a secret-free diagnostics bundle.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAX_LOG: u64 = 512 * 1024;
static LOG: OnceLock<PathBuf> = OnceLock::new();

pub fn init(dir: &Path) {
    let _ = LOG.set(dir.join("stremflix-rs.log"));
}

/// Writes to stderr and, once `init` ran, the log file. Use via `elog!`.
pub fn log(msg: &str) {
    eprintln!("{msg}");
    let Some(p) = LOG.get() else { return };
    // Keep one previous file so the total stays under 2x MAX_LOG.
    if std::fs::metadata(p).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = std::fs::rename(p, p.with_extension("log.1"));
    }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    if let Ok(mut f) = o.open(p) {
        let _ = writeln!(f, "[{t}] {}", redact(msg));
    }
}

#[macro_export]
macro_rules! elog {
    ($($a:tt)*) => { $crate::diag::log(&format!($($a)*)) };
}

/// Keeps only scheme://host of URLs: userinfo, path (addon configs embed tokens there), query and fragment go.
/// Magnet links (info hashes, trackers) are replaced whole.
pub fn redact(s: &str) -> String {
    s.split_inclusive(char::is_whitespace)
        .map(|w| {
            if let Some(i) = w.find("magnet:") {
                let end = w[i..].find(|c: char| c.is_whitespace() || "\"'<>)".contains(c)).map_or(w.len(), |e| i + e);
                return format!("{}magnet:<redacted>{}", &w[..i], &w[end..]);
            }
            let Some(i) = w.find("://") else { return w.to_string() };
            let start = w[..i].rfind(|c: char| !c.is_ascii_alphanumeric() && !"+-.".contains(c)).map_or(0, |p| p + 1);
            let rest = &w[i + 3..];
            let end = rest.find(|c: char| c.is_whitespace() || "\"'<>)".contains(c)).unwrap_or(rest.len());
            let (url, tail) = rest.split_at(end);
            let authority = url.split(['/', '?', '#']).next().unwrap_or("");
            let host = authority.rsplit('@').next().unwrap_or("");
            format!("{}://{host}/<redacted>{tail}", &w[start..i])
        })
        .collect()
}

/// Version, OS, caller's summary and the redacted tail of the log.
pub fn bundle(summary: &str) -> String {
    let mut out = format!("stremflix-rs {}\nos: {} {}\n{summary}\n\n--- recent log ---\n", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    if let Some(p) = LOG.get() {
        let log = std::fs::read(p).map(|b| String::from_utf8_lossy(&b[b.len().saturating_sub(64 * 1024)..]).into_owned()).unwrap_or_default();
        out.push_str(&redact(&log));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_paths_and_queries() {
        let r = redact("fetch https://user:pw@addon.example:7000/SECRETCFG/manifest.json?token=abc failed\nnext");
        assert_eq!(r, "fetch https://addon.example:7000/<redacted> failed\nnext");
        assert!(!r.contains("SECRET") && !r.contains("pw") && !r.contains("abc"));
        assert_eq!(redact("no urls here"), "no urls here");
        let m = redact("add magnet:?xt=urn:btih:ABC123&tr=x ok");
        assert_eq!(m, "add magnet:<redacted> ok");
        assert!(!m.contains("ABC123"));
    }
}
