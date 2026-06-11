use std::fs;
use std::process::Command;

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
        sql_dir: None,
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

#[test]
fn analyzes_and_emits_from_configured_sql_dir() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table settings (
            name text primary key,
            value text not null
        );
        create table likes (
            id integer primary key,
            user_id integer not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let sql_dir = source_root.join("sql");
    let likes_sql = sql_dir.join("likes");
    fs::create_dir_all(&likes_sql).unwrap();
    fs::write(
        sql_dir.join("get_settings.sql"),
        "select name, value from settings order by name",
    )
    .unwrap();
    fs::write(
        likes_sql.join("get_likes.sql"),
        "select id from likes where user_id = @user_id",
    )
    .unwrap();

    let config = Config {
        database,
        source_root,
        sql_dir: Some(sql_dir),
        output: dir.path().join("src/generated/sql"),
        target: Target::Rust,
        check: false,
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let mod_rs = fs::read_to_string(config.output.join("mod.rs")).unwrap();
    assert_eq!(mod_rs, "pub mod likes_sql;\npub mod sql;\n");

    let settings_output = fs::read_to_string(config.output.join("sql.rs")).unwrap();
    assert!(settings_output.contains("pub struct GetSettingsRow"));
    assert!(settings_output.contains("pub fn get_settings(conn: &Connection)"));

    let likes_output = fs::read_to_string(config.output.join("likes_sql.rs")).unwrap();
    assert!(likes_output.contains("pub fn get_likes(conn: &Connection, user_id: i64)"));
}

#[test]
fn generated_rust_functions_round_trip_against_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    create_runtime_schema(&conn);
    drop(conn);

    let source_root = dir.path().join("fixture/src");
    let sql_dir = source_root.join("app/sql");
    fs::create_dir_all(&sql_dir).unwrap();
    fs::write(
        sql_dir.join("create_user.sql"),
        "insert into users (name, active, avatar, score, nickname) \
         values (@name, @active, @avatar, @score, @nickname) \
         returning id, name, active, avatar, score, nickname",
    )
    .unwrap();
    fs::write(
        sql_dir.join("list_active_users.sql"),
        "select id, name, active, avatar, score, nickname \
         from users where active = @active order by id",
    )
    .unwrap();
    fs::write(
        sql_dir.join("count_users.sql"),
        "select count(*) from users",
    )
    .unwrap();
    fs::write(
        sql_dir.join("rename_user.sql"),
        "update users set nickname = @nickname where id = @id returning id, nickname",
    )
    .unwrap();
    fs::write(
        sql_dir.join("delete_user.sql"),
        "delete from users where id = @id",
    )
    .unwrap();
    fs::write(
        sql_dir.join("create_user_positional.sql"),
        "insert into users (name, active, avatar, score, nickname) \
         values (?, ?, ?, ?, ?) \
         returning id, name, active, avatar, score, nickname",
    )
    .unwrap();
    fs::write(
        sql_dir.join("set_score_positional.sql"),
        "update users set score = ? where id = ?",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_name_numbered.sql"),
        "select name from users where id = ?1 and active = ?2",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_name_numbered_leading_zero.sql"),
        "select name from users where id = ?01 and active = ?02",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_active_sparse_numbered.sql"),
        "select name from users where active = ?2 order by id",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_name_mixed_positional.sql"),
        "select name from users where id = ?1 and active = ?",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_name_named_numbered_same_slot.sql"),
        "select name from users where id = @id and id = ?1",
    )
    .unwrap();
    fs::write(
        sql_dir.join("create_user_returning_star.sql"),
        "insert into users (name, active, avatar, score, nickname) \
         values (@name, @active, @avatar, @score, @nickname) \
         returning *",
    )
    .unwrap();
    fs::write(
        sql_dir.join("delete_user_returning.sql"),
        "delete from users where id = @id returning id, name",
    )
    .unwrap();
    fs::write(
        sql_dir.join("create_keyword_table_row.sql"),
        "insert into \"returning\" (id, name) values (@id, @name) returning id, name",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_keyword_table_row.sql"),
        "select name from \"returning\" where id = @id",
    )
    .unwrap();
    fs::write(
        sql_dir.join("find_typed_thing.sql"),
        r#"select id, "type" from typed_things where "type" = @type"#,
    )
    .unwrap();
    fs::write(
        sql_dir.join("get_user_shared.sql"),
        "-- returns: UserRow\nselect id, name from users where id = @id",
    )
    .unwrap();
    fs::write(
        sql_dir.join("list_users_shared.sql"),
        "-- returns: UserRow\nselect id, name from users order by id",
    )
    .unwrap();

    let config = Config {
        database,
        source_root,
        sql_dir: None,
        output: dir.path().join("runtime/src/generated/sql"),
        target: Target::Rust,
        check: false,
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();
    write_runtime_crate(dir.path());

    let output = Command::new("cargo")
        .arg("test")
        .current_dir(dir.path().join("runtime"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generated crate tests failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_runtime_schema(conn: &Connection) {
    conn.execute_batch(
        r#"
        create table users (
            id integer primary key,
            name text not null,
            active boolean not null,
            avatar blob not null,
            score real not null,
            nickname text
        );
        create table "returning" (
            id integer not null,
            name text not null
        );
        create table typed_things (
            id integer primary key,
            "type" text not null
        );
        "#,
    )
    .unwrap();
}

fn write_runtime_crate(root: &std::path::Path) {
    let runtime = root.join("runtime");
    fs::create_dir_all(runtime.join("src/generated")).unwrap();
    fs::write(
        runtime.join("Cargo.toml"),
        r#"
[package]
name = "marmot-generated-runtime-test"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
rusqlite = { version = "0.37", features = ["bundled", "column_metadata"] }
"#,
    )
    .unwrap();
    fs::write(runtime.join("src/generated/mod.rs"), "pub mod sql;\n").unwrap();
    fs::write(
        runtime.join("src/lib.rs"),
        r##"
pub mod generated;

#[cfg(test)]
mod tests {
    use super::generated::sql::app_sql;
    use rusqlite::Connection;

    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            r#"
            create table users (
                id integer primary key,
                name text not null,
                active boolean not null,
                avatar blob not null,
                score real not null,
                nickname text
            );
            create table "returning" (
                id integer not null,
                name text not null
            );
            create table typed_things (
                id integer primary key,
                "type" text not null
            );
            "#,
        )
        .unwrap();
    }

    #[test]
    fn generated_functions_round_trip_common_sqlite_types() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app_sql::create_user(
            &conn,
            "alice",
            true,
            [1_u8, 2, 3],
            9.5,
            Some("ally"),
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 1);
        assert_eq!(created[0].name, "alice");
        assert!(created[0].active);
        assert_eq!(created[0].avatar, vec![1, 2, 3]);
        assert_eq!(created[0].score, 9.5);
        assert_eq!(created[0].nickname.as_deref(), Some("ally"));

        assert_eq!(app_sql::count_users_one(&conn).unwrap(), 1);

        let active = app_sql::list_active_users(&conn, true).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "alice");

        let renamed = app_sql::rename_user(&conn, None, 1).unwrap();
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].id, 1);
        assert_eq!(renamed[0].nickname, None);

        assert_eq!(app_sql::delete_user(&conn, 1).unwrap(), 1);
        assert_eq!(app_sql::count_users_one(&conn).unwrap(), 0);
    }

    #[test]
    fn generated_functions_bind_positional_and_numbered_parameters() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app_sql::create_user_positional(
            &conn,
            "bob",
            true,
            [4_u8, 5, 6],
            1.25,
            None,
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 1);
        assert_eq!(created[0].name, "bob");
        assert_eq!(created[0].avatar, vec![4, 5, 6]);
        assert_eq!(created[0].nickname, None);

        assert_eq!(app_sql::set_score_positional(&conn, 2.5, 1).unwrap(), 1);
        let active = app_sql::list_active_users(&conn, true).unwrap();
        assert_eq!(active[0].score, 2.5);

        assert_eq!(
            app_sql::find_name_numbered_one(&conn, 1, true).unwrap(),
            "bob"
        );
        assert_eq!(
            app_sql::find_name_numbered_leading_zero_one(&conn, 1, true).unwrap(),
            "bob"
        );
        assert_eq!(
            app_sql::find_active_sparse_numbered_one(&conn, true).unwrap(),
            "bob"
        );
        assert_eq!(
            app_sql::find_name_mixed_positional_one(&conn, 1, true).unwrap(),
            "bob"
        );
        assert_eq!(
            app_sql::find_name_named_numbered_same_slot_one(&conn, 1).unwrap(),
            "bob"
        );
    }

    #[test]
    fn generated_functions_handle_empty_results_returning_star_and_delete_returning() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        assert_eq!(app_sql::list_active_users(&conn, true).unwrap(), vec![]);

        let created = app_sql::create_user_returning_star(
            &conn,
            "carol",
            false,
            [7_u8, 8, 9],
            4.75,
            Some("c"),
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 1);
        assert_eq!(created[0].name, "carol");
        assert!(!created[0].active);
        assert_eq!(created[0].avatar, vec![7, 8, 9]);
        assert_eq!(created[0].score, 4.75);
        assert_eq!(created[0].nickname.as_deref(), Some("c"));

        let deleted = app_sql::delete_user_returning(&conn, 1).unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].id, 1);
        assert_eq!(deleted[0].name, "carol");
        assert_eq!(app_sql::count_users_one(&conn).unwrap(), 0);
    }

    #[test]
    fn generated_functions_handle_quoted_keyword_table_names() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app_sql::create_keyword_table_row(&conn, 10, "keyword").unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 10);
        assert_eq!(created[0].name, "keyword");
        assert_eq!(
            app_sql::find_keyword_table_row_one(&conn, 10).unwrap(),
            "keyword"
        );
    }

    #[test]
    fn generated_functions_handle_reserved_word_column_names() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        conn.execute(
            r#"insert into typed_things (id, "type") values (1, 'primary')"#,
            [],
        )
        .unwrap();

        let rows = app_sql::find_typed_thing(&conn, "primary").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].type_, "primary");
    }

    #[test]
    fn generated_functions_share_return_row_types() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        app_sql::create_user(&conn, "dana", true, [1_u8], 3.0, None).unwrap();
        app_sql::create_user(&conn, "erin", false, [2_u8], 4.0, None).unwrap();

        let dana: app_sql::UserRow = app_sql::get_user_shared(&conn, 1)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(dana.id, 1);
        assert_eq!(dana.name, "dana");

        let users: Vec<app_sql::UserRow> = app_sql::list_users_shared(&conn).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[1].id, 2);
        assert_eq!(users[1].name, "erin");
    }
}
"##,
    )
    .unwrap();
}
