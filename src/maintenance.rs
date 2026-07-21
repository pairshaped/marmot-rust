use std::time::{Duration, Instant};

use rusqlite::Connection;

pub const SUPPORTED_SQLITE_VERSION: &str = "3.50.2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptimizeScope {
    SchemaChange,
    FirstOpen,
    Periodic,
}

impl OptimizeScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SchemaChange => "schema_change",
            Self::FirstOpen => "first_open",
            Self::Periodic => "periodic",
        }
    }

    const fn preview_sql(self) -> &'static str {
        match self {
            Self::FirstOpen => "PRAGMA optimize(0x10003)",
            Self::SchemaChange | Self::Periodic => "PRAGMA optimize(0xffff)",
        }
    }

    const fn optimize_sql(self) -> &'static str {
        match self {
            Self::FirstOpen => "PRAGMA optimize=0x10002",
            Self::SchemaChange | Self::Periodic => "PRAGMA optimize",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptimizeReport {
    pub scope: OptimizeScope,
    pub did_work: bool,
    pub action_count: usize,
    pub duration: Duration,
}

pub fn optimize(
    conn: &Connection,
    scope: OptimizeScope,
) -> Result<OptimizeReport, rusqlite::Error> {
    let started = Instant::now();
    let action_count = proposed_action_count(conn, scope.preview_sql())?;
    conn.execute_batch(scope.optimize_sql())?;

    Ok(OptimizeReport {
        scope,
        did_work: action_count > 0,
        action_count,
        duration: started.elapsed(),
    })
}

fn proposed_action_count(conn: &Connection, sql: &str) -> Result<usize, rusqlite::Error> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut count = 0;
    while rows.next()?.is_some() {
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_version_matches_bundled_sqlite() {
        assert_eq!(rusqlite::version(), SUPPORTED_SQLITE_VERSION);
    }

    #[test]
    fn optimize_reports_work_then_becomes_idempotent() {
        let conn = database_needing_statistics();

        let first = optimize(&conn, OptimizeScope::FirstOpen).expect("optimize database");
        let second = optimize(&conn, OptimizeScope::Periodic).expect("optimize database again");

        assert!(first.did_work);
        assert!(first.action_count > 0);
        assert!(!second.did_work);
        assert_eq!(second.action_count, 0);
        assert!(statistics_exist(&conn));
    }

    #[test]
    fn optimize_propagates_sqlite_errors() {
        let conn = database_needing_statistics();
        conn.pragma_update(None, "query_only", "ON")
            .expect("make connection read-only");

        let error = optimize(&conn, OptimizeScope::FirstOpen).expect_err("optimize should fail");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(error, _)
                if error.code == rusqlite::ErrorCode::ReadOnly
        ));
    }

    fn database_needing_statistics() -> Connection {
        let conn = Connection::open_in_memory().expect("open database");
        conn.execute_batch(
            "CREATE TABLE teams (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\
             CREATE INDEX teams_name ON teams(name);\
             WITH RECURSIVE numbers(value) AS (\
               SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
             )\
             INSERT INTO teams(name) SELECT printf('team-%04d', value) FROM numbers;",
        )
        .expect("create database needing statistics");
        conn
    }

    fn statistics_exist(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_stat1 WHERE tbl = 'teams')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false)
    }
}
