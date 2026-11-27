//! Keys of progress, saved titles, watched marks and downloads.
//! An addon item is `<type>:<id>`, and an episode's id is its video id (`series:tt1:1:2`), so each episode resumes on its own.
//! A local file is `file:<path>` and a raw magnet `btih:<info hash>`; neither has an addon page.

pub const FILE: &str = "file";
pub const MAGNET: &str = "btih";

pub fn item(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

pub fn file(path: &str) -> String {
    item(FILE, path)
}

pub fn magnet(hash: &str) -> String {
    item(MAGNET, hash)
}

/// Type and id of any key; `None` without a `:`.
pub fn split(key: &str) -> Option<(&str, &str)> {
    key.split_once(':')
}

/// Type and id of an addon item; `None` for local files, raw magnets and keys without a type.
pub fn addon_item(key: &str) -> Option<(&str, &str)> {
    split(key).filter(|(k, _)| !is_local(k))
}

/// Local files and raw magnets: types no addon serves.
pub fn is_local(kind: &str) -> bool {
    kind == FILE || kind == MAGNET
}

/// The id in `key` when it is an item of `kind`.
pub fn id_of<'a>(key: &'a str, kind: &str) -> Option<&'a str> {
    key.strip_prefix(kind)?.strip_prefix(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_local_kinds_have_no_addon_item() {
        let k = item("series", "tt1:1:2");
        assert_eq!(split(&k), Some(("series", "tt1:1:2")));
        assert_eq!(addon_item(&k), Some(("series", "tt1:1:2")));
        assert_eq!(id_of(&k, "series"), Some("tt1:1:2"));
        assert_eq!(id_of(&k, "serie"), None);
        assert_eq!(addon_item(&file("/v/a:b.mkv")), None);
        assert_eq!(split(&file("/v/a:b.mkv")), Some((FILE, "/v/a:b.mkv")));
        assert_eq!(addon_item(&magnet("abc")), None);
        assert_eq!(addon_item("nokind"), None);
    }
}
