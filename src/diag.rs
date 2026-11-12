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
        #[cfg(unix)]
        let _ = std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o600));
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
            let Some(i) = w.find("://") else { return w.to_string() };
            let start = w[..i].char_indices().rev().find(|(_, c)| !c.is_ascii_alphanumeric() && !"+-.".contains(*c)).map_or(0, |(p, c)| p + c.len_utf8());
            let rest = &w[i + 3..];
            let end = rest.find(|c: char| c.is_whitespace() || "\"'<>)".contains(c)).unwrap_or(rest.len());
            let (url, tail) = rest.split_at(end);
            let authority = url.split(['/', '?', '#']).next().unwrap_or("");
            let host = authority.rsplit('@').next().unwrap_or("");
            format!("{}://{host}/<redacted>{tail}", &w[start..i])
        })
        .collect::<String>()
        .split_inclusive(char::is_whitespace)
        .map(|w| {
            // URLs went first so a magnet inside a URL path/query is already gone; ASCII lowercase keeps byte offsets.
            let Some(i) = w.to_ascii_lowercase().find("magnet:") else { return w.to_string() };
            let end = w[i..].find(|c: char| c.is_whitespace() || "\"'<>)".contains(c)).map_or(w.len(), |e| i + e);
            format!("{}magnet:<redacted>{}", &w[..i], &w[end..])
        })
        .collect()
}

/// Package version plus the release channel and commit a release build was made from: nightlies all share one package version.
pub fn version() -> String {
    build_label(env!("CARGO_PKG_VERSION"), crate::addon::RELEASE, crate::addon::COMMIT)
}

fn build_label(version: &str, release: Option<&str>, commit: Option<&str>) -> String {
    match (release, commit) {
        (Some(r), Some(c)) => format!("{version} ({r} {})", c.get(..7).unwrap_or(c)),
        _ => format!("{version} (source build)"),
    }
}

/// Version, OS, caller's summary and the redacted tail of the log.
pub fn bundle(summary: &str) -> String {
    let mut out = format!("stremflix-rs {}\nos: {} {}\n{summary}\n\n--- recent log ---\n", version(), std::env::consts::OS, std::env::consts::ARCH);
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
    fn version_names_the_release_build() {
        assert_eq!(build_label("0.1.0", Some("nightly"), Some("0123456789abcdef")), "0.1.0 (nightly 0123456)");
        assert_eq!(build_label("0.1.0", Some("v1.2.0"), Some("abc")), "0.1.0 (v1.2.0 abc)");
        assert_eq!(build_label("0.1.0", None, None), "0.1.0 (source build)");
        assert!(bundle("x").starts_with(&format!("stremflix-rs {}\n", version())));
    }

    #[test]
    fn redacts_credentials_paths_and_queries() {
        let r = redact("fetch https://user:pw@addon.example:7000/SECRETCFG/manifest.json?token=abc failed\nnext");
        assert_eq!(r, "fetch https://addon.example:7000/<redacted> failed\nnext");
        assert!(!r.contains("SECRET") && !r.contains("pw") && !r.contains("abc"));
        assert_eq!(redact("no urls here"), "no urls here");
        let m = redact("add magnet:?xt=urn:btih:ABC123&tr=x ok");
        assert_eq!(m, "add magnet:<redacted> ok");
        assert!(!m.contains("ABC123"));
        let u = redact("“https://a.example/SECRET” 日本https://b.example/TOKEN");
        assert_eq!(u, "https://a.example/<redacted> https://b.example/<redacted>");
        assert!(!u.contains("SECRET") && !u.contains("TOKEN"));
        let n = redact("GET https://host/SECRET?u=magnet:?xt=urn:btih:ABC123 x MAGNET:?xt=urn:btih:DEF456");
        assert_eq!(n, "GET https://host/<redacted> x magnet:<redacted>");
        assert!(!redact("日本magnet:?xt=urn:btih:ABC123").contains("ABC123"));
    }
}
