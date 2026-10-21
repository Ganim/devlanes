//! The migrated database against `src/schema.rs`, and a scan of every table for
//! columns that could hold client environments or secrets.

use devlanes_state::Store;
use diesel::prelude::*;

#[derive(diesel::QueryableByName)]
struct Table {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

#[derive(Debug, diesel::QueryableByName)]
struct Column {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    kind: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    notnull: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pk: i32,
}

/// Opens a fully migrated file database and a second raw connection to it.
async fn migrated() -> (tempfile::TempDir, SqliteConnection) {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.db");
    Store::open(&path)
        .await
        .expect("store")
        .close()
        .await
        .expect("close");
    let connection = SqliteConnection::establish(path.to_str().expect("path")).expect("db");
    (dir, connection)
}

/// Application tables, excluding SQLite internals and Diesel's migration ledger.
fn tables(c: &mut SqliteConnection) -> Vec<String> {
    diesel::sql_query(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' AND name != '__diesel_schema_migrations' ORDER BY name",
    )
    .load::<Table>(c)
    .expect("tables")
    .into_iter()
    .map(|table| table.name)
    .collect()
}

/// Columns of `table` in declaration order.
fn columns(c: &mut SqliteConnection, table: &str) -> Vec<Column> {
    diesel::sql_query(
        "SELECT name, type AS kind, \"notnull\", pk FROM pragma_table_info(?) ORDER BY cid",
    )
    .bind::<diesel::sql_types::Text, _>(table)
    .load::<Column>(c)
    .expect("columns")
}

/// `(table, [(column, diesel type)])` from the `diesel::table!` blocks of schema.rs.
fn declared() -> Vec<(String, Vec<(String, String)>)> {
    let mut tables = Vec::new();
    for line in include_str!("../../src/schema.rs").lines() {
        let line = line.trim();
        if let Some((name, _)) = line.split_once(" (")
            && line.ends_with('{')
            && !name.contains(' ')
        {
            tables.push((name.to_owned(), Vec::new()));
        } else if let Some((column, kind)) = line.split_once(" -> ")
            && let Some((_, columns)) = tables.last_mut()
        {
            columns.push((column.to_owned(), kind.trim_end_matches(',').to_owned()));
        }
    }
    tables
}

#[tokio::test]
/// Every migrated table and column matches schema.rs in name, order, type and
/// nullability, so a migration cannot drift from the checked-in schema.
async fn schema_rs_matches_the_migrations() {
    let (_dir, mut c) = migrated().await;
    let declared = declared();
    let mut names: Vec<_> = declared.iter().map(|(name, _)| name.clone()).collect();
    names.sort();
    assert_eq!(tables(&mut c), names, "tables");
    for (table, expected) in declared {
        let actual = columns(&mut c, &table);
        assert_eq!(
            actual.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            expected.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            "columns of {table}"
        );
        for (column, (_, kind)) in actual.iter().zip(&expected) {
            let (nullable, inner) = match kind
                .strip_prefix("Nullable<")
                .and_then(|k| k.strip_suffix('>'))
            {
                Some(inner) => (true, inner),
                None => (false, kind.as_str()),
            };
            let sql = column.kind.to_ascii_uppercase();
            let compatible = match inner {
                "BigInt" => sql == "BIGINT" || sql == "INTEGER",
                "Integer" => sql == "INTEGER",
                "Text" => sql == "TEXT",
                "Binary" => sql == "BLOB",
                "Double" => sql == "DOUBLE",
                other => panic!("{table}.{}: unmapped Diesel type {other}", column.name),
            };
            assert!(compatible, "{table}.{}: {sql} vs {kind}", column.name);
            // SQLite reports INTEGER PRIMARY KEY columns as nullable; they never are.
            let sql_nullable = column.notnull == 0 && column.pk == 0;
            assert_eq!(
                sql_nullable, nullable,
                "{table}.{}: nullability vs {kind}",
                column.name
            );
        }
    }
}

#[tokio::test]
/// No table has a column meant for client environments, secrets or launch commands.
async fn no_table_stores_environments_or_secrets() {
    let (_dir, mut c) = migrated().await;
    let tables = tables(&mut c);
    assert!(tables.len() > 10, "{tables:?}");
    for table in tables {
        for column in columns(&mut c, &table) {
            let name = column.name.to_ascii_lowercase();
            for word in ["env", "secret", "password", "token", "command", "argv"] {
                assert!(
                    !name.contains(word),
                    "{table}.{} may hold client data",
                    column.name
                );
            }
        }
    }
}
