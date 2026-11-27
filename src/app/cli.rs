//! The command line and the inputs it, a drop or a paste can open: links, magnets, `.torrent` and media files.

use anyhow::Context;
use stremflix_rs::addon::{parse_magnet_hash, sanitize_text};
use stremflix_rs::player::command_words;

/// A local `.torrent` file path (not a URL or magnet).
pub(crate) fn is_torrent_file(s: &str) -> bool {
    !s.contains("://") && !s.starts_with("magnet:") && parse_magnet_hash(s).is_none() && std::path::Path::new(s).extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

/// Played through the torrent engine: a valid magnet (any scheme case, padding trimmed like `Url::parse`) or a `.torrent` file.
pub(crate) fn is_torrent_source(s: &str) -> bool {
    parse_magnet_hash(s).is_some() || is_torrent_file(s)
}

/// What to play or open from a magnet, `stremio://` link or local file: valid magnet, or an existing file.
pub(crate) fn is_open_input(s: &str) -> bool {
    parse_magnet_hash(s).is_some() || (!s.contains("://") && std::path::Path::new(s).is_file())
}

pub(crate) const USAGE: &str = "usage: stremflix-rs [--help] [<stremio:// link | magnet link | .torrent file | media file>]";

pub(crate) enum Cli {
    /// Optional link to hand to the (running or new) window; files are made absolute.
    Open(Option<String>),
    Help,
    Bad(String),
}

/// Parse the command line (without argv[0]). Reads the filesystem only to check that a file exists.
pub(crate) fn parse_cli(args: &[String]) -> Cli {
    // macOS Finder adds `-psn_…`.
    // Only before `--`: after it, every argument is a file name.
    let mut seen_end = false;
    let mut args = args.iter().filter(|a| {
        let drop = !seen_end && a.starts_with("-psn_");
        seen_end |= a.as_str() == "--";
        !drop
    });
    let Some(a) = args.next() else { return Cli::Open(None) };
    if matches!(a.as_str(), "-h" | "--help") {
        return Cli::Help;
    }
    // `--` ends the flags so a file named `-movie.mkv` can be opened.
    let end_of_flags = a == "--";
    let Some(a) = (if end_of_flags { args.next() } else { Some(a) }) else { return Cli::Open(None) };
    if args.next().is_some() {
        return Cli::Bad("only one argument is accepted".into());
    }
    if a.starts_with('-') && !end_of_flags {
        return Cli::Bad(format!("unknown flag: {}", sanitize_text(a, 100)));
    }
    if a.get(..10).is_some_and(|p| p.eq_ignore_ascii_case("stremio://")) {
        return Cli::Open(Some(a.clone()));
    }
    if parse_magnet_hash(a).is_some() {
        return Cli::Open(Some(a.clone()));
    }
    if a.contains("://") || a.trim_start().get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:")) {
        return Cli::Bad(format!("unsupported link: {}", sanitize_text(a, 100)));
    }
    match std::path::absolute(a) {
        Ok(p) if p.is_file() => match p.to_str() {
            Some(p) => Cli::Open(Some(p.into())),
            None => Cli::Bad("file path is not valid UTF-8".into()),
        },
        _ => Cli::Bad(format!("no such file: {}", sanitize_text(a, 100))),
    }
}

/// Start `command` (program and its arguments) with `url` as the last argument. No shell is involved.
/// A command that is an existing file is the program alone, so an unquoted path with spaces works.
pub(crate) fn open_external(command: &str, url: &str) -> anyhow::Result<()> {
    let command = command.trim();
    let words = if std::path::Path::new(command).is_file() { vec![command.to_string()] } else { command_words(command) };
    let mut words = words.iter();
    let prog = words.next().context("set an external player in Settings")?;
    let mut child = std::process::Command::new(prog).args(words).arg(url).spawn().with_context(|| format!("start {prog}"))?;
    // Reap it when it exits, so it never lingers as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_args() {
        let cli = |a: &[&str]| parse_cli(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let magnet = format!("magnet:?xt=urn:btih:{}", "a".repeat(40));
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert!(matches!(cli(&[]), Cli::Open(None)) && matches!(cli(&["-psn_0_1"]), Cli::Open(None)));
        assert!(matches!(cli(&["--help"]), Cli::Help));
        assert!(matches!(cli(&["stremio:///detail/movie/tt1"]), Cli::Open(Some(_))));
        assert!(matches!(cli(&["-h"]), Cli::Help));
        assert!(matches!(cli(&["--", "-psn_0_1"]), Cli::Bad(m) if m.starts_with("no such file")));
        assert!(matches!(cli(&["--", &magnet]), Cli::Open(Some(m)) if m == magnet));
        assert!(matches!(cli(&["--"]), Cli::Open(None)) && matches!(cli(&["--", "-no-such.mkv"]), Cli::Bad(m) if m.starts_with("no such file")));
        assert!(matches!(cli(&["MAGNET:?xt=bad"]), Cli::Bad(m) if m.starts_with("unsupported link")));
        assert!(matches!(cli(&[&magnet]), Cli::Open(Some(m)) if m == magnet));
        assert!(matches!(cli(&[file]), Cli::Open(Some(p)) if std::path::Path::new(&p) == std::path::absolute(file).unwrap()));
        // Url::parse accepts an upper-case scheme; Msg::Link routing uses the same predicate.
        let upper = magnet.replacen("magnet", "MAGNET", 1);
        assert!(matches!(cli(&[&upper]), Cli::Open(Some(_))) && parse_magnet_hash(&upper).is_some());
        for bad in [&["--nope"][..], &["magnet:?xt=bad"], &["https://x.example/a.torrent"], &["/no/such.mkv"], &[file, file]] {
            assert!(matches!(cli(bad), Cli::Bad(_)), "{bad:?}");
        }
    }

    #[test]
    fn torrent_source_routing() {
        let m = format!("magnet:?xt=urn:btih:{}", "a".repeat(40));
        for yes in [m.clone(), m.replacen("magnet", "MAGNET", 1), format!("  {m}"), "/m/a.torrent".into()] {
            assert!(is_torrent_source(&yes), "{yes}");
        }
        for no in ["magnet:?xt=bad", "/m/a.mkv", "https://x.example/a.torrent"] {
            assert!(!is_torrent_source(no), "{no}");
        }
    }

    #[test]
    fn torrent_file_detection() {
        assert!(is_torrent_file("/m/a.torrent") && is_torrent_file(r"C:\m\A.TORRENT"));
        for not in ["https://x.example/a.torrent", "magnet:?xt=urn:btih:a.torrent", "/m/a.mkv", "/m/torrent"] {
            assert!(!is_torrent_file(not), "{not}");
        }
    }
}
