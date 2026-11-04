//! Downloads for offline playback: a stream (http URL, or the torrent engine's local stream URL) saved to a file.

use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::library::MEDIA_EXTS;

/// File name for `title` with no path separators, `..` or control characters, so addon text can't
/// escape the downloads folder. The extension comes from `hint` (a file name or URL path) if it's a media type.
pub fn file_name(title: &str, hint: Option<&str>) -> String {
    let mut stem: String = title.chars().map(|c| if c.is_alphanumeric() || " -_()[],'&".contains(c) { c } else { '_' }).collect();
    // Bytes, not chars: file systems cap names at 255 bytes, and non-ASCII takes up to 4 each.
    while stem.len() > 120 {
        stem.pop();
    }
    let stem = stem.trim();
    let stem = if stem.is_empty() { "download" } else { stem };
    // Windows reserves these device names even with an extension.
    const RESERVED: [&str; 22] = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    let stem = if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) { format!("_{stem}") } else { stem.to_string() };
    let ext = (hint.and_then(|h| Path::new(h).extension()?.to_str()).map(str::to_ascii_lowercase))
        .filter(|e| MEDIA_EXTS.contains(&e.as_str()))
        .unwrap_or_else(|| "mkv".into());
    format!("{stem}.{ext}")
}

/// Where a download is written until it completes.
pub fn part(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    p.into()
}

/// Claim a free path for `name` in `dir` (`name (2).ext`, ... if taken) by creating its `.part` file.
pub fn reserve(dir: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "mkv"));
    for n in 1..1000 {
        let path = dir.join(if n == 1 { name.to_string() } else { format!("{stem} ({n}).{ext}") });
        if path.exists() {
            continue;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(part(&path)) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", part(&path).display())),
        }
    }
    anyhow::bail!("no free file name for {name}")
}

/// Delete `.part` files left by downloads cut off when the app quit.
pub fn clear_partial(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "part") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Download `url` (with the addon's request `headers`) into `part(dest)`, then move it to `dest`.
pub async fn fetch(url: &str, headers: &[(String, String)], dest: &Path) -> Result<()> {
    // No total timeout: a movie can take hours. A stalled connection still fails.
    let client = reqwest::Client::builder().user_agent("stremflix-rs").connect_timeout(Duration::from_secs(15)).read_timeout(Duration::from_secs(120)).build()?;
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let mut resp = req.send().await.context("download")?.error_for_status()?;
    let kind = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    ensure!(!["mpegurl", "dash+xml"].iter().any(|t| kind.contains(t)), "{kind} is a streaming playlist, not a video file, so it can't be downloaded");
    ensure!(!kind.starts_with("text/html"), "the server sent a web page, not a video file");
    // A live or endless stream has no length; refusing it keeps a download from filling the disk.
    let Some(len) = resp.content_length() else { anyhow::bail!("the server gave no file size (likely a live stream), so it can't be downloaded") };
    ensure!(len > 0, "the server sent an empty file");
    let tmp = part(dest);
    let mut f = tokio::fs::File::create(&tmp).await?;
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        f.write_all(&chunk).await?;
        got += chunk.len() as u64;
    }
    ensure!(len == got, "download cut off at {got} of {len} bytes");
    f.sync_all().await?;
    finish(&tmp, dest)
}

/// Move `part` to `dest`, failing rather than overwriting a file created there since `reserve`.
fn finish(part: &Path, dest: &Path) -> Result<()> {
    match std::fs::hard_link(part, dest) {
        Ok(()) => {
            // The file is complete; a stale `.part` is cleared at the next start.
            let _ = std::fs::remove_file(part);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e).with_context(|| format!("{} already exists", dest.display())),
        // shortcut: no hard links on this file system (e.g. FAT), so check then rename; racy, but only against other apps writing our downloads folder.
        Err(_) => {
            ensure!(!dest.exists(), "{} already exists", dest.display());
            Ok(std::fs::rename(part, dest)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_stay_in_the_folder() {
        assert_eq!(file_name("Movie: The Sequel", Some("Movie.2024.1080p.MP4")), "Movie_ The Sequel.mp4");
        assert_eq!(file_name("../../etc/passwd", None), "______etc_passwd.mkv");
        assert_eq!(file_name("a\\b\u{202e}c\n", Some("/stream/0")), "a_b_c_.mkv");
        assert_eq!(file_name("  ", Some("x.exe")), "download.mkv", "blank title, non-media extension");
        assert_eq!(file_name("..", None), "__.mkv");
        assert_eq!(file_name(&"x".repeat(500), None).len(), 124);
        assert!(file_name(&"é".repeat(500), None).len() <= 124, "capped by bytes");
        assert_eq!(file_name("nul", None), "_nul.mkv");
    }

    #[test]
    fn reserve_picks_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = reserve(dir.path(), "A.mkv").unwrap();
        let b = reserve(dir.path(), "A.mkv").unwrap();
        assert_eq!((a.file_name().unwrap(), b.file_name().unwrap()), ("A.mkv".as_ref(), "A (2).mkv".as_ref()));
        assert!(part(&a).exists() && !a.exists());
        clear_partial(dir.path());
        assert!(!part(&a).exists() && !part(&b).exists());
    }

    #[test]
    fn finish_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        std::fs::write(part(&dest), b"new").unwrap();
        std::fs::write(&dest, b"theirs").unwrap();
        assert!(finish(&part(&dest), &dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"theirs");
        std::fs::remove_file(&dest).unwrap();
        finish(&part(&dest), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!part(&dest).exists());
    }

    #[tokio::test]
    async fn fetch_writes_the_file() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0; 1024]);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        fetch(&format!("http://{addr}/a.mp4"), &[], &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert!(!part(&dest).exists());
    }
}
