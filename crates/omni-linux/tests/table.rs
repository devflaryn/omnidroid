use omni_linux::syscall::{name_of, nr, Refusals, Table};

#[test]
fn names_come_from_the_arm64_table_and_unknown_numbers_are_still_named() {
    assert_eq!(name_of(nr::OPENAT), "openat");
    assert_eq!(name_of(nr::EXIT_GROUP), "exit_group");
    assert_eq!(name_of(9999), "syscall_9999");
}

#[test]
fn an_empty_table_has_no_handler() {
    assert!(Table::new().get(nr::GETPID).is_none());
    assert!(Table::new().get(1 << 40).is_none(), "an absurd number is just absent");
}

#[test]
fn a_refusal_is_recorded_once_with_its_first_caller_and_counted() {
    let r = Refusals::default();
    r.record("clone3".into(), 0x1000, 0x2000);
    r.record("clone3".into(), 0x3000, 0x4000);
    let list = r.list();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].count, list[0].first_pc, list[0].first_lr), (2, 0x1000, 0x2000));
    assert!(r.report().contains("clone3"));
}
