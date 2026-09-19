use super::*;

#[test]
fn the_default_is_squashfs_the_only_in_process_writer() {
    // The default must be the filesystem that needs no host tooling —
    // otherwise `umf compile` would acquire a hard dependency on a binary
    // for its default path, which is what `host_mkfs` returning `None`
    // encodes.
    assert_eq!(RootfsFs::default(), RootfsFs::Squashfs);
    assert_eq!(RootfsFs::default().host_mkfs(), None);
}

#[test]
fn every_filesystem_round_trips_through_its_token() {
    for fs in RootfsFs::ALL {
        assert_eq!(
            RootfsFs::from_token(fs.as_str()),
            Some(fs),
            "{fs} must parse back from its own token",
        );
    }
}

#[test]
fn an_unknown_token_is_rejected_rather_than_defaulted() {
    // Falling back to squashfs here would project a filesystem the operator
    // did not ask for, under a cmdline claiming it was theirs.
    assert_eq!(RootfsFs::from_token("btrfs"), None);
    assert_eq!(RootfsFs::from_token(""), None);
    assert_eq!(RootfsFs::from_token("SQUASHFS"), None, "tokens are exact");
}

#[test]
fn the_error_names_every_supported_filesystem() {
    let rendered = UnsupportedRootfsFs("btrfs".to_string()).to_string();
    assert!(rendered.contains("btrfs"), "names the rejected value");
    for fs in RootfsFs::ALL {
        assert!(
            rendered.contains(fs.as_str()),
            "error must list {fs} as supported: {rendered}",
        );
    }
}

#[test]
fn only_ext4_is_writable() {
    assert!(!RootfsFs::Ext4.is_read_only());
    assert!(RootfsFs::Squashfs.is_read_only());
    assert!(RootfsFs::Erofs.is_read_only());
}
