use std::fs;
use std::process::Command;

use rusqlite::Connection;

#[test]
fn help_does_not_require_database_configuration() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("--help")
        .current_dir(dir.path())
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "help failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: marmot"));
}

#[test]
fn unknown_command_shows_help_without_database_configuration() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("wat")
        .current_dir(dir.path())
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unrecognized subcommand 'wat'"));
    assert!(stderr.contains("Usage: marmot"));
}

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
fn migrate_command_uses_marmot_toml_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let migrations_dir = dir.path().join("db/migrations/custom");
    fs::create_dir_all(&migrations_dir).unwrap();
    fs::write(
        migrations_dir.join("001_create_users.sql"),
        "create table users (id integer primary key)",
    )
    .unwrap();
    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot]
database = "{}"
migrations_dir = "{}"
"#,
            database.display(),
            migrations_dir.display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("--config")
        .arg(&config)
        .arg("migrate")
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
}

#[test]
fn generate_command_uses_marmot_toml_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "
        create table users (
            id integer primary key,
            name text not null
        );
        ",
    )
    .unwrap();
    drop(conn);

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(
        users_sql.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();

    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot]
database = "{}"
source_root = "{}"
output = "{}"
"#,
            database.display(),
            source_root.display(),
            source_root.join("generated/sql").display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("--config")
        .arg(&config)
        .arg("generate")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(source_root.join("generated/sql/users_sql.rs").exists());
}

#[test]
fn generate_command_cli_flags_override_marmot_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_database = dir.path().join("config.sqlite3");
    let cli_database = dir.path().join("cli.sqlite3");
    create_users_database(&config_database, "config_name");
    create_users_database(&cli_database, "cli_name");

    let config_source_root = dir.path().join("config_src");
    let cli_source_root = dir.path().join("cli_src");
    let config_sql = config_source_root.join("users/sql");
    let cli_sql = cli_source_root.join("users/sql");
    fs::create_dir_all(&config_sql).unwrap();
    fs::create_dir_all(&cli_sql).unwrap();
    fs::write(config_sql.join("from_config.sql"), "select name from users").unwrap();
    fs::write(cli_sql.join("from_cli.sql"), "select name from users").unwrap();

    let config_output = config_source_root.join("generated/sql");
    let cli_output = cli_source_root.join("generated/sql");
    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot]
database = "{}"
source_root = "{}"
output = "{}"
"#,
            config_database.display(),
            config_source_root.display(),
            config_output.display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("--config")
        .arg(&config)
        .arg("generate")
        .arg("--database")
        .arg(&cli_database)
        .arg("--source-root")
        .arg(&cli_source_root)
        .arg("--output")
        .arg(&cli_output)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!config_output.exists());
    assert!(cli_output.join("users_sql.rs").exists());
    let generated = fs::read_to_string(cli_output.join("users_sql.rs")).unwrap();
    assert!(generated.contains("FROM_CLI_SQL"));
}

#[test]
fn generate_command_database_url_overrides_marmot_toml_database() {
    let dir = tempfile::tempdir().unwrap();
    let config_database = dir.path().join("config.sqlite3");
    let env_database = dir.path().join("env.sqlite3");
    create_users_database(&config_database, "config_name");
    create_users_database(&env_database, "env_name");

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(users_sql.join("find_user.sql"), "select name from users").unwrap();
    let output_dir = source_root.join("generated/sql");
    fs::write(
        dir.path().join("marmot.toml"),
        format!(
            r#"
[tools.marmot]
database = "{}"
source_root = "{}"
output = "{}"
"#,
            config_database.display(),
            source_root.display(),
            output_dir.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .env("DATABASE_URL", &env_database)
        .arg("generate")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = fs::read_to_string(output_dir.join("users_sql.rs")).unwrap();
    assert!(generated.contains("FIND_USER_SQL"));
}

#[test]
fn generate_command_cli_database_overrides_database_url() {
    let dir = tempfile::tempdir().unwrap();
    let env_database = dir.path().join("env.sqlite3");
    let cli_database = dir.path().join("cli.sqlite3");
    create_users_database(&env_database, "env_name");
    create_users_database(&cli_database, "cli_name");

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(users_sql.join("find_user.sql"), "select name from users").unwrap();
    let output_dir = source_root.join("generated/sql");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .env("DATABASE_URL", &env_database)
        .arg("generate")
        .arg("--database")
        .arg(&cli_database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&output_dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_dir.join("users_sql.rs").exists());
}

#[test]
fn generate_command_rejects_output_outside_source_root() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(users_sql.join("find_user.sql"), "select name from users").unwrap();
    let output_dir = dir.path().join("generated/sql");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&output_dir)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("output path must be under source root"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output_dir.exists());
}

#[test]
fn generate_command_rejects_cli_database_with_named_database_selection() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.app]
path = "db/app.db"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .arg("--database")
        .arg("tmp/test.db")
        .arg("--database-name")
        .arg("app")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--database cannot be used with --database-name"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_rejects_malformed_marmot_toml() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot
database = "dev.sqlite"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not parse config"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_rejects_named_database_array_without_name() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[[tools.marmot.databases]]
path = "db/app.db"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("missing or empty name in [[tools.marmot.databases]]"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_check_reports_missing_generated_files_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "check_name");

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(
        users_sql.join("find_user.sql"),
        "select id, name from users",
    )
    .unwrap();
    let generated = source_root.join("generated/sql");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&generated)
        .arg("--check")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("generated file is stale"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!generated.join("users_sql.rs").exists());
}

#[test]
fn generate_command_check_passes_after_generation_and_fails_after_changes() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "check_name");

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(
        users_sql.join("find_user.sql"),
        "select id, name from users",
    )
    .unwrap();
    let generated = source_root.join("generated/sql");

    let generate = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&generated)
        .output()
        .unwrap();
    assert!(
        generate.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&generate.stdout),
        String::from_utf8_lossy(&generate.stderr)
    );

    let check = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&generated)
        .arg("--check")
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );

    fs::write(
        users_sql.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();
    let stale_check = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&generated)
        .arg("--check")
        .output()
        .unwrap();

    assert!(!stale_check.status.success());
    assert!(
        String::from_utf8_lossy(&stale_check.stderr).contains("generated file is stale"),
        "stderr:\n{}",
        String::from_utf8_lossy(&stale_check.stderr)
    );
}

#[test]
fn generate_command_rejects_missing_configured_sql_dir() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "missing_sql_dir");
    fs::write(
        dir.path().join("marmot.toml"),
        format!(
            r#"
[tools.marmot]
database = "{}"
source_root = "src"
sql_dir = "src/sql"
output = "src/generated/sql"
"#,
            database.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing SQL directory"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_rejects_configured_sql_dir_that_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "bad_sql_dir");
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/sql"), "not a directory").unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        format!(
            r#"
[tools.marmot]
database = "{}"
source_root = "src"
sql_dir = "src/sql"
output = "src/generated/sql"
"#,
            database.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("SQL path is not a directory"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_uses_named_database_config() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("db/app.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src");
    let sql_dir = dir.path().join("src/sql/app");
    fs::create_dir_all(&sql_dir).unwrap();
    fs::write(
        sql_dir.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();

    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot]
source_root = "{}"

[tools.marmot.databases.app]
path = "{}"
sql_dir = "{}"
output = "{}"
"#,
            source_root.display(),
            database.display(),
            sql_dir.display(),
            dir.path().join("src/generated/sql/app").display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("--config")
        .arg(&config)
        .arg("generate")
        .arg("--database-name")
        .arg("app")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("src/generated/sql/app/sql.rs").exists());
}

#[test]
fn generate_command_appends_named_database_to_global_dirs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/primary.sqlite"), "primary_name");

    let sql_dir = dir.path().join("src/database_sql/primary");
    fs::create_dir_all(&sql_dir).unwrap();
    fs::write(
        sql_dir.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot]
sql_dir = "src/database_sql"
output = "src/generated/database_sql"

[[tools.marmot.databases]]
name = "primary"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .arg("--database-name")
        .arg("primary")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        dir.path()
            .join("src/generated/database_sql/primary/sql.rs")
            .exists()
    );
}

#[test]
fn generate_command_does_not_double_named_database_global_dirs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/curling.sqlite"), "curling_name");

    let sql_dir = dir.path().join("src/sql/curling");
    fs::create_dir_all(&sql_dir).unwrap();
    fs::write(
        sql_dir.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot]
sql_dir = "src/sql/curling"
output = "src/generated/sql/curling"

[[tools.marmot.databases]]
name = "curling"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .arg("--database-name")
        .arg("curling")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("src/generated/sql/curling/sql.rs").exists());
    assert!(
        !dir.path()
            .join("src/generated/sql/curling/curling/sql.rs")
            .exists()
    );
}

#[test]
fn generate_command_runs_all_named_database_configs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/app.sqlite"), "app_name");
    let analytics = Connection::open(dir.path().join("db/analytics.sqlite")).unwrap();
    analytics
        .execute_batch(
            "
            create table events (
                id integer primary key,
                title text not null
            );
            ",
        )
        .unwrap();
    drop(analytics);

    let app_sql = dir.path().join("src/sql/app");
    let analytics_sql = dir.path().join("src/sql/analytics");
    fs::create_dir_all(&app_sql).unwrap();
    fs::create_dir_all(&analytics_sql).unwrap();
    fs::write(
        app_sql.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();
    fs::write(
        analytics_sql.join("list_events.sql"),
        "select id, title from events",
    )
    .unwrap();

    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[[tools.marmot.databases]]
name = "app"

[[tools.marmot.databases]]
name = "analytics"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("src/generated/sql/app/sql.rs").exists());
    assert!(
        dir.path()
            .join("src/generated/sql/analytics/sql.rs")
            .exists()
    );
}

#[test]
fn generate_command_rejects_named_database_output_collisions() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/app.sqlite"), "app_name");
    let analytics = Connection::open(dir.path().join("db/analytics.sqlite")).unwrap();
    analytics
        .execute_batch(
            "
            create table events (
                id integer primary key,
                title text not null
            );
            ",
        )
        .unwrap();
    drop(analytics);

    let app_sql = dir.path().join("src/sql/app");
    let analytics_sql = dir.path().join("src/sql/analytics");
    fs::create_dir_all(&app_sql).unwrap();
    fs::create_dir_all(&analytics_sql).unwrap();
    fs::write(app_sql.join("find.sql"), "select id, name from users").unwrap();
    fs::write(
        analytics_sql.join("find.sql"),
        "select id, title from events",
    )
    .unwrap();

    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.app]
path = "db/app.sqlite"
sql_dir = "src/sql/app"
output = "src/generated/sql"

[tools.marmot.databases.analytics]
path = "db/analytics.sqlite"
sql_dir = "src/sql/analytics"
output = "src/generated/sql"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("generated output collision"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("src/generated/sql/sql.rs").exists());
}

#[test]
fn inspect_command_runs_all_named_database_configs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/app.sqlite"), "app_name");
    let analytics = Connection::open(dir.path().join("db/analytics.sqlite")).unwrap();
    analytics
        .execute_batch(
            "
            create table events (
                id integer primary key,
                title text not null
            );
            ",
        )
        .unwrap();
    drop(analytics);

    let app_sql = dir.path().join("src/sql/app");
    let analytics_sql = dir.path().join("src/sql/analytics");
    fs::create_dir_all(&app_sql).unwrap();
    fs::create_dir_all(&analytics_sql).unwrap();
    fs::write(
        app_sql.join("find_user.sql"),
        "select id, name from users where id = @id",
    )
    .unwrap();
    fs::write(
        analytics_sql.join("list_events.sql"),
        "select id, title from events",
    )
    .unwrap();

    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[[tools.marmot.databases]]
name = "app"

[[tools.marmot.databases]]
name = "analytics"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("inspect")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "inspect failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("sql::find_user params=1 columns=2"));
    assert!(stdout.contains("sql::list_events params=0 columns=2"));
}

#[test]
fn inspect_command_does_not_validate_generated_output() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src");
    let users_sql = source_root.join("users/sql");
    fs::create_dir_all(&users_sql).unwrap();
    fs::write(users_sql.join("find_user.sql"), "select name from users").unwrap();
    let output_dir = dir.path().join("generated/sql");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("inspect")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&output_dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "inspect failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output_dir.exists());
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

#[test]
fn seed_command_runs_all_named_database_configs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db/seeds/app")).unwrap();
    fs::create_dir_all(dir.path().join("db/seeds/analytics")).unwrap();
    fs::write(
        dir.path().join("db/seeds/app/001_create_app_table.sql"),
        "create table app_table (id integer primary key)",
    )
    .unwrap();
    fs::write(
        dir.path()
            .join("db/seeds/analytics/001_create_analytics_table.sql"),
        "create table analytics_table (id integer primary key)",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.app]
path = "db/app.db"
seeds_dir = "db/seeds/app"

[tools.marmot.databases.analytics]
path = "db/analytics.db"
seeds_dir = "db/seeds/analytics"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("seed")
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
        "Ran 001_create_analytics_table\nRan 001_create_app_table\n"
    );

    let app = Connection::open(dir.path().join("db/app.db")).unwrap();
    assert!(table_names(&app).contains(&"app_table".to_string()));
    let analytics = Connection::open(dir.path().join("db/analytics.db")).unwrap();
    assert!(table_names(&analytics).contains(&"analytics_table".to_string()));
}

#[test]
fn migrate_command_runs_all_named_database_configs() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db/migrations/app")).unwrap();
    fs::create_dir_all(dir.path().join("db/migrations/analytics")).unwrap();
    fs::write(
        dir.path()
            .join("db/migrations/app/001_create_app_table.sql"),
        "create table app_table (id integer primary key)",
    )
    .unwrap();
    fs::write(
        dir.path()
            .join("db/migrations/analytics/001_create_analytics_table.sql"),
        "create table analytics_table (id integer primary key)",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.app]
path = "db/app.db"
migrations_dir = "db/migrations/app"

[tools.marmot.databases.analytics]
path = "db/analytics.db"
migrations_dir = "db/migrations/analytics"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("migrate")
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
        "Applied 001_create_analytics_table\nApplied 001_create_app_table\n"
    );

    let app = Connection::open(dir.path().join("db/app.db")).unwrap();
    assert!(table_names(&app).contains(&"app_table".to_string()));
    let analytics = Connection::open(dir.path().join("db/analytics.db")).unwrap();
    assert!(table_names(&analytics).contains(&"analytics_table".to_string()));
}

#[test]
fn migrate_command_uses_named_database_default_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db/migrations/primary")).unwrap();
    fs::write(
        dir.path()
            .join("db/migrations/primary/001_create_users.sql"),
        "create table users (id integer primary key)",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[[tools.marmot.databases]]
name = "primary"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("migrate")
        .arg("--database-name")
        .arg("primary")
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

    let conn = Connection::open(dir.path().join("db/primary.sqlite")).unwrap();
    assert!(table_names(&conn).contains(&"users".to_string()));
}

#[test]
fn generate_command_rejects_unknown_database_name() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.app]
path = "db/app.db"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .arg("--database-name")
        .arg("missing")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown database name missing"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_rejects_mixed_top_level_and_named_database_config() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot]
database = "db/app.db"

[tools.marmot.databases.analytics]
path = "db/analytics.db"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("mixed database configuration"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn reset_command_drops_database_then_runs_migrations_and_seeds() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let migrations_dir = dir.path().join("db/migrations");
    let seeds_dir = dir.path().join("db/seeds");
    fs::create_dir_all(&migrations_dir).unwrap();
    fs::create_dir_all(&seeds_dir).unwrap();
    fs::write(
        migrations_dir.join("001_create_users.sql"),
        "create table users (id integer primary key, name text not null)",
    )
    .unwrap();
    fs::write(
        seeds_dir.join("001_add_lucy.sql"),
        "insert into users (id, name) values (1, 'Lucy')",
    )
    .unwrap();

    let stale = Connection::open(&database).unwrap();
    stale
        .execute("create table stale (id integer primary key)", [])
        .unwrap();
    drop(stale);

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("reset")
        .arg("--database")
        .arg(&database)
        .arg("--migrations-dir")
        .arg(&migrations_dir)
        .arg("--seeds-dir")
        .arg(&seeds_dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "reset failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Applied 001_create_users\nRan 001_add_lucy\n"
    );

    let conn = Connection::open(&database).unwrap();
    let tables = table_names(&conn);
    assert_eq!(tables, ["schema_migrations", "users"]);
    let name: String = conn
        .query_row("select name from users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "Lucy");
}

#[test]
fn reset_command_uses_named_database_config() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db/migrations/curling")).unwrap();
    fs::create_dir_all(dir.path().join("db/seeds/curling")).unwrap();
    fs::write(
        dir.path()
            .join("db/migrations/curling/001_create_users.sql"),
        "create table users (id integer primary key, name text not null)",
    )
    .unwrap();
    fs::write(
        dir.path().join("db/seeds/curling/001_add_lucy.sql"),
        "insert into users (id, name) values (1, 'Lucy')",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.curling]
path = "db/curling.db"
migrations_dir = "db/migrations/curling"
seeds_dir = "db/seeds/curling"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("reset")
        .arg("--database-name")
        .arg("curling")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "reset failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let conn = Connection::open(dir.path().join("db/curling.db")).unwrap();
    let name: String = conn
        .query_row("select name from users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "Lucy");
}

fn table_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("select name from sqlite_master where type = 'table' order by name")
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn create_users_database(path: &std::path::Path, name: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "
        create table users (
            id integer primary key,
            name text not null
        );
        ",
    )
    .unwrap();
    conn.execute("insert into users (name) values (?1)", [name])
        .unwrap();
}
