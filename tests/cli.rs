use std::fs;
use std::process::Command;

use rusqlite::Connection;

#[test]
fn migrate_command_applies_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let migrations_dir = dir.path().join("db/migrations");
    fs::create_dir_all(&migrations_dir).unwrap();
    fs::write(
        migrations_dir.join("001_create_users.sql"),
        "create table users (id integer primary key)",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("migrate")
        .arg("--database")
        .arg(&database)
        .arg("--migrations-dir")
        .arg(&migrations_dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "migrate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Applied 001_create_users\n"
    );

    let conn = Connection::open(&database).unwrap();
    let count: i64 = conn
        .query_row(
            "select count(*) from schema_migrations where version = '001_create_users'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn seed_command_runs_seed_files() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let seeds_dir = dir.path().join("db/seeds");
    fs::create_dir_all(&seeds_dir).unwrap();
    fs::write(
        seeds_dir.join("001_create_users.sql"),
        "create table users (id integer primary key, name text not null)",
    )
    .unwrap();
    fs::write(
        seeds_dir.join("002_add_lucy.sql"),
        "insert into users (name) values ('Lucy')",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("seed")
        .arg("--database")
        .arg(&database)
        .arg("--seeds-dir")
        .arg(&seeds_dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "seed failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Ran 001_create_users\nRan 002_add_lucy\n"
    );

    let conn = Connection::open(&database).unwrap();
    let count: i64 = conn
        .query_row("select count(*) from users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
