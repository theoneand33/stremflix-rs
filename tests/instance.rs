use std::sync::mpsc::channel;
use std::time::Duration;
use stremflix_rs::instance::{forward, listen};

#[test]
fn later_launch_hands_link_to_running_instance() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!forward(dir.path(), "stremio:///detail/movie/tt1"), "no instance running yet");
    let (tx, rx) = channel();
    listen(dir.path(), move |l| tx.send(l).unwrap()).unwrap();
    assert!(forward(dir.path(), "stremio:///detail/movie/tt1"));
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "stremio:///detail/movie/tt1");

    // A wrong token is refused, and a link can't smuggle in a second line.
    let file = dir.path().join("instance");
    let good = std::fs::read_to_string(&file).unwrap();
    let port = good.split_once(' ').unwrap().0;
    std::fs::write(&file, format!("{port} wrong")).unwrap();
    assert!(!forward(dir.path(), "stremio:///detail/movie/tt2"));
    std::fs::write(&file, &good).unwrap();
    assert!(forward(dir.path(), "a\nb"));
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "ab");
    assert!(rx.try_recv().is_err());
}
