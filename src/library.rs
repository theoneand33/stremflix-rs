//! Local media library scanning.

use std::path::{Path, PathBuf};

pub const MEDIA_EXTS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v", "ogv", "mp3", "ogg", "flac", "wav", "m4a"];

#[derive(Debug, Clone, PartialEq)]
pub struct LibraryItem {
    pub path: PathBuf,
    pub title: String,
}

/// Whether `path` has one of [`MEDIA_EXTS`], in any case.
pub fn is_media(path: &Path) -> bool {
    path.extension().and_then(|x| x.to_str()).is_some_and(|x| MEDIA_EXTS.contains(&x.to_ascii_lowercase().as_str()))
}

/// Recursively find media files under `dir`, skipping hidden files/dirs. Sorted by path.
pub fn scan(dir: &Path) -> std::io::Result<Vec<LibraryItem>> {
    if !dir.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("not a directory: {}", dir.display())));
    }
    let mut items: Vec<LibraryItem> = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| is_media(e.path()))
        .map(|e| LibraryItem {
            title: e.path().file_stem().unwrap_or_default().to_string_lossy().into_owned(),
            path: e.into_path(),
        })
        .collect();
    items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(items)
}
