use std::path::Path;
use stremflix_rs::library::scan;

#[test]
fn scans_fixture_folder() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/library");
    let items = scan(&dir).unwrap();
    let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
    // notes.txt is not media; sub/.hidden.mp4 is hidden.
    assert_eq!(titles, ["movie-one", "clip-two"]);
    assert!(items[1].path.ends_with("sub/clip-two.mkv"));
    assert!(scan(&dir.join("does-not-exist")).is_err());
}

#[test]
fn media_files_go_by_extension_in_any_case() {
    use stremflix_rs::library::is_media;
    assert!(is_media(Path::new("/x/Film.MKV")) && is_media(Path::new("song.flac")));
    assert!(!is_media(Path::new("/x/film.torrent")) && !is_media(Path::new("/x/mkv")) && !is_media(Path::new("notes.txt")));
}
