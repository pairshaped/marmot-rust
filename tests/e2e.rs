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
        output: dir.path().join("src/generated/sql"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    assert!(!config.output.exists(), "no central tree is created");

    let users_output = fs::read_to_string(config.source_root.join("users.generated.rs")).unwrap();
    assert!(users_output.contains("pub struct FindUserRow"));
    assert!(users_output.contains("pub struct FindUserParams"));
    assert!(users_output.contains("pub fn find_user(conn: &Connection, params: FindUserParams)"));
    assert!(users_output.contains("pub struct ListUsersRow"));
    assert!(users_output.contains("pub fn list_users(conn: &Connection)"));

    let items_output = fs::read_to_string(config.source_root.join("items.generated.rs")).unwrap();
    assert!(items_output.contains("pub struct ListItemsRow"));
    assert!(items_output.contains("pub struct ListItemsParams"));
    assert!(items_output.contains("pub fn list_items(conn: &Connection, params: ListItemsParams)"));
    assert!(items_output.contains("where owner_id = ?1"));
    assert!(items_output.contains("params![params.owner_id]"));

    let admin_items_output =
        fs::read_to_string(config.source_root.join("admin/items/index.generated.rs")).unwrap();
    assert!(admin_items_output.contains("pub struct ListAdminItemsRow"));
    assert!(admin_items_output.contains("pub fn list_admin_items(conn: &Connection)"));
}

#[test]
fn statement_labels_join_the_path_from_the_configured_source_root() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch("create table users (id integer primary key, name text not null) strict;")
        .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    write_sql_file(
        &source_root.join("admin/users"),
        "list_users.sql",
        "select id, name from users order by id",
    );
    let config = Config {
        database,
        source_root: source_root.clone(),
        output: dir.path().join("src/generated/sql"),
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };
    let project = analyze_project(&config).unwrap();
    let generated = source_root.join("admin/users.generated.rs");
    let sql_path = source_root.join("admin/users.sql");
    let label = format!("/* marmot:{}#list_users */", sql_path.display());

    emit_project(&config, &project).unwrap();
    assert!(!fs::read_to_string(&generated).unwrap().contains("marmot:"));

    marmot::emit::emit_project_with_binding_visibility(
        &config,
        &project,
        &Default::default(),
        &[],
        None,
        true,
    )
    .unwrap();
    assert!(fs::read_to_string(&generated).unwrap().contains(&label));
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

    let output = fs::read_to_string(config.source_root.join("settings.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("carts.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("tasks.generated.rs")).unwrap();
    assert!(output.contains("pub struct ListTasksParams<'a>"));
    assert!(output.contains("pub param: Option<i64>"));
    assert!(output.contains("pub param_2: Option<&'a str>"));
    assert!(output.contains("pub param_3: i64"));
    assert!(output.contains("pub fn list_tasks(conn: &Connection, params: ListTasksParams<'_>)"));
}

fn temporal_fixture_config(dir: &tempfile::TempDir) -> Config {
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

    let source_root = dir.path().join("runtime/src");
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

    Config {
        database,
        source_root,
        output: dir.path().join("runtime/src/generated/sql"),
        target: Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            strict_suffixes: true,
            module: Some("crate::generated::sql::temporal".to_string()),
            output: Some(dir.path().join("runtime/src/generated/sql/temporal.rs")),
            ..Default::default()
        },
    }
}

#[test]
fn strict_temporal_suffixes_generate_checked_boundary_types() {
    let dir = tempfile::tempdir().unwrap();
    let config = temporal_fixture_config(&dir);

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let mod_rs = fs::read_to_string(config.output.join("mod.rs")).unwrap();
    assert_eq!(mod_rs, "pub mod temporal;\n");
    assert!(config.output.join("temporal.rs").is_file());
    let app_rs = fs::read_to_string(config.source_root.join("app.generated.rs")).unwrap();
    // The emitted module is rustfmt-normalized, which drops the redundant `as temporal` alias.
    assert!(app_rs.contains("use crate::generated::sql::temporal;"));
    assert!(app_rs.contains("pub starts_at: temporal::DbDateTime"));
    assert!(app_rs.contains("pub registration_closes_on: Option<temporal::DbDate>"));
    assert!(app_rs.contains("pub starts_at: &'a temporal::DbDateTime"));
    assert!(app_rs.contains("pub registration_closes_on: Option<&'a temporal::DbDate>"));
    // rustfmt wraps long macro calls, so compare without whitespace.
    let compact_app_rs = app_rs.split_whitespace().collect::<String>();
    assert!(compact_app_rs.contains(
        &"params![params.title, params.starts_at.as_str(), params.registration_closes_on.map(|value| value.as_str())]"
            .split_whitespace()
            .collect::<String>()
    ));

    write_temporal_runtime_crate(dir.path());

    let output = isolated_cargo(&dir.path().join("runtime"), &["test"])
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
fn project_supplied_temporal_module_compiles_without_a_generated_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = temporal_fixture_config(&dir);
    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();
    let temporal_module = fs::read_to_string(config.output.join("temporal.rs")).unwrap();

    config.temporal.module = Some("crate::calendar_support::temporal".to_string());
    config.temporal.output = None;
    emit_project(&config, &project).unwrap();
    assert_eq!(
        fs::read_dir(&config.output).unwrap().count(),
        0,
        "no generated temporal declaration or module remains"
    );
    let app_rs = fs::read_to_string(config.source_root.join("app.generated.rs")).unwrap();
    assert!(app_rs.contains("use crate::calendar_support::temporal;"));

    write_temporal_runtime_crate(dir.path());
    let runtime = dir.path().join("runtime/src");
    fs::remove_dir_all(runtime.join("generated")).unwrap();
    fs::create_dir_all(runtime.join("calendar_support")).unwrap();
    fs::write(
        runtime.join("calendar_support/mod.rs"),
        "pub mod temporal;\n",
    )
    .unwrap();
    fs::write(
        runtime.join("calendar_support/temporal.rs"),
        temporal_module,
    )
    .unwrap();
    let lib = fs::read_to_string(runtime.join("lib.rs")).unwrap();
    fs::write(
        runtime.join("lib.rs"),
        lib.replace("pub mod generated;", "pub mod calendar_support;")
            .replace(
                "super::generated::sql::temporal",
                "super::calendar_support::temporal",
            ),
    )
    .unwrap();

    let output = isolated_cargo(&dir.path().join("runtime"), &["test"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "project-supplied temporal crate failed\nstdout:\n{}\nstderr:\n{}",
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
            module: Some("crate::calendar_support::temporal".to_string()),
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
            module: Some("crate::calendar_support::temporal".to_string()),
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.source_root.join("registrations.generated.rs")).unwrap();
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
            module: Some("crate::calendar_support::temporal".to_string()),
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.source_root.join("events.generated.rs")).unwrap();
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
            module: Some("crate::calendar_support::temporal".to_string()),
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.source_root.join("events.generated.rs")).unwrap();
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
            module: Some("crate::calendar_support::temporal".to_string()),
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.source_root.join("events.generated.rs")).unwrap();
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
            module: Some("crate::calendar_support::temporal".to_string()),
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
            module: Some("crate::calendar_support::temporal".to_string()),
            ..Default::default()
        },
    };

    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();

    let output = fs::read_to_string(config.source_root.join("registrations.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("products.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("products.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("products.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("products.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("products.generated.rs")).unwrap();
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

    let output = fs::read_to_string(config.source_root.join("products.generated.rs")).unwrap();
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

    let source_root = dir.path().join("runtime/src");
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
        "-- batch\ninsert into users (name, active, avatar, score, nickname) \
         values (@name, @active, @avatar, @score, @nickname)",
    );
    write_sql_file(
        &module_dir,
        "list_active_users.sql",
        "select id, name, active, avatar, score, nickname \
         from users where active = @active order by id",
    );
    write_sql_file(
        &module_dir,
        "count_users.sql",
        "-- cardinality: one\nselect count(*) from users",
    );
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
        "-- cardinality: both\nselect name from users where id = ?1 and active = ?2",
    );
    write_sql_file(
        &module_dir,
        "find_name_numbered_leading_zero.sql",
        "-- cardinality: one\nselect name from users where id = ?01 and active = ?02",
    );
    write_sql_file(
        &module_dir,
        "find_active_sparse_numbered.sql",
        "-- cardinality: one\nselect name from users where active = ?2 order by id",
    );
    write_sql_file(
        &module_dir,
        "find_name_mixed_positional.sql",
        "-- cardinality: one\nselect name from users where id = ?1 and active = ?",
    );
    write_sql_file(
        &module_dir,
        "find_name_named_numbered_same_slot.sql",
        "-- cardinality: one\nselect name from users where id = @id and id = ?1",
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
        "-- cardinality: one\nselect name from \"returning\" where id = @id",
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

    let output = isolated_cargo(&dir.path().join("runtime"), &["test"])
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
use marmot_generated_runtime_test::app;
use rusqlite::Connection;

fn main() {
    let conn = Connection::open_in_memory().unwrap();
    let _ = app::delete_user(&conn, app::DeleteUserParams { id: 1 });
}
"#,
    )
    .unwrap();
    let output = isolated_cargo(
        &dir.path().join("runtime"),
        &["check", "--bin", "immutable_mutation"],
    )
    .output()
    .unwrap();
    assert!(
        !output.status.success(),
        "immutable mutation unexpectedly compiled"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("MutationConnection"), "{stderr}");
}

fn isolated_cargo(runtime: &Path, arguments: &[&str]) -> Command {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("bin/cargo").is_file());
    let mut command = if let Some(root) = root {
        fs::create_dir_all(root.join("target")).unwrap();
        let mut command = Command::new("flock");
        // The parent test holds this checkout's wrapper lock and reservation.
        // Serialize nested fixtures through wrapper cleanup as well as Cargo.
        command
            .args(["--close"])
            .arg(root.join("target/marmot-fixtures.lock"))
            .arg(root.join("bin/cargo"))
            .current_dir(root)
            .args([
                "__sports_cargo_repository_lock_held",
                "__sports_cargo_checkout_lock_held",
            ])
            .args(arguments)
            .arg("--manifest-path")
            .arg(runtime.join("Cargo.toml"));
        command
    } else {
        // The exported public library has no monorepo wrapper.
        let mut command = Command::new(env!("CARGO"));
        command.current_dir(runtime).args(arguments);
        command
    };
    for variable in [
        "CARGO_BUILD_BUILD_DIR",
        "CARGO_TARGET_DIR",
        "RUSTC_WRAPPER",
        "SPORTS_CARGO_RUSTC_WRAPPER",
        "SPORTS_SHARED_BUILD_DIR",
        "SPORTS_WORKTREE_TARGET_DIR",
    ] {
        command.env_remove(variable);
    }
    if let Some(root) = root {
        command.env("CARGO_TARGET_DIR", root.join("target/marmot-fixtures"));
    }
    command
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
"#
        .to_owned()
            + &format!("marmot = {{ path = {:?} }}\n", env!("CARGO_MANIFEST_DIR")),
    )
    .unwrap();
    fs::write(runtime.join("src/generated/mod.rs"), "pub mod sql;\n").unwrap();
    fs::write(
        runtime.join("src/lib.rs"),
        r##"
pub mod generated;
#[path = "app.generated.rs"]
pub mod app;

#[cfg(test)]
mod tests {
    use super::app;
    use super::generated::sql::temporal::{DbDate, DbDateTime};
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
        for raw in ["2026-06-11 09:30:00", "2026-06-11 09:30:00.123456", "2026-06-11 09:30:00.123456789"] {
            let stamp=DbDateTime::new(raw).unwrap();
            let conn=Connection::open_in_memory().unwrap();
            let roundtrip:DbDateTime=conn.query_row("select ?1", [&stamp], |row|row.get(0)).unwrap();
            assert_eq!(roundtrip.as_str(),raw);
        }
        for invalid in ["2026-06-11 09:30:00.", "2026-06-11 09:30:00.1234567890", "2026-06-11 09:30:00.1Z", "2026-06-11 09:30:00.é"] {assert!(DbDateTime::new(invalid).is_err());}

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
    fs::create_dir_all(runtime.join("src")).unwrap();
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
"#
        .to_owned()
            + &format!("marmot = {{ path = {:?} }}\n", env!("CARGO_MANIFEST_DIR")),
    )
    .unwrap();
    fs::write(
        runtime.join("src/lib.rs"),
        r##"
#[path = "app.generated.rs"]
pub mod app;

#[cfg(test)]
mod tests {
    use super::app;
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
            app::find_name_numbered(&conn, app::FindNameNumberedParams { param: 1, param_2: true }).unwrap(),
            vec!["bob".to_string()]
        );
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

fn value_set_fixture(sql: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    Connection::open(&database)
        .unwrap()
        .execute_batch(
            "create table notices (id integer primary key, status text not null, previous text);
         create table other (id integer primary key, status text not null);",
        )
        .unwrap();
    let source_root = dir.path().join("src");
    write_sql_file(&source_root.join("notices"), "query.sql", sql);
    fs::write(
        source_root.join("value_sets.toml"),
        r#"
module = "crate::value_sets"
[sets.NoticeStatus]
values = ["draft", "published"]
columns = ["notices.status", "notices.previous"]
"#,
    )
    .unwrap();
    let config = Config {
        database,
        output: source_root.join("generated/sql"),
        source_root,
        target: Target::Rust,
        check: false,
        temporal: Default::default(),
    };
    (dir, config)
}

#[test]
fn value_sets_validate_literals_by_column_identity() {
    for (valid, invalid) in [
        (
            "select status from notices where status = 'draft'",
            "select status from notices where status = 'drfat'",
        ),
        (
            "select n.status from notices n where n.status = 'draft'",
            "select n.status from notices n where n.status = 'drfat'",
        ),
        (
            "select n.status from notices n join other o on n.id = o.id where n.status = 'draft' and o.status = 'unrelated'",
            "select n.status from notices n join other o on n.id = o.id where n.status = 'drfat'",
        ),
        (
            "select status from notices where status = @status and @status = 'draft'",
            "select status from notices where status = @status and @status = 'drfat'",
        ),
        (
            "select status from notices where status in ('draft', 'published')",
            "select status from notices where status in ('draft', 'drfat')",
        ),
        (
            "select case status when 'draft' then 1 else 0 end as result from notices",
            "select case status when 'drfat' then 1 else 0 end as result from notices",
        ),
        (
            "update notices set status = case when id = @id then 'draft' else 'published' end",
            "update notices set status = case when id = @id then 'drfat' else 'published' end",
        ),
        (
            "insert into notices (status) values ('draft')",
            "insert into notices (status) values ('drfat')",
        ),
        (
            "select case when id = 1 then status else 'draft' end as result from notices",
            "select case when id = 1 then status else 'drfat' end as result from notices",
        ),
    ] {
        let (_dir, config) = value_set_fixture(valid);
        analyze_project(&config).unwrap_or_else(|e| panic!("{valid}: {e}"));
        let (_dir, config) = value_set_fixture(invalid);
        let error = analyze_project(&config).expect_err(invalid).to_string();
        assert!(
            error.contains("value set") && error.contains("drfat"),
            "{error}"
        );
    }
}

#[test]
fn value_sets_generate_typed_nullable_params_and_results() {
    let (_dir, config) = value_set_fixture(
        "select status, previous from notices where status = @status and previous is @previous",
    );
    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();
    let output = fs::read_to_string(config.source_root.join("notices.generated.rs")).unwrap();
    assert!(
        output.contains("pub status: crate::value_sets::NoticeStatus"),
        "{output}"
    );
    assert!(
        output.contains("pub previous: Option<crate::value_sets::NoticeStatus>"),
        "{output}"
    );
    let (_dir, insert_config) =
        value_set_fixture("insert into notices (status, previous) values (@status, @previous)");
    let insert = analyze_project(&insert_config).unwrap();
    assert_eq!(
        insert.queries[0].parameters[0].rust_type(),
        "crate::value_sets::NoticeStatus"
    );
    assert_eq!(
        insert.queries[0].parameters[1].rust_type(),
        "Option<crate::value_sets::NoticeStatus>"
    );
    let types = fs::read_to_string(config.output.join("value_sets.rs")).unwrap();
    assert!(types.contains("pub enum NoticeStatus"));
}

#[test]
fn value_sets_fail_closed_for_transformed_or_conflicting_values() {
    for sql in [
        "select upper(status) as status from notices",
        "select status || 'suffix' as status from notices",
        "select status from notices where lower(status) = 'draft'",
    ] {
        let (_dir, config) = value_set_fixture(sql);
        let error = analyze_project(&config).expect_err(sql).to_string();
        assert!(error.contains("value set"), "{error}");
    }
}

#[test]
fn value_set_runtime_rejects_unknown_reads_and_growth_breaks_exhaustive_matches() {
    let (_dir, config) = value_set_fixture("select status, previous from notices where id = @id");
    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();
    let runtime = config.source_root.parent().unwrap();
    fs::write(
        runtime.join("Cargo.toml"),
        r#"
[package]
name = "marmot-value-set-runtime"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
rusqlite = { version = "0.37", features = ["bundled"] }
"#,
    )
    .unwrap();
    fs::write(config.source_root.join("lib.rs"), r#"
#[path = "generated/sql/value_sets.rs"]
pub mod value_sets;
#[path = "notices.generated.rs"]
pub mod notices;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_known_nullable_and_unknown_values() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("create table notices(id integer, status text, previous text);
            insert into notices values (1, 'draft', null), (2, 'legacy', 'published');").unwrap();
        let rows = notices::query(&conn, notices::QueryParams { id: 1 }).unwrap();
        assert_eq!(rows[0].status, value_sets::NoticeStatus::Draft);
        assert_eq!(rows[0].previous, None);
        let error = notices::query(&conn, notices::QueryParams { id: 2 }).unwrap_err();
        assert!(error.to_string().contains("legacy"), "{error}");
        conn.execute("insert into notices values (3, ?1, ?2)",
            rusqlite::params![value_sets::NoticeStatus::Published, Some(value_sets::NoticeStatus::Draft)]).unwrap();
        assert_eq!(notices::query(&conn, notices::QueryParams { id: 3 }).unwrap()[0].status,
            value_sets::NoticeStatus::Published);
    }
}
"#).unwrap();
    fs::write(
        config.source_root.join("main.rs"),
        r#"
use marmot_value_set_runtime::value_sets::NoticeStatus;
fn main() {
    let text = match NoticeStatus::Draft {
        NoticeStatus::Draft => "draft",
        NoticeStatus::Published => "published",
    };
    assert_eq!(text, "draft");
}
"#,
    )
    .unwrap();
    let result = isolated_cargo(runtime, &["test"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let declarations = config.source_root.join("value_sets.toml");
    let grown = fs::read_to_string(&declarations).unwrap().replace(
        "values = [\"draft\", \"published\"]",
        "values = [\"draft\", \"published\", \"archived\"]",
    );
    fs::write(declarations, grown).unwrap();
    emit_project(&config, &analyze_project(&config).unwrap()).unwrap();
    let result = isolated_cargo(runtime, &["check", "--bin", "marmot-value-set-runtime"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("non-exhaustive") && stderr.contains("Archived"),
        "{stderr}"
    );
}

#[test]
fn value_set_declarations_validate_identity_and_remove_stale_output() {
    for (old, new) in [
        ("notices.status", "notices.absent"),
        ("notices.status", "notices.id"),
        ("\"draft\", \"published\"", "\"draft\", \"draft\""),
        (
            "module = \"crate::value_sets\"",
            "module = \"self::value_sets\"",
        ),
    ] {
        let (_dir, config) = value_set_fixture("select status from notices");
        let path = config.source_root.join("value_sets.toml");
        let declaration = fs::read_to_string(&path).unwrap();
        assert!(declaration.contains(old));
        fs::write(path, declaration.replace(old, new)).unwrap();
        assert!(
            analyze_project(&config)
                .unwrap_err()
                .to_string()
                .contains("value set")
        );
    }
    let (_dir, config) = value_set_fixture("select status from notices");
    emit_project(&config, &analyze_project(&config).unwrap()).unwrap();
    let output = config.output.join("value_sets.rs");
    assert!(output.exists());
    fs::remove_file(config.source_root.join("value_sets.toml")).unwrap();
    emit_project(&config, &analyze_project(&config).unwrap()).unwrap();
    assert!(!output.exists());
}

#[test]
fn value_set_conflicting_parameter_evidence_is_rejected() {
    let (_dir, config) = value_set_fixture(
        "select n.status from notices n join other o on n.id = o.id where n.status = @status and o.status = @status",
    );
    let path = config.source_root.join("value_sets.toml");
    let mut declaration = fs::read_to_string(&path).unwrap();
    declaration
        .push_str("\n[sets.OtherStatus]\nvalues = [\"ready\"]\ncolumns = [\"other.status\"]\n");
    fs::write(path, declaration).unwrap();
    let error = analyze_project(&config).unwrap_err().to_string();
    assert!(
        error.contains("conflicting") && error.contains("value set"),
        "{error}"
    );
}

#[test]
fn value_sets_validate_cte_sources_and_column_substitutions() {
    for (sql, valid) in [
        (
            "with typed as (select status from notices where status = 'draft') select status from typed",
            true,
        ),
        (
            "with typed as (select status from notices where status = 'drfat') select status from typed",
            false,
        ),
        (
            "-- columns: status, previous\nupdate notices set {{column}} = @value where id = @id",
            true,
        ),
    ] {
        let (_dir, config) = value_set_fixture(sql);
        let result = analyze_project(&config);
        if valid {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().to_string().contains("drfat"));
        }
    }
    let (_dir, config) = value_set_fixture(
        "-- columns: status, id\nupdate notices set {{column}} = @value where id = @id",
    );
    assert!(
        analyze_project(&config)
            .unwrap_err()
            .to_string()
            .contains("value set")
    );
}

#[test]
fn value_sets_follow_view_column_identity_without_rewriting_storage() {
    for (literal, valid) in [("draft", true), ("drfat", false)] {
        let (_dir, config) = value_set_fixture(&format!(
            "select status from notice_view where status = '{literal}'"
        ));
        Connection::open(&config.database)
            .unwrap()
            .execute_batch("create view notice_view as select status from notices;")
            .unwrap();
        let result = analyze_project(&config);
        if valid {
            let project = result.unwrap();
            assert_eq!(
                project.queries[0].columns[0].rust_type(),
                "crate::value_sets::NoticeStatus"
            );
        } else {
            assert!(result.unwrap_err().to_string().contains("drfat"));
        }
    }
}

#[test]
fn value_sets_keep_window_partitions_and_case_sorting_typed() {
    let (_dir, config) = value_set_fixture(
        "with ranked as (select status, row_number() over (partition by status order by id desc) as position from notices where status in (@draft, @published)) select status from ranked where position <= @limit order by case status when @draft then 0 when @published then 1 end asc",
    );
    let project = analyze_project(&config).unwrap();
    assert_eq!(
        project.queries[0].columns[0].column_type.rust_type(),
        "crate::value_sets::NoticeStatus"
    );
    assert_eq!(
        project.queries[0]
            .parameters
            .iter()
            .find(|p| p.name == "draft")
            .unwrap()
            .column_type
            .rust_type(),
        "crate::value_sets::NoticeStatus"
    );
}

#[test]
fn value_sets_reject_predicates_in_text_assignments() {
    for sql in [
        "insert into notices (status) values (1 = 1)",
        "update notices set status = (id in (1, 2))",
    ] {
        let (_dir, config) = value_set_fixture(sql);
        let error = analyze_project(&config).unwrap_err().to_string();
        assert!(error.contains("value set"), "{error}");
    }
}

#[test]
fn value_sets_do_not_silently_bypass_unreviewed_query_shapes() {
    for sql in [
        "select status from notices union select 'drfat'",
        "select 'draft' union select status from notices",
        "select (select n.status from notices n where n.status='drfat') as status from other",
        "delete from notices returning case status when 'drfat' then 'draft' else status end as status",
    ] {
        let (_dir, config) = value_set_fixture(sql);
        assert!(
            analyze_project(&config)
                .unwrap_err()
                .to_string()
                .contains("value set"),
            "{sql}"
        );
    }
}
