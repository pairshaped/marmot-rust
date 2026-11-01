#[test]
fn sqlite_runtime_includes_wal_reset_fix() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    let version: String = connection
        .query_row("select sqlite_version()", [], |row| row.get(0))
        .unwrap();
    let components: Vec<u32> = version
        .split('.')
        .map(|part| part.parse().unwrap())
        .collect();
    assert!(
        components.as_slice() >= [3, 51, 3].as_slice(),
        "SQLite {version} lacks the WAL-reset corruption fix"
    );
}
