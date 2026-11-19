use agent_forge::safety::{private_mkdir, read_private, write_new};
#[test]
fn directory_creation_refuses_link_before_mutating_target() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&outside, &alias).unwrap();
    assert!(private_mkdir(&alias.join("unexpected")).is_err());
    assert!(!outside.join("unexpected").exists());
}
#[test]
fn private_files_refuse_links_and_mutable_aliases() {
    let temp = tempfile::tempdir().unwrap();
    private_mkdir(temp.path()).unwrap();
    let file = temp.path().join("receipt");
    write_new(&file, b"private receipt").unwrap();
    assert_eq!(read_private(&file).unwrap(), b"private receipt");
    let linked = temp.path().join("other");
    std::fs::hard_link(&file, &linked).unwrap();
    assert!(read_private(&file).is_err());
    assert!(write_new(&file, b"replacement").is_err());
}
