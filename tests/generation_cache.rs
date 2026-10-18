use std::fs;
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for path in ["src", "views", "db/migrations", "db/bootstrap"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        fs::write(
            root.path().join("src/items.rs"),
            "#[path = \"items.generated.rs\"]\nmod sql;\n",
        )
        .unwrap();
        fs::write(
            root.path().join("src/items.sql"),
            "-- func: list_items\nselect id, name from items",
        )
        .unwrap();
        fs::write(root.path().join("marmot.toml"), "[tools.marmot]\ndatabase = \"db/app.sqlite\"\nsource_root = \"src\"\nview_source_root = \"views\"\noutput = \"src/generated/sql\"\nmigrations_dir = \"db/migrations\"\nbootstrap_dir = \"db/bootstrap\"\n").unwrap();
        // The CLI's database-file identity is the boundary under test. Start
        // from a fresh in-memory template, never an application database.
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "create table items (id integer primary key, name text not null) strict;",
            )
            .unwrap();
        connection
            .execute(
                "vacuum into ?1",
                [root.path().join("db/app.sqlite").to_str().unwrap()],
            )
            .unwrap();
        Self { root }
    }

    fn generate(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_marmot"))
            .arg("generate")
            .args(extra)
            .current_dir(self.root.path())
            .env_remove("DATABASE_URL")
            .output()
            .unwrap()
    }

    fn run(&self, extra: &[&str]) -> String {
        let result = self.generate(extra);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    }

    fn file(&self, path: &str) -> std::path::PathBuf {
        self.root.path().join(path)
    }
}

#[test]
fn unchanged_generation_skips_and_preserves_published_file_times() {
    let fixture = Fixture::new();
    assert!(!fixture.run(&[]).contains("Skipped"));
    let output = fixture.file("src/items.generated.rs");
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::open(&output)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
    assert!(fixture.run(&[]).contains("Skipped"));
    assert_eq!(fs::metadata(output).unwrap().modified().unwrap(), time);
}

#[test]
fn each_input_class_invalidates_a_warm_receipt() {
    for change in [
        "sql",
        "sql_added",
        "sql_removed",
        "module_added",
        "module_removed",
        "migration",
        "bootstrap",
        "init",
        "config",
        "temporal",
        "schema",
        "reset",
    ] {
        let fixture = Fixture::new();
        if matches!(change, "sql_removed" | "module_removed") {
            fs::write(fixture.file("src/extra.rs"), "").unwrap();
            fs::write(
                fixture.file("src/extra.sql"),
                "-- func: extra\nselect 1 as number",
            )
            .unwrap();
        }
        if change == "module_added" {
            fs::write(
                fixture.file("src/extra.sql"),
                "-- func: extra\nselect 1 as number",
            )
            .unwrap();
        }
        if change == "init" {
            let mut config = fs::read_to_string(fixture.file("marmot.toml")).unwrap();
            config.push_str("init_sql = \"db/init.sql\"\n");
            fs::write(fixture.file("marmot.toml"), config).unwrap();
            fs::write(fixture.file("db/init.sql"), "-- initial\n").unwrap();
        }
        fixture.run(&[]);
        assert!(
            fixture.run(&[]).contains("Skipped"),
            "{change}: establish warm skip"
        );
        match change {
            "sql" => fs::write(
                fixture.file("src/items.sql"),
                "-- func: list_items\nselect id from items",
            )
            .unwrap(),
            "sql_added" => {
                fs::write(fixture.file("src/extra.rs"), "").unwrap();
                fs::write(
                    fixture.file("src/extra.sql"),
                    "-- func: extra\nselect 1 as number",
                )
                .unwrap();
            }
            "sql_removed" => fs::remove_file(fixture.file("src/extra.sql")).unwrap(),
            "module_added" => fs::write(fixture.file("src/extra.rs"), "").unwrap(),
            "module_removed" => fs::remove_file(fixture.file("src/extra.rs")).unwrap(),
            "migration" => fs::write(
                fixture.file("db/migrations/001.sql"),
                "-- migration input\n",
            )
            .unwrap(),
            "bootstrap" => {
                fs::write(fixture.file("db/bootstrap/001.sql"), "-- bootstrap input\n").unwrap()
            }
            "init" => fs::write(fixture.file("db/init.sql"), "-- changed init\n").unwrap(),
            "config" => {
                let mut config = fs::read_to_string(fixture.file("marmot.toml")).unwrap();
                config.push_str("serialize_modules = [\"items\"]\n");
                fs::write(fixture.file("marmot.toml"), config).unwrap();
            }
            "temporal" => {
                let mut config = fs::read_to_string(fixture.file("marmot.toml")).unwrap();
                config.push_str("[tools.marmot.temporal]\nstrict_suffixes = true\nmodule = \"crate::generated::sql::temporal\"\n");
                fs::write(fixture.file("marmot.toml"), config).unwrap();
            }
            "schema" => rusqlite::Connection::open(fixture.file("db/app.sqlite"))
                .unwrap()
                .execute_batch("alter table items add column extra text;")
                .unwrap(),
            "reset" => {
                fs::remove_file(fixture.file("db/app.sqlite")).unwrap();
                let template = rusqlite::Connection::open_in_memory().unwrap();
                template
                    .execute_batch(
                        "create table items (id integer primary key, name text not null) strict;",
                    )
                    .unwrap();
                template
                    .execute(
                        "vacuum into ?1",
                        [fixture.file("db/app.sqlite").to_str().unwrap()],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !fixture.run(&[]).contains("Skipped"),
            "{change}: full generation"
        );
        assert!(
            fixture.run(&[]).contains("Skipped"),
            "{change}: new receipt becomes warm"
        );
    }
}

#[test]
fn views_in_distinct_roots_and_effective_cli_overrides_invalidate() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.file("views/db_views")).unwrap();
    fixture.run(&[]);
    assert!(fixture.run(&[]).contains("Skipped"));
    let view = fixture.file("views/db_views/view_items.sql");
    fs::write(
        &view,
        "create view view_items (id, name) as select id, name from items;",
    )
    .unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    let database = rusqlite::Connection::open(fixture.file("db/app.sqlite")).unwrap();
    let before: i64 = database
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .unwrap();
    assert!(fixture.run(&[]).contains("Skipped"));
    let after: i64 = database
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .unwrap();
    assert_eq!(before, after, "skip does not reconcile installed views");
    fs::write(
        &view,
        "create view view_items (id, name) as select id, name from items where id > 0;",
    )
    .unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(fixture.run(&[]).contains("Skipped"));
    fs::remove_file(view).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    // The owner is unchanged; only the effective CLI output directory differs.
    assert!(
        !fixture
            .run(&["--output", "src/generated/alternate"])
            .contains("Skipped")
    );
    assert!(
        fixture
            .run(&["--output", "src/generated/alternate"])
            .contains("Skipped")
    );
}

#[test]
fn warm_receipts_repair_output_damage_and_force_and_check_bypass_them() {
    for damage in ["deleted", "modified", "obsolete"] {
        let fixture = Fixture::new();
        fixture.run(&[]);
        let output = fixture.file("src/items.generated.rs");
        let expected = fs::read(&output).unwrap();
        assert!(fixture.run(&[]).contains("Skipped"));
        match damage {
            "deleted" => fs::remove_file(&output).unwrap(),
            "modified" => fs::write(&output, "stale").unwrap(),
            "obsolete" => fs::write(fixture.file("src/obsolete.generated.rs"), "obsolete").unwrap(),
            _ => unreachable!(),
        }
        assert!(!fixture.run(&[]).contains("Skipped"), "{damage}");
        assert_eq!(fs::read(&output).unwrap(), expected);
        assert!(!fixture.file("src/obsolete.generated.rs").exists());
        assert!(fixture.run(&[]).contains("Skipped"));
        assert!(!fixture.run(&["--force"]).contains("Skipped"));
        fs::write(&output, "stale").unwrap();
        let check = fixture.generate(&["--check"]);
        assert!(!check.status.success());
        assert!(String::from_utf8_lossy(&check.stderr).contains("generated file is stale"));
        assert_eq!(fs::read_to_string(output).unwrap(), "stale");
    }
}

#[test]
fn cache_loss_corruption_and_failed_runs_never_authorize_a_skip() {
    let fixture = Fixture::new();
    fs::write(
        fixture.file("src/items.sql"),
        "-- func: invalid\nselect * from missing",
    )
    .unwrap();
    assert!(!fixture.generate(&[]).status.success());
    assert!(!fixture.file(".local/marmot").exists());
    fs::write(
        fixture.file("src/items.sql"),
        "-- func: list_items\nselect id, name from items",
    )
    .unwrap();
    fixture.run(&[]);
    let cache = fs::read_dir(fixture.file(".local/marmot"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let before = fs::read(&cache).unwrap();
    fs::write(
        fixture.file("src/items.sql"),
        "-- func: invalid\nselect * from missing",
    )
    .unwrap();
    assert!(!fixture.generate(&[]).status.success());
    assert_eq!(fs::read(&cache).unwrap(), before);
    fs::write(
        fixture.file("src/items.sql"),
        "-- func: list_items\nselect id, name from items",
    )
    .unwrap();
    fs::write(&cache, "broken").unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(fixture.run(&[]).contains("Skipped"));
    fs::remove_file(cache).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
}

#[cfg(target_os = "linux")]
#[test]
fn executable_content_change_invalidates_without_a_version_bump() {
    use std::io::Write;
    let fixture = Fixture::new();
    fixture.run(&[]);
    assert!(fixture.run(&[]).contains("Skipped"));
    let binary = fixture.file("marmot-copy");
    fs::copy(env!("CARGO_BIN_EXE_marmot"), &binary).unwrap();
    let run = || {
        let result = Command::new(&binary)
            .arg("generate")
            .current_dir(fixture.root.path())
            .env_remove("DATABASE_URL")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    assert!(
        run().contains("Skipped"),
        "identical executable bytes reuse the receipt"
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"cache identity change")
        .unwrap();
    assert!(!run().contains("Skipped"));
    assert!(run().contains("Skipped"));
}

#[test]
fn check_is_authoritative_even_when_normal_generation_would_skip() {
    let fixture = Fixture::new();
    fixture.run(&[]);
    // Module content is not an input to generation, but check must parse its
    // binding declaration. Both the inputs and published output still match.
    fs::write(fixture.file("src/items.rs"), "pub fn handler() {}\n").unwrap();
    assert!(fixture.run(&[]).contains("Skipped"));
    let result = fixture.generate(&["--check"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no owner declaration"));
}

#[test]
fn source_root_view_output_is_an_input_even_with_a_separate_analysis_root() {
    let fixture = Fixture::new();
    fixture.run(&[]);
    assert!(fixture.run(&[]).contains("Skipped"));
    fs::create_dir_all(fixture.file("src/db_views")).unwrap();
    let view = fixture.file("src/db_views/view_local.sql");
    fs::write(
        &view,
        "create view view_local (id) as select id from items;",
    )
    .unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(
        fs::read_to_string(fixture.file("src/generated/sql/views.sql"))
            .unwrap()
            .contains("view_local")
    );
    assert!(fixture.run(&[]).contains("Skipped"));
    fs::remove_file(view).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(!fixture.file("src/generated/sql/views.sql").exists());
}

#[test]
fn value_set_changes_invalidate_warm_generation() {
    let fixture = Fixture::new();
    fixture.run(&[]);
    assert!(fixture.run(&[]).contains("Skipped"));
    let declaration = fixture.file("src/value_sets.toml");
    fs::write(&declaration, "module = \"crate::value_sets\"\n[sets.ItemName]\nvalues = [\"first\", \"second\"]\ncolumns = [\"items.name\"]\n").unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    let output = fixture.file("src/generated/sql/value_sets.rs");
    assert!(fs::read_to_string(&output).unwrap().contains("Second"));
    assert!(fixture.run(&[]).contains("Skipped"));
    let source = fs::read_to_string(&declaration).unwrap();
    fs::write(&declaration, source.replace("second", "changed")).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(fs::read_to_string(&output).unwrap().contains("Changed"));
    fs::remove_file(declaration).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(!output.exists());
}

#[test]
fn scoped_registry_and_external_gate_sources_invalidate_generation() {
    let fixture = Fixture::new();
    fixture.run(&[]);
    fs::create_dir_all(fixture.file("outside")).unwrap();
    fs::write(fixture.file("outside/gate.rs"), "").unwrap();
    let gate = fixture.file("outside/gate.sql");
    fs::write(&gate,"-- func: gate\n-- produces: id = scope_fixture::scoped_ids::ScopedItemId\nselect id,name from items where id=@id").unwrap();
    let registry = fixture.file("src/scoped_ids.toml");
    fs::write(&registry,"module = \"scope_fixture::scoped_ids\"\n[ids.ScopedItemId]\ngates = [\"../outside/gate.sql#gate\"]\n").unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    let generated = fixture.file("src/generated/sql/scoped_ids.rs");
    assert!(generated.exists());
    assert!(fixture.run(&[]).contains("Skipped"));
    let sql = fs::read_to_string(&gate).unwrap();
    fs::write(&gate, format!("{sql} and name != 'excluded'")).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(fs::read_to_string(&generated).unwrap().contains("excluded"));
    fs::remove_file(registry).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
    assert!(!generated.exists());
}

#[test]
fn imported_scope_home_is_a_generation_input() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.file("contracts")).unwrap();
    fs::write(fixture.file("contracts/gate.rs"), "").unwrap();
    let home = fixture.file("contracts/scoped_ids.toml");
    fs::write(
        &home,
        "module = \"scope_fixture::scoped_ids\"\n[ids.ScopedItemId]\ngates = [\"gate.sql#gate\"]\n",
    )
    .unwrap();
    fs::write(fixture.file("contracts/gate.sql"),"-- func: gate\n-- produces: id = scope_fixture::scoped_ids::ScopedItemId\nselect id,name from items where id=@id").unwrap();
    fs::write(
        fixture.file("src/scoped_ids.toml"),
        "imports = [\"../contracts/scoped_ids.toml\"]\n",
    )
    .unwrap();
    fixture.run(&[]);
    assert!(fixture.run(&[]).contains("Skipped"));
    let source = fs::read_to_string(&home).unwrap();
    fs::write(home, format!("{source}\n")).unwrap();
    assert!(!fixture.run(&[]).contains("Skipped"));
}
