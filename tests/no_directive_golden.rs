use std::fs;

use marmot::{Config, Target, analyze_project, emit_project};
use rusqlite::Connection;

#[test]
fn no_directive_output_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.sqlite3");
    Connection::open(&database)
        .unwrap()
        .execute_batch(include_str!("fixtures/no_directive/schema.sql"))
        .unwrap();
    let source_root = dir.path().join("src");
    fs::create_dir_all(&source_root).unwrap();
    fs::write(source_root.join("items.rs"), "").unwrap();
    fs::write(
        source_root.join("items.sql"),
        include_str!("fixtures/no_directive/items.sql"),
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
    let project = analyze_project(&config).unwrap();
    emit_project(&config, &project).unwrap();
    let actual = fs::read_to_string(config.source_root.join("items.generated.rs")).unwrap();
    assert_eq!(
        actual,
        include_str!("fixtures/no_directive/items.expected.rs")
    );
}
