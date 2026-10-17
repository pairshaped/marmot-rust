use rusqlite::{Connection, Transaction};

mod sealed {
    use super::{Connection, Transaction};

    pub trait Sealed {}

    impl Sealed for &mut Connection {}
    impl Sealed for &Transaction<'_> {}
}

/// A connection authorized for generated mutation SQL.
///
/// Immutable standalone connections cannot mutate through this boundary.
///
/// ```compile_fail
/// use marmot::MutationConnection;
/// let connection = rusqlite::Connection::open_in_memory().unwrap();
/// fn mutation(_: impl MutationConnection) {}
/// mutation(&connection);
/// ```
///
/// Implementations outside Marmot cannot widen the accepted connection forms.
///
/// ```compile_fail
/// struct Other;
/// impl marmot::MutationConnection for Other {
///     fn as_connection(&self) -> &rusqlite::Connection { unimplemented!() }
/// }
/// ```
pub trait MutationConnection: sealed::Sealed {
    fn as_connection(&self) -> &Connection;
}

impl MutationConnection for &mut Connection {
    fn as_connection(&self) -> &Connection {
        self
    }
}

impl MutationConnection for &Transaction<'_> {
    fn as_connection(&self) -> &Connection {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::MutationConnection;

    #[test]
    fn mutable_connections_and_transactions_keep_the_same_database_and_rollback() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        (&mut connection)
            .as_connection()
            .execute("create table items (id integer)", [])
            .unwrap();
        let transaction = connection.transaction().unwrap();
        (&transaction)
            .as_connection()
            .execute("insert into items values (1)", [])
            .unwrap();
        transaction.rollback().unwrap();
        assert_eq!(
            connection
                .query_row("select count(*) from items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
