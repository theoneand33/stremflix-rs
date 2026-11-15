use stremflix_rs::profile::{active, dir, list, switch, valid_name};

#[test]
fn names_are_safe_folder_names() {
    assert_eq!(valid_name("  Kids 2 "), Some("Kids 2"));
    for bad in ["", "  ", "../x", "a/b", "a\\b", ".", "x.db", "con", "LPT1", "COM¹", &"a".repeat(33)] {
        assert_eq!(valid_name(bad), None, "{bad:?}");
    }
}

#[test]
fn switching_creates_and_selects_a_profile() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    assert_eq!((active(root), list(root)), (String::new(), vec![]));
    switch(root, "Kids").unwrap();
    switch(root, "Anna").unwrap();
    assert_eq!((active(root).as_str(), list(root)), ("Anna", vec!["Anna".to_string(), "Kids".to_string()]));
    assert_eq!(dir(root, "Anna"), root.join("profiles").join("Anna"));
    assert!(dir(root, "Anna").is_dir());
    // A name that would leave the profiles folder is refused and changes nothing.
    assert!(switch(root, "../evil").is_err());
    assert_eq!(active(root), "Anna");
    // A tampered or stale pointer falls back to the default profile, which is the data folder itself.
    std::fs::write(root.join("profile"), "../..").unwrap();
    assert_eq!(active(root), "");
    std::fs::write(root.join("profile"), "Gone").unwrap();
    assert_eq!(active(root), "");
    switch(root, "").unwrap();
    assert_eq!((active(root), dir(root, "")), (String::new(), root.to_path_buf()));
}
