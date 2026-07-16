use std::fs;
use std::path::Path;
use std::process::Command;

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

fn write_view(source_root: &Path, name: &str, columns: &str, sql: &str) {
    let directory = source_root.join("db_views");
    fs::create_dir_all(&directory).unwrap();
    let sql = sql.trim_end().trim_end_matches(';');
    fs::write(
        directory.join(format!("{name}.sql")),
        format!("CREATE VIEW {name} ({columns}) AS\n{sql};\n"),
    )
    .unwrap();
}

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
    assert!(stdout.contains("generate"));
    assert!(stdout.contains("migrate"));
    assert!(stdout.contains("bootstrap"));
    assert!(stdout.contains("seed"));
    assert!(stdout.contains("reset"));
    assert!(stdout.contains("dump-schema"));
    assert!(stdout.contains("audit-views"));
}

#[test]
fn dump_schema_writes_and_checks_a_deterministic_schema_file() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let schema = dir.path().join("db/schema.sql");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch("create table users (id integer primary key, name text not null);")
        .unwrap();
    drop(connection);

    let write = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("dump-schema")
        .arg("--database")
        .arg(&database)
        .arg("--output")
        .arg(&schema)
        .output()
        .unwrap();
    assert!(write.status.success());
    assert!(
        fs::read_to_string(&schema)
            .unwrap()
            .contains("CREATE TABLE users")
    );

    let check = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("dump-schema")
        .arg("--database")
        .arg(&database)
        .arg("--output")
        .arg(&schema)
        .arg("--check")
        .output()
        .unwrap();
    assert!(check.status.success());

    fs::write(&schema, "stale\n").unwrap();
    let stale = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("dump-schema")
        .arg("--database")
        .arg(&database)
        .arg("--output")
        .arg(&schema)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("schema dump is stale"));
}

#[test]
fn generate_reconciles_declared_views_before_analyzing_consumers() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE users (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                active INTEGER NOT NULL
             );
             INSERT INTO users (id, name, active) VALUES (1, 'Lucy', 1), (2, 'Mina', 0);",
        )
        .unwrap();
    drop(connection);

    let source_root = dir.path().join("src");
    let output = source_root.join("generated/sql");
    write_view(
        &source_root,
        "view_active_users",
        "id, name",
        "SELECT id, name FROM users WHERE active = 1",
    );
    let query = source_root.join("active_users");
    fs::create_dir_all(&source_root).unwrap();
    write_sql_file(
        &query,
        "list.sql",
        "SELECT id, name FROM view_active_users ORDER BY id",
    );

    let generated = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        generated.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&generated.stdout),
        String::from_utf8_lossy(&generated.stderr)
    );
    assert!(output.join("views.sql").exists());
    assert!(output.join("active_users.rs").exists());
    let connection = Connection::open(&database).unwrap();
    let names = connection
        .prepare("SELECT name FROM view_active_users ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(names, ["Lucy"]);
}

#[test]
fn audit_views_warns_and_strict_mode_fails_with_removal_sql() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let source_root = dir.path().join("src");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE VIEW view_stale_memberships (id) AS SELECT 1;")
        .unwrap();
    drop(connection);

    let warning = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("audit-views")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .output()
        .unwrap();
    assert!(warning.status.success());
    let stderr = String::from_utf8_lossy(&warning.stderr);
    assert!(stderr.contains("warning: database view `view_stale_memberships`"));
    assert!(stderr.contains("DROP VIEW IF EXISTS \"view_stale_memberships\";"));

    let strict = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("audit-views")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .arg("--deny-warnings")
        .output()
        .unwrap();
    assert!(!strict.status.success());
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(stderr.contains("error: database view `view_stale_memberships`"));
    assert!(stderr.contains("DROP VIEW IF EXISTS \"view_stale_memberships\";"));
}

#[test]
fn reset_installs_declared_views_before_running_seeds() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let source_root = dir.path().join("src");
    let migrations = dir.path().join("db/migrations");
    let seeds = dir.path().join("db/seeds");
    fs::create_dir_all(&migrations).unwrap();
    fs::create_dir_all(&seeds).unwrap();
    fs::write(
        migrations.join("001_create_users.sql"),
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    fs::write(
        seeds.join("001_seed_users.sql"),
        "INSERT INTO users (id, name) SELECT id, name FROM view_seed_users;",
    )
    .unwrap();
    write_view(
        &source_root,
        "view_seed_users",
        "id, name",
        "SELECT 1, 'Lucy'",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("reset")
        .arg("--database")
        .arg(&database)
        .arg("--migrations-dir")
        .arg(&migrations)
        .arg("--seeds-dir")
        .arg(&seeds)
        .arg("--source-root")
        .arg(&source_root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "reset failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let connection = Connection::open(&database).unwrap();
    let name: String = connection
        .query_row("SELECT name FROM users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "Lucy");
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
    let schema_output = dir.path().join("db/schema.sql");
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
migration_table = "schema_versions"
schema_output = "{}"
"#,
            database.display(),
            migrations_dir.display(),
            schema_output.display(),
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
        format!(
            "Applied 001_create_users\nWrote {}\n",
            schema_output.display()
        )
    );
    let conn = Connection::open(&database).unwrap();
    let version: String = conn
        .query_row("select version from schema_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, "001_create_users");
    assert!(
        fs::read_to_string(schema_output)
            .unwrap()
            .contains("CREATE TABLE users")
    );
}

#[test]
fn reset_uses_configured_bootstrap_and_seed_directories_and_writes_schema() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db/migrations")).unwrap();
    fs::create_dir_all(dir.path().join("db/bootstrap")).unwrap();
    fs::create_dir_all(dir.path().join("db/seeds")).unwrap();
    fs::write(
        dir.path().join("db/migrations/001_create_users.sql"),
        "create table users (id integer primary key, name text not null)",
    )
    .unwrap();
    fs::write(
        dir.path().join("db/bootstrap/admin.sql"),
        "insert into users (id, name) values (1, 'Admin')",
    )
    .unwrap();
    fs::write(
        dir.path().join("db/seeds/demo.sql"),
        "insert into users (id, name) values (2, 'Lucy')",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot]
database = "db/app.db"
migrations_dir = "db/migrations"
bootstrap_dir = "db/bootstrap"
seeds_dir = "db/seeds"
migration_table = "schema_versions"
schema_output = "db/schema.sql"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("reset")
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
        "Applied 001_create_users\nRan admin\nRan demo\nWrote db/schema.sql\n"
    );
    let conn = Connection::open(dir.path().join("db/app.db")).unwrap();
    let names = conn
        .prepare("select name from users order by id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(names, ["Admin", "Lucy"]);
    assert!(dir.path().join("db/schema.sql").exists());
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
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(
        &users,
        "find_user.sql",
        "select id, name from users where id = @id",
    );

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
            source_root.join("generated").display(),
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
    assert!(source_root.join("generated/users.rs").exists());
}

#[test]
fn generate_command_runs_init_sql_before_introspection() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    let init_sql = dir.path().join("db/marmot_init.sql");
    fs::create_dir_all(init_sql.parent().unwrap()).unwrap();
    fs::write(
        &init_sql,
        "
        create table users (
            id integer primary key,
            name text not null
        );
        ",
    )
    .unwrap();

    let source_root = dir.path().join("src");
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(
        &users,
        "find_user.sql",
        "select id, name from users where id = @id",
    );

    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot]
database = "{}"
source_root = "{}"
output = "{}"
init_sql = "{}"
"#,
            database.display(),
            source_root.display(),
            source_root.join("generated").display(),
            init_sql.display(),
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
    assert!(source_root.join("generated/users.rs").exists());
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
    let config_sql = config_source_root.join("users");
    let cli_sql = cli_source_root.join("users");
    fs::create_dir_all(&config_sql).unwrap();
    fs::create_dir_all(&cli_sql).unwrap();
    write_sql_file(&config_sql, "from_config.sql", "select name from users");
    write_sql_file(&cli_sql, "from_cli.sql", "select name from users");

    let config_output = config_source_root.join("generated");
    let cli_output = cli_source_root.join("generated");
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
    assert!(cli_output.join("users.rs").exists());
    let generated = fs::read_to_string(cli_output.join("users.rs")).unwrap();
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
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select name from users");
    let output_dir = source_root.join("generated");
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
    let generated = fs::read_to_string(output_dir.join("users.rs")).unwrap();
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
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select name from users");
    let output_dir = source_root.join("generated");

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
    assert!(output_dir.join("users.rs").exists());
}

#[test]
fn generate_command_rejects_output_outside_source_root() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src");
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select name from users");
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
fn generate_command_accepts_output_with_current_dir_segments() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let users = dir.path().join("src/users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select name from users");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg("src")
        .arg("--output")
        .arg("src/./generated/sql")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("src/generated/sql/users.rs").exists());
}

#[test]
fn generate_command_rejects_output_that_escapes_source_root_with_parent_segments() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let users = dir.path().join("src/users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select name from users");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg("src")
        .arg("--output")
        .arg("src/a/../../../outside")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("output path must be under source root"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("outside").exists());
}

#[test]
fn generate_command_succeeds_with_no_sql_directories() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src");
    fs::create_dir_all(&source_root).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(&source_root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("src/generated/sql/mod.rs").exists());
}

#[test]
fn generate_command_rejects_missing_database_configuration() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .env_remove("DATABASE_URL")
        .arg("generate")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing required database path"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_rejects_database_flag_without_value() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .env_remove("DATABASE_URL")
        .arg("generate")
        .arg("--database")
        .arg("--output")
        .arg("src/generated/sql")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("a value is required for '--database <DATABASE>'"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_rejects_empty_database_flag_value() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .env_remove("DATABASE_URL")
        .arg("generate")
        .arg("--database=")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("a value is required for '--database <DATABASE>'"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generate_command_reports_database_open_errors() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("missing/app.sqlite3");

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("generate")
        .arg("--database")
        .arg(&database)
        .arg("--source-root")
        .arg(dir.path().join("src"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not open sqlite database"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select id, name from users");
    let generated = source_root.join("generated");

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
    assert!(!generated.join("users.rs").exists());
}

#[test]
fn generate_command_check_passes_after_generation_and_fails_after_changes() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "check_name");

    let source_root = dir.path().join("src");
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select id, name from users");
    let generated = source_root.join("generated");

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
        source_root.join("users.sql"),
        "-- func: find_user\nselect id, name from users where id = @id",
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
fn generate_command_uses_named_database_config() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("db/app.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src/app");
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(
        &users,
        "find_user.sql",
        "select id, name from users where id = @id",
    );

    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot.databases.app]
path = "{}"
source_root = "{}"
output = "{}"
"#,
            database.display(),
            source_root.display(),
            source_root.join("generated").display(),
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
    assert!(dir.path().join("src/app/generated/users.rs").exists());
}

#[test]
fn generate_command_cli_source_root_overrides_named_database_source_root() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let config_source_root = dir.path().join("config_src/app");
    let config_users = config_source_root.join("users");
    write_sql_file(
        &config_users,
        "find_user.sql",
        "select 'FROM_CONFIG_SQL' as name from users",
    );

    let cli_source_root = dir.path().join("cli_src");
    let cli_users = cli_source_root.join("app/users");
    write_sql_file(
        &cli_users,
        "find_user.sql",
        "select 'FROM_CLI_SQL' as name from users",
    );

    let config = dir.path().join("marmot.toml");
    fs::write(
        &config,
        format!(
            r#"
[tools.marmot.databases.app]
path = "{}"
source_root = "{}"
"#,
            database.display(),
            config_source_root.display(),
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .arg("--config")
        .arg(&config)
        .arg("generate")
        .arg("--database-name")
        .arg("app")
        .arg("--source-root")
        .arg(&cli_source_root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let generated_path = cli_source_root.join("app/generated/sql/users.rs");
    let generated = fs::read_to_string(generated_path).unwrap();
    assert!(generated.contains("FROM_CLI_SQL"));
    assert!(!generated.contains("FROM_CONFIG_SQL"));
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

    let app = dir.path().join("src/app/users");
    let analytics_sql = dir.path().join("src/analytics/events");
    fs::create_dir_all(&app).unwrap();
    fs::create_dir_all(&analytics_sql).unwrap();
    write_sql_file(
        &app,
        "find_user.sql",
        "select id, name from users where id = @id",
    );
    write_sql_file(
        &analytics_sql,
        "list_events.sql",
        "select id, title from events",
    );

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
    assert!(dir.path().join("src/app/generated/sql/users.rs").exists());
    assert!(
        dir.path()
            .join("src/analytics/generated/sql/events.rs")
            .exists()
    );
}

#[test]
fn generate_command_runs_named_database_configs_when_database_url_is_set() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/env.sqlite"), "env_name");
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

    let app = dir.path().join("src/app/users");
    let analytics_sql = dir.path().join("src/analytics/events");
    fs::create_dir_all(&app).unwrap();
    fs::create_dir_all(&analytics_sql).unwrap();
    write_sql_file(
        &app,
        "find_user.sql",
        "select id, name from users where id = @id",
    );
    write_sql_file(
        &analytics_sql,
        "list_events.sql",
        "select id, title from events",
    );

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
        .env("DATABASE_URL", dir.path().join("db/env.sqlite"))
        .arg("generate")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "generate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("src/app/generated/sql/users.rs").exists());
    assert!(
        dir.path()
            .join("src/analytics/generated/sql/events.rs")
            .exists()
    );
}

#[test]
fn generate_command_rejects_named_database_output_collisions() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("db")).unwrap();
    create_users_database(&dir.path().join("db/app.sqlite"), "app_name");
    create_users_database(&dir.path().join("db/analytics.sqlite"), "analytics_name");

    let shared_sql = dir.path().join("src/shared/users");
    fs::create_dir_all(&shared_sql).unwrap();
    write_sql_file(&shared_sql, "find.sql", "select id, name from users");

    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot.databases.app]
path = "db/app.sqlite"
source_root = "src/shared"
output = "src/shared/generated/sql"

[tools.marmot.databases.analytics]
path = "db/analytics.sqlite"
source_root = "src/shared"
output = "src/shared/generated/sql"
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
    assert!(
        !dir.path()
            .join("src/shared/generated/sql/users.rs")
            .exists()
    );
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

    let app = dir.path().join("src/app/users");
    let analytics_sql = dir.path().join("src/analytics/events");
    fs::create_dir_all(&app).unwrap();
    fs::create_dir_all(&analytics_sql).unwrap();
    write_sql_file(
        &app,
        "find_user.sql",
        "select id, name from users where id = @id",
    );
    write_sql_file(
        &analytics_sql,
        "list_events.sql",
        "select id, title from events",
    );

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
    assert!(stdout.contains("users::find_user params=1 columns=2"));
    assert!(stdout.contains("events::list_events params=0 columns=2"));
}

#[test]
fn inspect_command_does_not_validate_generated_output() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    create_users_database(&database, "app_name");

    let source_root = dir.path().join("src");
    let users = source_root.join("users");
    fs::create_dir_all(&users).unwrap();
    write_sql_file(&users, "find_user.sql", "select name from users");
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
fn bootstrap_command_runs_only_the_configured_bootstrap_directory() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let bootstrap_dir = dir.path().join("db/bootstrap");
    let seeds_dir = dir.path().join("db/seeds");
    fs::create_dir_all(&bootstrap_dir).unwrap();
    fs::create_dir_all(&seeds_dir).unwrap();
    Connection::open(&database)
        .unwrap()
        .execute_batch("create table users (id integer primary key, name text not null);")
        .unwrap();
    fs::write(
        bootstrap_dir.join("admin.sql"),
        "insert into users (id, name) values (1, 'Admin')",
    )
    .unwrap();
    fs::write(
        seeds_dir.join("demo.sql"),
        "insert into users (id, name) values (2, 'Lucy')",
    )
    .unwrap();
    fs::write(
        dir.path().join("marmot.toml"),
        r#"
[tools.marmot]
database = "app.db"
bootstrap_dir = "db/bootstrap"
seeds_dir = "db/seeds"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_marmot"))
        .current_dir(dir.path())
        .arg("bootstrap")
        .output()
        .unwrap();

    assert!(output.status.success());
    let conn = Connection::open(&database).unwrap();
    let names = conn
        .prepare("select name from users order by id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(names, ["Admin"]);
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
