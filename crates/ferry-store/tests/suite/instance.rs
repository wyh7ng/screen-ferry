use ferry_store::Store;

use crate::support::TempDir;

#[test]
fn only_one_instance_holds_a_directory_until_it_exits() {
    let dir = TempDir::new();
    let store = Store::open(dir.path()).unwrap();
    let first = store.try_lock_instance().unwrap().expect("第一份取得锁");
    assert!(store.clone().try_lock_instance().unwrap().is_none());
    drop(first);
    assert!(store.try_lock_instance().unwrap().is_some());
}

#[test]
fn different_data_directories_have_independent_locks() {
    let a = TempDir::new();
    let b = TempDir::new();
    let a = Store::open(a.path()).unwrap();
    let b = Store::open(b.path()).unwrap();
    let _held = a.try_lock_instance().unwrap().unwrap();
    assert!(b.try_lock_instance().unwrap().is_some());
}
