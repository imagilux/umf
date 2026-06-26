//! Unit tests for the `ps` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::cli::process::{ProcessKind, ProcessStatus};

fn rec(
    id: &str,
    name: &str,
    kind: ProcessKind,
    status: ProcessStatus,
    started: u64,
) -> ProcessRecord {
    ProcessRecord {
        id: id.into(),
        name: name.into(),
        process: "op".into(),
        kind,
        status,
        exit_code: None,
        release: None,
        reference: None,
        pid: 1,
        started_epoch: started,
        finished_epoch: None,
    }
}

#[test]
fn filters_parse_and_and_wildcard() {
    let c = parse_filters(&["STATUS=exited,TYPE=build".to_string()]).expect("parse");
    assert_eq!(
        c,
        vec![
            ("status".into(), "exited".into()),
            ("type".into(), "build".into())
        ]
    );
    let r = rec("a", "x", ProcessKind::Build, ProcessStatus::Exited, 0);
    assert!(matches(&r, "status", "exited"));
    assert!(matches(&r, "type", "build"));
    assert!(matches(&r, "name", "all")); // wildcard
    assert!(!matches(&r, "status", "running"));
}

#[test]
fn unknown_filter_key_errors() {
    assert!(parse_filters(&["NOPE=1".to_string()]).is_err());
}

#[test]
fn sort_parsing_covers_keys_and_directions() {
    assert_eq!(parse_sort(None).unwrap(), ("started".into(), true));
    assert_eq!(parse_sort(Some("asc")).unwrap(), ("started".into(), false));
    assert_eq!(parse_sort(Some("name")).unwrap(), ("name".into(), false));
    assert_eq!(
        parse_sort(Some("status:desc")).unwrap(),
        ("status".into(), true)
    );
    assert!(parse_sort(Some("bogus")).is_err());
    assert!(parse_sort(Some("name:sideways")).is_err());
}

#[test]
fn sort_by_name_ascending() {
    let mut v = vec![
        rec("3", "charlie", ProcessKind::Build, ProcessStatus::Exited, 3),
        rec("1", "alpha", ProcessKind::Vm, ProcessStatus::Running, 1),
        rec(
            "2",
            "bravo",
            ProcessKind::Container,
            ProcessStatus::Failed,
            2,
        ),
    ];
    sort_records(&mut v, "name", false);
    assert_eq!(
        v.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["alpha", "bravo", "charlie"]
    );
}

#[test]
fn sort_started_desc_is_newest_first() {
    let mut v = vec![
        rec("1", "a", ProcessKind::Build, ProcessStatus::Exited, 10),
        rec("2", "b", ProcessKind::Build, ProcessStatus::Exited, 30),
        rec("3", "c", ProcessKind::Build, ProcessStatus::Exited, 20),
    ];
    sort_records(&mut v, "started", true);
    assert_eq!(
        v.iter().map(|r| r.started_epoch).collect::<Vec<_>>(),
        [30, 20, 10]
    );
}

#[test]
fn prune_removes_finished_keeps_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg = ProcessRegistry::at(dir.path().to_path_buf()).expect("registry");
    let mut running = rec(
        "run1",
        "live",
        ProcessKind::Container,
        ProcessStatus::Running,
        1,
    );
    running.pid = std::process::id() as i32; // alive ⇒ list() won't reconcile it away
    reg.write(&running).expect("write running");
    reg.write(&rec(
        "ex1",
        "done",
        ProcessKind::Build,
        ProcessStatus::Exited,
        2,
    ))
    .expect("write exited");
    reg.write(&rec(
        "fa1",
        "bad",
        ProcessKind::Build,
        ProcessStatus::Failed,
        3,
    ))
    .expect("write failed");

    let records = reg.list().expect("list");
    assert_eq!(records.len(), 3);
    let _ = do_prune(&reg, records, PsOutput::Plain);

    let after = reg.list().expect("list after prune");
    assert_eq!(
        after.len(),
        1,
        "only the running record should survive prune"
    );
    assert_eq!(after[0].id, "run1");
}
