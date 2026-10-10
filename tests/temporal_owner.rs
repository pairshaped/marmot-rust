use std::fs;
use std::process::{Command, Output};

struct Owner {
    root: tempfile::TempDir,
}

impl Owner {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for path in ["crates/clock/src/generated/sql", "src/generated/sql", "db"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        let owner = Self { root };
        fs::write(
            owner.file("crates/clock/src/lib.rs"),
            "pub mod generated;\n",
        )
        .unwrap();
        fs::write(
            owner.file("crates/clock/src/generated/mod.rs"),
            "pub mod sql;\n",
        )
        .unwrap();
        fs::write(owner.file("src/generated/sql/other.rs"), "other owner\n").unwrap();
        rusqlite::Connection::open_in_memory()
            .unwrap()
            .execute(
                "vacuum into ?1",
                [owner.file("db/local.sqlite").to_str().unwrap()],
            )
            .unwrap();
        owner.config("emit_without_queries = true\n");
        owner
    }

    fn file(&self, path: &str) -> std::path::PathBuf {
        self.root.path().join(path)
    }

    fn config(&self, option: &str) {
        fs::write(self.file("marmot.toml"), format!(
            "[tools.marmot]\ndatabase = \"db/local.sqlite\"\nsource_root = \"crates/clock/src\"\noutput = \"crates/clock/src/generated/sql\"\n[tools.marmot.temporal]\nmodule = \"crate::generated::sql::temporal\"\noutput = \"crates/clock/src/generated/sql/temporal.rs\"\n{option}"
        )).unwrap();
    }

    fn generate(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_marmot"))
            .arg("generate")
            .args(args)
            .current_dir(self.root.path())
            .env_remove("DATABASE_URL")
            .output()
            .unwrap()
    }

    fn run(&self) -> String {
        let output = self.generate(&[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

#[test]
fn empty_owner_publishes_and_caches_only_its_temporal_output() {
    let owner = Owner::new();
    let temporal = owner.file("crates/clock/src/generated/sql/temporal.rs");
    owner.run();
    assert!(
        fs::read_to_string(&temporal)
            .unwrap()
            .contains("pub struct DbDateTime")
    );
    assert_eq!(
        fs::read_to_string(owner.file("crates/clock/src/generated/sql/mod.rs")).unwrap(),
        "pub mod temporal;\n"
    );
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::open(&temporal)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
    assert!(owner.file("crates/clock/.local/marmot").exists());
    assert!(owner.run().contains("Skipped"));
    assert_eq!(fs::metadata(&temporal).unwrap().modified().unwrap(), time);
    let obsolete = owner.file("crates/clock/src/generated/sql/obsolete.rs");
    fs::write(&obsolete, "obsolete").unwrap();
    assert!(!owner.run().contains("Skipped"));
    assert!(!obsolete.exists());
    assert_eq!(
        fs::read_to_string(owner.file("crates/clock/src/lib.rs")).unwrap(),
        "pub mod generated;\n"
    );
    assert_eq!(
        fs::read_to_string(owner.file("crates/clock/src/generated/mod.rs")).unwrap(),
        "pub mod sql;\n"
    );
    assert_eq!(
        fs::read_to_string(owner.file("src/generated/sql/other.rs")).unwrap(),
        "other owner\n"
    );
    owner.config("emit_without_queries = false\n");
    assert!(!owner.run().contains("Skipped"));
    assert!(!temporal.exists());
    owner.config("emit_without_queries = true\n");
    assert!(!owner.run().contains("Skipped"));
    assert!(temporal.exists());
}

#[test]
fn check_rejects_missing_or_stale_temporal_output_without_writing() {
    for missing in [true, false] {
        let owner = Owner::new();
        owner.run();
        assert!(owner.generate(&["--check"]).status.success());
        let temporal = owner.file("crates/clock/src/generated/sql/temporal.rs");
        if missing {
            fs::remove_file(&temporal).unwrap();
        } else {
            fs::write(&temporal, "stale").unwrap();
        }
        let output = owner.generate(&["--check"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("generated file"));
        if missing {
            assert!(!temporal.exists());
        } else {
            assert_eq!(fs::read_to_string(&temporal).unwrap(), "stale");
        }
        assert!(!owner.run().contains("Skipped"));
        assert!(owner.generate(&["--check"]).status.success());
    }
}

#[test]
fn invalid_temporal_owner_never_publishes_a_module_or_cache() {
    for change in ["type", "module", "output", "outside"] {
        let owner = Owner::new();
        let config = fs::read_to_string(owner.file("marmot.toml")).unwrap();
        let config = match change {
            "type" => config.replace(
                "emit_without_queries = true",
                "emit_without_queries = 'yes'",
            ),
            "module" => config.replace("module = \"crate::generated::sql::temporal\"\n", ""),
            "output" => config.replace(
                "output = \"crates/clock/src/generated/sql/temporal.rs\"\n",
                "",
            ),
            "outside" => config.replace(
                "crates/clock/src/generated/sql/temporal.rs",
                "src/generated/sql/temporal.rs",
            ),
            _ => unreachable!(),
        };
        fs::write(owner.file("marmot.toml"), config).unwrap();
        assert!(!owner.generate(&[]).status.success(), "{change}");
        assert!(
            !owner
                .file("crates/clock/src/generated/sql/temporal.rs")
                .exists()
        );
        assert!(!owner.file("crates/clock/.local/marmot").exists());
    }
}

#[test]
fn empty_owner_tracks_both_temporal_output_paths() {
    let owner = Owner::new();
    let output = owner.file("crates/clock/src/generated/sql");
    let config = marmot::config::Config {
        database: owner.file("db/local.sqlite"),
        source_root: owner.file("crates/clock/src"),
        output: output.clone(),
        target: marmot::config::Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            module: Some("crate::generated::sql::temporal".into()),
            output: Some(output.join("temporal.rs")),
            emit_without_queries: true,
            ..Default::default()
        },
    };
    assert_eq!(
        marmot::emit::output_paths(&config, &marmot::model::Project { queries: vec![] }),
        [output.join("mod.rs"), output.join("temporal.rs")]
            .into_iter()
            .collect()
    );
}

#[test]
fn library_emission_also_rejects_an_owner_without_output() {
    let owner = Owner::new();
    let config = marmot::config::Config {
        database: owner.file("db/local.sqlite"),
        source_root: owner.file("crates/clock/src"),
        output: owner.file("crates/clock/src/generated/sql"),
        target: marmot::config::Target::Rust,
        check: false,
        temporal: marmot::config::TemporalConfig {
            module: Some("crate::generated::sql::temporal".into()),
            emit_without_queries: true,
            ..Default::default()
        },
    };
    assert!(matches!(
        marmot::emit::emit_project(&config, &marmot::model::Project { queries: vec![] }),
        Err(marmot::error::Error::InvalidTemporalConfig { .. })
    ));
    assert!(!config.output.join("temporal.rs").exists());
}
