//! Household profiles: each has a data folder of its own, `<data>/profiles/<name>`, so addons, library,
//! progress, settings and downloads never mix. The default profile (`""`) is the data folder itself.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A profile name as typed: trimmed, 1 to 32 letters, digits, spaces, `-` or `_`.
/// It becomes a folder name, so nothing else passes (no separators, no dots, no Windows device names).
pub fn valid_name(s: &str) -> Option<&str> {
    let s = s.trim();
    let u = s.to_ascii_uppercase();
    let device = ["CON", "PRN", "AUX", "NUL"].contains(&u.as_str()) || u.len() == 4 && (u.starts_with("COM") || u.starts_with("LPT")) && u.ends_with(|c: char| c.is_ascii_digit());
    (!device && !s.is_empty() && s.chars().count() <= 32 && s.chars().all(|c| c.is_alphanumeric() || " -_".contains(c))).then_some(s)
}

pub fn dir(root: &Path, name: &str) -> PathBuf {
    if name.is_empty() { root.into() } else { root.join("profiles").join(name) }
}

/// The profile to open: the one named in `<data>/profile` while its folder exists, else the default.
pub fn active(root: &Path) -> String {
    let name = std::fs::read_to_string(root.join("profile")).unwrap_or_default();
    valid_name(&name).filter(|n| dir(root, n).is_dir()).unwrap_or_default().into()
}

/// Names of the profiles besides the default, sorted.
pub fn list(root: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(root.join("profiles")).into_iter().flatten().flatten();
    let mut names: Vec<String> = entries.filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()).filter(|n| valid_name(n) == Some(n)).collect();
    names.sort();
    names
}

/// Make `name` the profile the next launch opens, creating its folder; `""` is the default profile.
pub fn switch(root: &Path, name: &str) -> Result<()> {
    if !name.is_empty() {
        let name = valid_name(name).filter(|n| *n == name).context("profile names take up to 32 letters, digits, spaces, - or _")?;
        std::fs::create_dir_all(dir(root, name))?;
    }
    Ok(std::fs::write(root.join("profile"), name)?)
}
