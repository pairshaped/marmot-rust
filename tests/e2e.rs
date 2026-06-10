use std::fs;

use marmot::{Config, Target, analyze_project, emit_project};
use rusqlite::Connection;

#[test]
fn analyzes_and_emits_multiple_colocated_sql_modules() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table users (
            id integer primary key,
            name text not null
        );
        create table items (
            id integer primary key,
            owner_id integer not null,
            title text not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    let items_sql = source_root.join("items/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::create_dir_all(&items_sql).unwrap();
    fs::write(
        users_sql.join("find_user.sql"),
        "select id, name from users where id = ?",
    )
    .unwrap();
    fs::write(
        items_sql.join("list_items.sql"),
        "select id, title from items where owner_id = @owner_id",
    )
    .unwrap();

    let config = Config {
        database,
        source_root,
        output: dir.path().join("src/generated/sql"),
        target: Target::Rust,
        check: false,
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let mod_rs = fs::read_to_string(config.output.join("mod.rs")).unwrap();
    assert_eq!(mod_rs, "pub mod items_sql;\npub mod users_sql;\n");

    let users_output = fs::read_to_string(config.output.join("users_sql.rs")).unwrap();
    assert!(users_output.contains("pub struct FindUserRow"));
    assert!(users_output.contains("pub fn find_user(conn: &Connection, param: i64)"));

    let items_output = fs::read_to_string(config.output.join("items_sql.rs")).unwrap();
    assert!(items_output.contains("pub struct ListItemsRow"));
    assert!(items_output.contains("pub fn list_items(conn: &Connection, owner_id: i64)"));
    assert!(items_output.contains("\"@owner_id\": owner_id"));
}
