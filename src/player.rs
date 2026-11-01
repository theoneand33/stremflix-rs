//! Playback backend: an external `mpv` process controlled over its JSON IPC socket.
//! `STREMIO_LITE_MPV` overrides the binary; `STREMIO_LITE_MPV_ARGS` adds args
//! (e.g. `--vo=null --ao=null` for headless CI).

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub struct Player {
    child: Child,
    socket: PathBuf,
}

/// Fixed, documented socket path so external tools can query player state.
pub fn ipc_socket_path() -> PathBuf {
    std::env::var_os("STREMIO_LITE_IPC")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("stremio-lite-mpv.sock"))
}

impl Player {
    /// `source` is a URL or local path; it is passed after `--` so it can't be parsed as an option.
    pub fn launch(source: &str, title: &str, start: f64, subtitles: &[String]) -> Result<Self> {
        let socket = ipc_socket_path();
        let _ = std::fs::remove_file(&socket);
        let bin = std::env::var("STREMIO_LITE_MPV").unwrap_or_else(|_| "mpv".into());
        let mut cmd = Command::new(bin);
        cmd.arg(format!("--input-ipc-server={}", socket.display()))
            .arg("--force-window=yes")
            .arg("--keep-open=yes")
            .arg("--idle=no")
            .arg("--no-terminal")
            .arg("--no-ytdl")
            .arg(format!("--force-media-title={title}"))
            .arg(format!("--start={start:.1}"));
        for s in subtitles {
            cmd.arg(format!("--sub-file={s}"));
        }
        if let Ok(extra) = std::env::var("STREMIO_LITE_MPV_ARGS") {
            cmd.args(extra.split_whitespace());
        }
        cmd.arg("--").arg(source).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = cmd.spawn().context("failed to start mpv (is it installed?)")?;
        Ok(Self { child, socket })
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn get_property(&self, name: &str) -> Result<serde_json::Value> {
        get_property_at(&self.socket, name)
    }

    /// Current playback position and duration in seconds, if known.
    pub fn position(&self) -> Option<(f64, f64)> {
        let pos = self.get_property("time-pos").ok()?.as_f64()?;
        let dur = self.get_property("duration").ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        Some((pos, dur))
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(unix)]
pub fn get_property_at(socket: &std::path::Path, name: &str) -> Result<serde_json::Value> {
    let mut s = std::os::unix::net::UnixStream::connect(socket).context("mpv IPC not available")?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    let req = serde_json::json!({"command": ["get_property", name], "request_id": 1});
    writeln!(s, "{req}")?;
    for line in BufReader::new(s).lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        if v.get("request_id").and_then(|r| r.as_i64()) == Some(1) {
            if v["error"] != "success" {
                bail!("mpv: {}", v["error"]);
            }
            return Ok(v["data"].clone());
        }
    }
    bail!("mpv IPC closed")
}

#[cfg(not(unix))]
pub fn get_property_at(_socket: &std::path::Path, _name: &str) -> Result<serde_json::Value> {
    bail!("mpv IPC is only implemented for Unix sockets")
}
