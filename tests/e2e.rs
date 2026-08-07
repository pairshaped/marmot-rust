use std::fs;
use std::path::Path;
use std::process::Command;

use marmot::model::ConnectionAccess;
use marmot::{Config, Error, Target, analyze_project, emit_project};
use rusqlite::Connection;

fn write_sql_file(module_path: &Path, file_name: &str, sql: impl AsRef<str>) {
    fs::create_dir_all(module_path.parent().unwrap()).unwrap();
    fs::write(module_path.with_extension("rs"), "").unwrap();
    let companion = module_path.with_extension("sql");
    let existing = fs::read_to_string(&companion).unwrap_or_default();
    let separator = if existing.trim().is_empty() {
        ""
    } else {
        "\n\n"
    };
    let function_name = file_name.trim_end_matches(".sql");
    fs::write(
        companion,
        format!(
            "{existing}{separator}-- func: {function_name}\n{}",
            sql.as_ref()
        ),
    )
    .unwrap();
}

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
    fs::create_dir_all(&source_root).unwrap();
    fs::write(source_root.join("users.rs"), "").unwrap();
    fs::write(source_root.join("items.rs"), "").unwrap();
    fs::create_dir_all(source_root.join("admin/items")).unwrap();
    fs::write(source_root.join("admin/items/index.rs"), "").unwrap();
    fs::write(
        source_root.join("users.sql"),
        "
        -- func: find_user
        select id, name from users where id = ?;

        -- func: list_users
        select id, name from users order by id;
        ",
    )
    .unwrap();
    fs::write(
        source_root.join("admin/items/index.sql"),
        "
        -- func: list_admin_items
        select id, title from items order by id;
        ",
    )
    .unwrap();
    fs::write(
        source_root.join("items.sql"),
        "
        -- func: list_items
        select id, title from items where owner_id = @owner_id;
        ",
    )
    .unwrap();

    let config = Config {
        database,
        source_root,
        output: dir.path().join("src/generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let mod_rs = fs::read_to_string(config.output.join("mod.rs")).unwrap();
    assert_eq!(mod_rs, "pub mod admin;\npub mod items;\npub mod users;\n");

    let admin_mod_rs = fs::read_to_string(config.output.join("admin/mod.rs")).unwrap();
    assert_eq!(admin_mod_rs, "pub mod items;\n");

    let admin_items_mod_rs = fs::read_to_string(config.output.join("admin/items/mod.rs")).unwrap();
    assert_eq!(admin_items_mod_rs, "pub mod index;\n");

    let users_output = fs::read_to_string(config.output.join("users.rs")).unwrap();
    assert!(users_output.contains("pub struct FindUserRow"));
    assert!(users_output.contains("pub struct FindUserParams"));
    assert!(users_output.contains("pub fn find_user(conn: &Connection, params: FindUserParams)"));
    assert!(users_output.contains("pub struct ListUsersRow"));
    assert!(users_output.contains("pub fn list_users(conn: &Connection)"));

    let items_output = fs::read_to_string(config.output.join("items.rs")).unwrap();
    assert!(items_output.contains("pub struct ListItemsRow"));
    assert!(items_output.contains("pub struct ListItemsParams"));
    assert!(items_output.contains("pub fn list_items(conn: &Connection, params: ListItemsParams)"));
    assert!(items_output.contains("where owner_id = ?1"));
    assert!(items_output.contains("params![params.owner_id]"));

    let admin_items_output =
        fs::read_to_string(config.output.join("admin/items/index.rs")).unwrap();
    assert!(admin_items_output.contains("pub struct ListAdminItemsRow"));
    assert!(admin_items_output.contains("pub fn list_admin_items(conn: &Connection)"));
}

#[test]
fn named_boolean_constraints_emit_bool_types_for_strict_tables() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table settings (
            id integer primary key,
            enabled integer not null
                constraint boolean check (enabled in (0, 1)),
            featured integer
                constraint boolean check (featured in (0, 1)),
            winner_side integer check (winner_side in (0, 1))
        ) strict;
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("settings");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "show.sql",
        "select enabled, featured, winner_side from settings where id = @id",
    );
    write_sql_file(
        &module_dir,
        "update.sql",
        "update settings set enabled = @enabled, featured = @featured where id = @id",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("settings.rs")).unwrap();
    assert!(output.contains("pub enabled: bool"));
    assert!(output.contains("pub featured: Option<bool>"));
    assert!(output.contains("pub winner_side: Option<i64>"));
    assert!(output.contains("enabled: bool"));
    assert!(output.contains("featured: Option<bool>"));
}

#[test]
fn scalar_subquery_outputs_generate_nullable_rust_fields() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table carts (id integer primary key);
        create table waivers (
            id integer primary key,
            cart_id integer not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("carts");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "show_cart.sql",
        "
        select carts.id,
               (
                   select waivers.id
                   from waivers
                   where waivers.cart_id = carts.id
                   order by waivers.id asc
                   limit 1
               ) as first_required_waiver_id,
               cast((
                   select waivers.id
                   from waivers
                   where waivers.cart_id = carts.id
                   order by waivers.id asc
                   limit 1
               ) as integer) as first_required_waiver_id_cast
        from carts
        ",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("carts.rs")).unwrap();
    assert!(output.contains("pub first_required_waiver_id: Option<i64>"));
    assert!(output.contains("pub first_required_waiver_id_cast: Option<i64>"));
}

#[test]
fn optional_and_sentinel_filters_generate_expected_parameter_types() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table tasks (
            id integer primary key,
            account_id integer not null,
            status text not null,
            accepted boolean not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("tasks");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_tasks.sql",
        "
        select id
        from tasks
        where (?1 is null or account_id = ?1)
          and (?2 is null or status = ?2)
          and (?3 = -1 or accepted = ?3)
        ",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("tasks.rs")).unwrap();
    assert!(output.contains("pub struct ListTasksParams<'a>"));
    assert!(output.contains("pub param: Option<i64>"));
    assert!(output.contains("pub param_2: Option<&'a str>"));
    assert!(output.contains("pub param_3: i64"));
    assert!(output.contains("pub fn list_tasks(conn: &Connection, params: ListTasksParams<'_>)"));
}

#[test]
fn strict_temporal_suffixes_generate_checked_boundary_types() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table events (
            id integer primary key,
            title text not null,
            starts_at text not null,
            registration_closes_on text
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("fixture/src");
    let module_dir = source_root.join("app");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "create_event.sql",
        "insert into events (title, starts_at, registration_closes_on) \
         values (@title, @starts_at, @registration_closes_on) \
         returning id, title, starts_at, registration_closes_on",
    );
    write_sql_file(
        &module_dir,
        "list_events_after.sql",
        "select id, title, starts_at, registration_closes_on \
         from events \
         where starts_at >= @starts_at \
           and (@registration_closes_on is null \
                or registration_closes_on = @registration_closes_on) \
         order by id",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("runtime/src/generated/sql"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let mod_rs = fs::read_to_string(config.output.join("mod.rs")).unwrap();
    assert_eq!(mod_rs, "pub mod app;\npub mod temporal;\n");
    let app_rs = fs::read_to_string(config.output.join("app.rs")).unwrap();
    assert!(app_rs.contains("use super::temporal as temporal;"));
    assert!(app_rs.contains("pub starts_at: temporal::DbDateTime"));
    assert!(app_rs.contains("pub registration_closes_on: Option<temporal::DbDate>"));
    assert!(app_rs.contains("pub starts_at: &'a temporal::DbDateTime"));
    assert!(app_rs.contains("pub registration_closes_on: Option<&'a temporal::DbDate>"));
    assert!(app_rs.contains(
        "params![params.title, params.starts_at.as_str(), params.registration_closes_on.map(|value| value.as_str())]"
    ));

    write_temporal_runtime_crate(dir.path());

    let output = Command::new("cargo")
        .arg("test")
        .current_dir(dir.path().join("runtime"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generated temporal crate tests failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn strict_temporal_suffixes_reject_non_text_storage() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table events (
            id integer primary key,
            starts_at integer not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("events");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_events.sql",
        "select id, starts_at from events",
    );

    let result = analyze_project(&Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    });

    assert!(matches!(
        result,
        Err(Error::TemporalColumnTypeMismatch {
            table,
            column,
            declared_type,
            expected
        }) if table == "events"
            && column == "starts_at"
            && declared_type == "INTEGER"
            && expected == "TEXT"
    ));
}

#[test]
fn temporal_comparison_expression_infers_datetime_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table registrations (
            id integer primary key,
            registered_at text,
            created_at text not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("registrations");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_recent.sql",
        "select id from registrations \
         where coalesce(registered_at, created_at) >= @season_start_at",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("registrations.rs")).unwrap();
    assert!(output.contains("pub season_start_at: &'a temporal::DbDateTime"));
}

#[test]
fn temporal_suffix_infers_standalone_parameter_type() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    Connection::open(&database).unwrap();

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("events");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "echo_published_at.sql",
        "select @published_at as value",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("events.rs")).unwrap();
    assert!(output.contains("pub published_at: &'a temporal::DbDateTime"));
}

#[test]
fn temporal_suffix_survives_storage_cast() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    Connection::open(&database).unwrap();

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("events");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "echo_published_at.sql",
        "select cast(@published_at as text) as value",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("events.rs")).unwrap();
    assert!(output.contains("pub published_at: &'a temporal::DbDateTime"));
}

#[test]
fn sqlite_unixepoch_datetime_infers_integer_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    Connection::open(&database).unwrap();

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("events");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "from_unixepoch.sql",
        "select datetime(@season_start, 'unixepoch') as value",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("events.rs")).unwrap();
    assert!(output.contains("season_start: i64"));
}

#[test]
fn conflicting_temporal_parameter_evidence_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table events (
            id integer primary key,
            starts_at text not null,
            registration_closes_on text not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("events");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_between.sql",
        "select id from events \
         where starts_at >= @boundary \
           and registration_closes_on <= @boundary",
    );

    let result = analyze_project(&Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    });

    let error = result.expect_err("conflicting temporal evidence should fail analysis");
    assert!(
        error
            .to_string()
            .contains("parameter @boundary has conflicting temporal types DbDateTime and DbDate")
    );
}

#[test]
fn cte_temporal_alias_infers_datetime_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table registrations (
            id integer primary key,
            registered_at text,
            created_at text not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("registrations");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_recent.sql",
        "with effective_registrations as (\
             select coalesce(registered_at, created_at) as effective_at \
             from registrations\
         ) \
         select effective_at \
         from effective_registrations \
         where effective_at >= @season_start_at",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("registrations.rs")).unwrap();
    assert!(output.contains("pub season_start_at: &'a temporal::DbDateTime"));
}

#[test]
fn guarded_nullable_column_emits_non_null_result() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table products (
            id integer primary key,
            season integer
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("products");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_seasons.sql",
        "select season from products where season is not null",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("products.rs")).unwrap();
    assert!(output.contains("Result<Vec<i64>>"));
    assert!(!output.contains("Result<Vec<Option<i64>>>"));
}

#[test]
fn guarded_distinct_nullable_column_emits_non_null_result() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table products (
            id integer primary key,
            season integer
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("products");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_seasons.sql",
        "select distinct p.season \
         from products p \
         where p.season is not null",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("products.rs")).unwrap();
    assert!(output.contains("Result<Vec<i64>>"));
    assert!(!output.contains("Result<Vec<Option<i64>>>"));
}

#[test]
fn guarded_nullable_column_cast_emits_non_null_result() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table products (
            id integer primary key,
            season integer
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("products");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_cast_seasons.sql",
        "select cast(season as integer) as season \
         from products \
         where season is not null",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("products.rs")).unwrap();
    assert!(output.contains("Result<Vec<i64>>"));
    assert!(!output.contains("Result<Vec<Option<i64>>>"));
}

#[test]
fn compound_result_stays_nullable_when_any_arm_is_unguarded() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table current_products (
            id integer primary key,
            season integer
        );
        create table archived_products (
            id integer primary key,
            season integer
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("products");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_seasons.sql",
        "select season from current_products where season is not null \
         union all \
         select season from archived_products",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("products.rs")).unwrap();
    assert!(output.contains("Result<Vec<Option<i64>>>"));
}

#[test]
fn compound_result_is_non_null_when_every_arm_is_guarded() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table current_products (
            id integer primary key,
            season integer
        );
        create table archived_products (
            id integer primary key,
            season integer
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("products");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_seasons.sql",
        "select p.season from current_products p where p.season is not null \
         union all \
         select p.season from archived_products p where p.season is not null",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("products.rs")).unwrap();
    assert!(output.contains("Result<Vec<i64>>"));
    assert!(!output.contains("Result<Vec<Option<i64>>>"));
}

#[test]
fn non_null_guards_do_not_leak_across_query_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table products (
            id integer primary key,
            season integer
        );
        create table archived_products (
            id integer primary key,
            season integer
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let module_dir = source_root.join("products");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "list_unguarded.sql",
        "select season from products",
    );
    write_sql_file(
        &module_dir,
        "list_unrelated_guard.sql",
        "select p.season \
         from products p \
         join archived_products a on a.id = p.id \
         where a.season is not null",
    );
    write_sql_file(
        &module_dir,
        "list_nested_guard.sql",
        "select p.season \
         from products p \
         where exists (\
             select 1 from archived_products a where a.season is not null\
         )",
    );
    write_sql_file(
        &module_dir,
        "list_outer_join_guard.sql",
        "select a.season \
         from products p \
         left join archived_products a \
           on a.id = p.id and a.season is not null",
    );

    let config = Config {
        database,
        source_root,
        output: dir.path().join("generated"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.output.join("products.rs")).unwrap();
    for function in [
        "list_unguarded",
        "list_unrelated_guard",
        "list_nested_guard",
        "list_outer_join_guard",
    ] {
        assert!(output.contains(&format!(
            "pub fn {function}(conn: &Connection) -> Result<Vec<Option<i64>>>"
        )));
    }
}

#[test]
fn generated_rust_functions_round_trip_against_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    create_runtime_schema(&conn);
    drop(conn);

    let source_root = dir.path().join("fixture/src");
    let module_dir = source_root.join("app");
    fs::create_dir_all(&module_dir).unwrap();
    write_sql_file(
        &module_dir,
        "create_user.sql",
        "insert into users (name, active, avatar, score, nickname) \
         values (@name, @active, @avatar, @score, @nickname) \
         returning id, name, active, avatar, score, nickname",
    );
    write_sql_file(
        &module_dir,
        "insert_user.sql",
        "insert into users (name, active, avatar, score, nickname) \
         values (@name, @active, @avatar, @score, @nickname)",
    );
    write_sql_file(
        &module_dir,
        "list_active_users.sql",
        "select id, name, active, avatar, score, nickname \
         from users where active = @active order by id",
    );
    write_sql_file(&module_dir, "count_users.sql", "select count(*) from users");
    write_sql_file(
        &module_dir,
        "rename_user.sql",
        "update users set nickname = @nickname where id = @id returning id, nickname",
    );
    write_sql_file(
        &module_dir,
        "delete_user.sql",
        "delete from users where id = @id",
    );
    write_sql_file(
        &module_dir,
        "create_user_positional.sql",
        "insert into users (name, active, avatar, score, nickname) \
         values (?, ?, ?, ?, ?) \
         returning id, name, active, avatar, score, nickname",
    );
    write_sql_file(
        &module_dir,
        "set_score_positional.sql",
        "update users set score = ? where id = ?",
    );
    write_sql_file(
        &module_dir,
        "find_name_numbered.sql",
        "select name from users where id = ?1 and active = ?2",
    );
    write_sql_file(
        &module_dir,
        "find_name_numbered_leading_zero.sql",
        "select name from users where id = ?01 and active = ?02",
    );
    write_sql_file(
        &module_dir,
        "find_active_sparse_numbered.sql",
        "select name from users where active = ?2 order by id",
    );
    write_sql_file(
        &module_dir,
        "find_name_mixed_positional.sql",
        "select name from users where id = ?1 and active = ?",
    );
    write_sql_file(
        &module_dir,
        "find_name_named_numbered_same_slot.sql",
        "select name from users where id = @id and id = ?1",
    );
    write_sql_file(
        &module_dir,
        "create_user_returning_star.sql",
        "insert into users (name, active, avatar, score, nickname) \
         values (@name, @active, @avatar, @score, @nickname) \
         returning *",
    );
    write_sql_file(
        &module_dir,
        "delete_user_returning.sql",
        "delete from users where id = @id returning id, name",
    );
    write_sql_file(
        &module_dir,
        "create_keyword_table_row.sql",
        "insert into \"returning\" (id, name) values (@id, @name) returning id, name",
    );
    write_sql_file(
        &module_dir,
        "find_keyword_table_row.sql",
        "select name from \"returning\" where id = @id",
    );
    write_sql_file(
        &module_dir,
        "find_typed_thing.sql",
        r#"select id, "type" from typed_things where "type" = @type"#,
    );
    write_sql_file(
        &module_dir,
        "create_event.sql",
        "insert into events (name, event_date, starts_at) \
         values (@name, @event_date, @starts_at) \
         returning id, event_date, starts_at, created_at",
    );
    write_sql_file(
        &module_dir,
        "list_events_since.sql",
        "select id, event_date, starts_at, created_at \
         from events where starts_at >= @starts_at order by id",
    );
    write_sql_file(
        &module_dir,
        "update_user_field.sql",
        "-- columns: name, active
         update users set {{column}} = @value where id = @id",
    );
    write_sql_file(
        &module_dir,
        "create_user_name_index.sql",
        "create index users_name_idx on users (name)",
    );
    let config = Config {
        database,
        source_root,
        output: dir.path().join("runtime/src/generated/sql"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    assert_eq!(
        project
            .queries
            .iter()
            .find(|query| query.name == "list_active_users")
            .unwrap()
            .connection_access,
        ConnectionAccess::Read
    );
    assert_eq!(
        project
            .queries
            .iter()
            .find(|query| query.name == "create_user")
            .unwrap()
            .connection_access,
        ConnectionAccess::Mutation
    );
    assert_eq!(
        project
            .queries
            .iter()
            .find(|query| query.name == "create_user_name_index")
            .unwrap()
            .connection_access,
        ConnectionAccess::Mutation
    );
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

    fs::create_dir_all(dir.path().join("runtime/src/bin")).unwrap();
    fs::write(
        dir.path().join("runtime/src/bin/immutable_mutation.rs"),
        r#"
use marmot_generated_runtime_test::generated::sql::app;
use rusqlite::Connection;

fn main() {
    let conn = Connection::open_in_memory().unwrap();
    let _ = app::delete_user(&conn, app::DeleteUserParams { id: 1 });
}
"#,
    )
    .unwrap();
    let output = Command::new("cargo")
        .args(["check", "--bin", "immutable_mutation"])
        .current_dir(dir.path().join("runtime"))
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "immutable mutation unexpectedly compiled"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("MutationConnection"), "{stderr}");
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
        create table events (
            id integer primary key,
            name text not null,
            event_date date not null,
            starts_at datetime not null,
            created_at timestamp not null default current_timestamp
        );
        "#,
    )
    .unwrap();
}

fn write_temporal_runtime_crate(root: &std::path::Path) {
    let runtime = root.join("runtime");
    fs::create_dir_all(runtime.join("src/generated")).unwrap();
    fs::write(
        runtime.join("Cargo.toml"),
        r#"
[package]
name = "marmot-generated-temporal-runtime-test"
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
    use super::generated::sql::{
        app,
        temporal::{DbDate, DbDateTime},
    };
    use rusqlite::Connection;

    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            r#"
            create table events (
                id integer primary key,
                title text not null,
                starts_at text not null,
                registration_closes_on text
            );
            "#,
        )
        .unwrap();
    }

    #[test]
    fn generated_functions_round_trip_temporal_suffix_types() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let starts_at = DbDateTime::new("2026-06-11 09:30:00").unwrap();
        let closes_on = DbDate::new("2026-06-01").unwrap();
        let created = app::create_event(
            &mut conn,
            app::CreateEventParams {
                title: "league",
                starts_at: &starts_at,
                registration_closes_on: Some(&closes_on),
            },
        )
        .unwrap();

        assert_eq!(created.len(), 1);
        assert_eq!(created[0].starts_at.as_str(), "2026-06-11 09:30:00");
        assert_eq!(
            created[0].registration_closes_on.as_ref().map(DbDate::as_str),
            Some("2026-06-01")
        );

        let rows = app::list_events_after(
            &conn,
            app::ListEventsAfterParams {
                starts_at: &DbDateTime::new("2026-06-01 00:00:00").unwrap(),
                registration_closes_on: Some(&closes_on),
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].starts_at, starts_at);
    }

    #[test]
    fn generated_types_reject_invalid_temporal_values() {
        assert!(DbDate::new("2026-02-29").is_err());
        assert!(DbDateTime::new("2026-06-11T09:30:00Z").is_err());

        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);
        conn.execute(
            "insert into events (title, starts_at) values ('bad', '2026-99-11 09:30:00')",
            [],
        )
        .unwrap();

        let error = app::list_events_after(
            &conn,
            app::ListEventsAfterParams {
                starts_at: &DbDateTime::new("2026-01-01 00:00:00").unwrap(),
                registration_closes_on: None,
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("starts_at"), "{message}");
        assert!(message.contains("2026-99-11 09:30:00"), "{message}");
        assert!(message.contains("YYYY-MM-DD HH:MM:SS"), "{message}");
        assert!(std::error::Error::source(&error).is_some());
        assert!(
            std::error::Error::source(&error)
                .and_then(std::error::Error::source)
                .is_some()
        );
    }
}
"##,
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
    use super::generated::sql::app;
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
            create table events (
                id integer primary key,
                name text not null,
                event_date date not null,
                starts_at datetime not null,
                created_at timestamp not null default current_timestamp
            );
            "#,
        )
        .unwrap();
    }

    #[test]
    fn generated_functions_round_trip_common_sqlite_types() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app::create_user(
            &mut conn,
            app::CreateUserParams {
                name: "alice",
                active: true,
                avatar: &[1_u8, 2, 3],
                score: 9.5,
                nickname: Some("ally"),
            },
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 1);
        assert_eq!(created[0].name, "alice");
        assert!(created[0].active);
        assert_eq!(created[0].avatar, vec![1, 2, 3]);
        assert_eq!(created[0].score, 9.5);
        assert_eq!(created[0].nickname.as_deref(), Some("ally"));

        assert_eq!(app::count_users_one(&conn).unwrap(), 1);

        let active =
            app::list_active_users(&conn, app::ListActiveUsersParams { active: true }).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "alice");

        let renamed = app::rename_user(
            &mut conn,
            app::RenameUserParams {
                nickname: None,
                id: 1,
            },
        )
        .unwrap();
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].id, 1);
        assert_eq!(renamed[0].nickname, None);

        assert_eq!(
            app::delete_user(&mut conn, app::DeleteUserParams { id: 1 }).unwrap(),
            1
        );
        assert_eq!(app::count_users_one(&conn).unwrap(), 0);
    }

    #[test]
    fn generated_mutations_accept_transactions() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let tx = conn.transaction().unwrap();
        app::create_user(
            &tx,
            app::CreateUserParams {
                name: "alice",
                active: true,
                avatar: &[],
                score: 1.0,
                nickname: None,
            },
        )
        .unwrap();
        assert_eq!(app::count_users_one(&tx).unwrap(), 1);
        tx.commit().unwrap();

        assert_eq!(app::count_users_one(&conn).unwrap(), 1);
    }

    #[test]
    fn generated_batch_mutations_prepare_once_inside_the_callers_transaction() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let tx = conn.transaction().unwrap();
        let rows = [
            app::InsertUserParams {
                name: "alice",
                active: true,
                avatar: &[1],
                score: 1.0,
                nickname: None,
            },
            app::InsertUserParams {
                name: "bob",
                active: false,
                avatar: &[2],
                score: 2.0,
                nickname: Some("bobby"),
            },
        ];
        assert_eq!(app::insert_user_batch(&tx, &rows).unwrap(), 2);
        assert_eq!(app::count_users_one(&tx).unwrap(), 2);
        tx.rollback().unwrap();

        assert_eq!(app::count_users_one(&conn).unwrap(), 0);
    }

    #[test]
    fn generated_allowlisted_column_updates_use_static_enum_variants() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);
        app::create_user(
            &mut conn,
            app::CreateUserParams {
                name: "alice",
                active: true,
                avatar: &[],
                score: 1.0,
                nickname: None,
            },
        )
        .unwrap();

        app::update_user_field(
            &mut conn,
            app::UpdateUserFieldParams {
                column: app::UpdateUserFieldColumn::Name,
                value: rusqlite::types::Value::Text("bob".to_string()),
                id: 1,
            },
        )
        .unwrap();
        app::update_user_field(
            &mut conn,
            app::UpdateUserFieldParams {
                column: app::UpdateUserFieldColumn::Active,
                value: rusqlite::types::Value::Integer(0),
                id: 1,
            },
        )
        .unwrap();

        let row = conn
            .query_row("select name, active from users where id = 1", [], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .unwrap();
        assert_eq!(row, ("bob".to_string(), false));
    }

    #[test]
    fn generated_functions_bind_positional_and_numbered_parameters() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app::create_user_positional(
            &mut conn,
            app::CreateUserPositionalParams {
                param: "bob",
                param_2: true,
                param_3: &[4_u8, 5, 6],
                param_4: 1.25,
                param_5: None,
            },
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 1);
        assert_eq!(created[0].name, "bob");
        assert_eq!(created[0].avatar, vec![4, 5, 6]);
        assert_eq!(created[0].nickname, None);

        assert_eq!(
            app::set_score_positional(
                &mut conn,
                app::SetScorePositionalParams {
                    param: 2.5,
                    param_2: 1,
                },
            )
            .unwrap(),
            1
        );
        let active =
            app::list_active_users(&conn, app::ListActiveUsersParams { active: true }).unwrap();
        assert_eq!(active[0].score, 2.5);

        assert_eq!(
            app::find_name_numbered_one(
                &conn,
                app::FindNameNumberedParams {
                    param: 1,
                    param_2: true,
                },
            )
            .unwrap(),
            "bob"
        );
        assert_eq!(
            app::find_name_numbered_leading_zero_one(
                &conn,
                app::FindNameNumberedLeadingZeroParams {
                    param: 1,
                    param_2: true,
                },
            )
            .unwrap(),
            "bob"
        );
        assert_eq!(
            app::find_active_sparse_numbered_one(
                &conn,
                app::FindActiveSparseNumberedParams { param_2: true },
            )
            .unwrap(),
            "bob"
        );
        assert_eq!(
            app::find_name_mixed_positional_one(
                &conn,
                app::FindNameMixedPositionalParams {
                    param: 1,
                    param_2: true,
                },
            )
            .unwrap(),
            "bob"
        );
        assert_eq!(
            app::find_name_named_numbered_same_slot_one(
                &conn,
                app::FindNameNamedNumberedSameSlotParams { id: 1 },
            )
            .unwrap(),
            "bob"
        );
    }

    #[test]
    fn generated_functions_handle_empty_results_returning_star_and_delete_returning() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        assert_eq!(
            app::list_active_users(&conn, app::ListActiveUsersParams { active: true }).unwrap(),
            vec![]
        );

        let created = app::create_user_returning_star(
            &mut conn,
            app::CreateUserReturningStarParams {
                name: "carol",
                active: false,
                avatar: &[7_u8, 8, 9],
                score: 4.75,
                nickname: Some("c"),
            },
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 1);
        assert_eq!(created[0].name, "carol");
        assert!(!created[0].active);
        assert_eq!(created[0].avatar, vec![7, 8, 9]);
        assert_eq!(created[0].score, 4.75);
        assert_eq!(created[0].nickname.as_deref(), Some("c"));

        let deleted = app::delete_user_returning(
            &mut conn,
            app::DeleteUserReturningParams { id: 1 },
        )
        .unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].id, 1);
        assert_eq!(deleted[0].name, "carol");
        assert_eq!(app::count_users_one(&conn).unwrap(), 0);
    }

    #[test]
    fn generated_functions_handle_quoted_keyword_table_names() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app::create_keyword_table_row(
            &mut conn,
            app::CreateKeywordTableRowParams {
                id: 10,
                name: "keyword",
            },
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].id, 10);
        assert_eq!(created[0].name, "keyword");
        assert_eq!(
            app::find_keyword_table_row_one(
                &conn,
                app::FindKeywordTableRowParams { id: 10 },
            )
            .unwrap(),
            "keyword"
        );
    }

    #[test]
    fn generated_functions_handle_reserved_word_column_names() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        conn.execute(
            r#"insert into typed_things (id, "type") values (1, 'primary')"#,
            [],
        )
        .unwrap();

        let rows = app::find_typed_thing(
            &conn,
            app::FindTypedThingParams { type_: "primary" },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].type_, "primary");
    }

    #[test]
    fn generated_functions_treat_sqlite_temporal_types_as_text() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_schema(&conn);

        let created = app::create_event(
            &mut conn,
            app::CreateEventParams {
                name: "launch",
                event_date: "2026-06-11",
                starts_at: "2026-06-11 09:30:00",
            },
        )
        .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].event_date, "2026-06-11");
        assert_eq!(created[0].starts_at, "2026-06-11 09:30:00");
        assert!(created[0].created_at.len() >= 19);

        let rows = app::list_events_since(
            &conn,
            app::ListEventsSinceParams {
                starts_at: "2026-06-11 00:00:00",
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_date, "2026-06-11");
        assert_eq!(rows[0].starts_at, "2026-06-11 09:30:00");
        assert_eq!(rows[0].created_at, created[0].created_at);
    }

}
"##,
    )
    .unwrap();
}
