//! Self-update: download this platform's asset of a newer release (as release.yml names them) and swap it in
//! for the running install. The new build runs after a restart.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// End of this platform's release asset name: `-{target}.{ext}`, the AppImage when running from one.
pub fn asset_suffix(appimage: bool) -> String {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    };
    let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { std::env::consts::ARCH };
    let ext = if appimage { "AppImage" } else if os == "windows" { "zip" } else { "tar.gz" };
    format!("-{os}-{arch}.{ext}")
}

/// Download URL of the asset whose name ends with `suffix` in a GitHub release JSON; only `repo`'s own release downloads count.
pub fn asset_url(release: &[u8], repo: &str, suffix: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct Rel {
        #[serde(default)]
        assets: Vec<Asset>,
    }
    let prefix = format!("https://github.com/{repo}/releases/download/");
    let rel: Rel = serde_json::from_slice(release).ok()?;
    rel.assets.into_iter().find(|a| a.name.starts_with("stremflix-rs-") && a.name.ends_with(suffix) && a.browser_download_url.starts_with(&prefix) && !a.browser_download_url.contains("..")).map(|a| a.browser_download_url)
}

/// What an update replaces: (name inside the archive's top folder, installed path). An empty name means the
/// download itself (an AppImage) is the new file.
pub fn plan(exe: &Path, appimage: Option<&Path>) -> Result<Vec<(&'static str, PathBuf)>> {
    if let Some(a) = appimage {
        return Ok(vec![("", a.to_path_buf())]);
    }
    let dir = exe.parent().context("no install folder")?;
    Ok(match std::env::consts::OS {
        "windows" => vec![("stremflix-rs.exe", exe.to_path_buf()), ("libmpv-2.dll", dir.join("libmpv-2.dll"))],
        "macos" => {
            // …/Name.app/Contents/MacOS/stremflix-rs: the whole bundle is replaced, so its signature stays sealed.
            let app = exe.ancestors().nth(3).filter(|a| a.extension().is_some_and(|e| e == "app")).context("not running from a release .app bundle")?;
            vec![("Stremflix-rs.app", app.to_path_buf())]
        }
        _ => vec![("stremflix-rs", exe.to_path_buf())],
    })
}

fn old(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

fn remove(p: &Path) -> std::io::Result<()> {
    if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

/// Put `new` at `installed`, keeping the replaced copy as `<installed>.old`.
fn swap(new: &Path, installed: &Path) -> Result<()> {
    let prev = old(installed);
    let _ = remove(&prev);
    if installed.exists() {
        std::fs::rename(installed, &prev).with_context(|| format!("move {} aside", installed.display()))?;
    }
    std::fs::rename(new, installed).map_err(|e| {
        let _ = std::fs::rename(&prev, installed);
        anyhow::Error::new(e).context(format!("replace {}", installed.display()))
    })
}

/// Swap every `(new, installed)` pair in, or none: a failure puts the already swapped ones back.
/// The `.old` copies are then removed where possible (Windows can't delete a running exe or loaded dll, only rename it).
fn swap_all(pairs: &[(PathBuf, &Path)]) -> Result<()> {
    for (i, (new, installed)) in pairs.iter().enumerate() {
        if let Err(e) = swap(new, installed) {
            for (_, done) in &pairs[..i] {
                let _ = remove(done);
                let _ = std::fs::rename(old(done), done);
            }
            return Err(e);
        }
    }
    for (_, installed) in pairs {
        let _ = remove(&old(installed));
    }
    Ok(())
}

/// The AppImage this build runs from: `APPIMAGE` alone could be inherited from another AppImage that launched us.
pub fn appimage() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDIR")?;
    std::env::current_exe().ok()?.starts_with(dir).then(|| std::env::var_os("APPIMAGE").map(PathBuf::from)).flatten()
}

/// Remove `.old` copies a previous update on Windows couldn't delete while they ran.
pub fn cleanup() {
    let Ok(exe) = std::env::current_exe() else { return };
    if crate::addon::RELEASE.is_none() {
        return;
    }
    for (_, p) in plan(&exe, appimage().as_deref()).unwrap_or_default() {
        let _ = remove(&old(&p));
    }
}

/// Windows' own bsdtar: a GNU tar earlier on PATH (Git for Windows) reads `C:` as a remote host and can't unzip.
fn tar() -> PathBuf {
    match std::env::var_os("SystemRoot") {
        Some(r) if cfg!(windows) => Path::new(&r).join(r"System32\tar.exe"),
        _ => "tar".into(),
    }
}

/// Unpack the downloaded asset `file` inside `work` and swap each `plan` entry in.
pub async fn apply(file: &Path, work: &Path, plan: &[(&str, PathBuf)]) -> Result<()> {
    if let [("", installed)] = plan {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))?;
        }
        return swap_all(&[(file.to_path_buf(), installed)]);
    }
    let out = work.join("x");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    // The system tar (bsdtar on Windows and macOS) reads both .tar.gz and .zip and refuses `..` and absolute entries.
    let st = tokio::process::Command::new(tar()).arg("-xf").arg(file).arg("-C").arg(&out).status().await.context("run tar")?;
    ensure!(st.success(), "tar failed to unpack the update ({st})");
    // Archives hold one folder, `stremflix-rs-<tag>-<target>` (macOS tar may add `._` metadata entries beside it).
    let top: Vec<PathBuf> = std::fs::read_dir(&out)?.filter_map(|e| Some(e.ok()?.path())).filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("stremflix-rs-"))).collect();
    let [top] = top.as_slice() else { bail!("unexpected update archive layout") };
    for (name, _) in plan {
        ensure!(top.join(name).exists(), "update archive has no {name}");
    }
    swap_all(&plan.iter().map(|(name, installed)| (top.join(name), installed.as_path())).collect::<Vec<_>>())
}

/// Download the release asset at `url` and install it over the running build.
pub async fn install(url: &str) -> Result<()> {
    let plan = plan(&std::env::current_exe()?, appimage().as_deref())?;
    // Work next to the install, so the final renames stay on one file system.
    let work = plan[0].1.parent().context("no install folder")?.join(".stremflix-rs-update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let file = work.join("download");
    let (_keep, mut cancel) = tokio::sync::oneshot::channel();
    let res = async {
        crate::download::fetch(url, &[], &file, ("", 0), &mut cancel, |_, _, _| {}).await?;
        apply(&file, &work, &plan).await
    }
    .await;
    let _ = std::fs::remove_dir_all(&work);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_pick() {
        let rel = br#"{"assets":[
            {"name":"stremflix-rs-v2-linux-x86_64.tar.gz","browser_download_url":"https://evil.example/releases/download/v2/a.tar.gz"},
            {"name":"stremflix-rs-v2-linux-x86_64.AppImage","browser_download_url":"https://github.com/o/r/releases/download/v2/b.AppImage"},
            {"name":"stremflix-rs-v2-linux-x86_64.tar.gz","browser_download_url":"https://github.com/o/r/releases/download/v2/c.tar.gz"}]}"#;
        assert_eq!(asset_url(rel, "o/r", "-linux-x86_64.tar.gz").as_deref(), Some("https://github.com/o/r/releases/download/v2/c.tar.gz"));
        assert_eq!(asset_url(rel, "o/r", "-linux-x86_64.AppImage").as_deref(), Some("https://github.com/o/r/releases/download/v2/b.AppImage"));
        assert_eq!(asset_url(rel, "o/r", "-windows-x86_64.zip"), None);
        assert_eq!(asset_url(b"{}", "o/r", "x"), None);
        assert!(asset_suffix(true).ends_with(".AppImage"));
    }

    #[tokio::test]
    async fn apply_replaces_install() {
        let d = tempfile::tempdir().unwrap();
        let (work, inst) = (d.path().join("work"), d.path().join("inst"));
        let src = work.join("src/stremflix-rs-v2-test");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&inst).unwrap();
        let file = work.join("download");
        let pack = |files: &[(&str, &str)]| {
            for (n, body) in files {
                std::fs::write(src.join(n), body).unwrap();
            }
            assert!(std::process::Command::new(tar()).arg("-czf").arg(&file).arg("-C").arg(work.join("src")).arg("stremflix-rs-v2-test").status().unwrap().success());
        };
        let read = |n: &str| std::fs::read_to_string(inst.join(n)).unwrap();
        std::fs::write(inst.join("bin"), "old").unwrap();
        pack(&[("bin", "new")]);
        apply(&file, &work, &[("bin", inst.join("bin"))]).await.unwrap();
        assert_eq!(read("bin"), "new");
        assert!(!old(&inst.join("bin")).exists());
        // a missing entry fails before anything is replaced
        pack(&[("bin", "next")]);
        assert!(apply(&file, &work, &[("bin", inst.join("bin")), ("nope", inst.join("nope"))]).await.is_err());
        assert_eq!(read("bin"), "new");
        // a failed second swap (its folder is a file) puts the first back
        std::fs::write(d.path().join("blocker"), "").unwrap();
        pack(&[("b2", "")]);
        assert!(apply(&file, &work, &[("bin", inst.join("bin")), ("b2", d.path().join("blocker/b2"))]).await.is_err());
        assert_eq!(read("bin"), "new");
        // an AppImage is the download itself
        std::fs::write(work.join("img"), "img2").unwrap();
        apply(&work.join("img"), &work, &[("", inst.join("app"))]).await.unwrap();
        assert_eq!(read("app"), "img2");
    }

    #[test]
    fn plan_appimage() {
        let p = plan(Path::new("/tmp/x/stremflix-rs"), Some(Path::new("/home/u/S.AppImage"))).unwrap();
        assert_eq!(p, vec![("", PathBuf::from("/home/u/S.AppImage"))]);
    }
}
