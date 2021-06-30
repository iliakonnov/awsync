use std::borrow::Borrow;
use std::path::{Path, PathBuf};

use crate::fileinfo::Info;
use crate::path::EncodedPath;
use crate::path::External;

use rusqlite::{named_params, params};
use snafu::{ensure, OptionExt, ResultExt, Snafu};

macro_rules! fmt_sql {
    ($($args:tt)*) => {{
        let sql = format!($($args)*);
        log!(fmt_sql: "fmt_sql: {}", sql);
        sql
    }}
}

#[derive(Debug, Snafu)]
pub enum Error {
    SqliteFailed {
        source: rusqlite::Error,
        backtrace: snafu::Backtrace,
    },
    JsonFailed {
        source: serde_json::Error,
        backtrace: snafu::Backtrace,
    },
    CantWalkdir {
        source: walkdir::Error,
    },
    CantBuildDiffName {
        source: NotAValidSqlName,
        before: SqlName,
        after: SqlName,
    },
    CantBuildPath {
        str: std::ffi::OsString,
        backtrace: snafu::Backtrace,
    },
    TooManySnapshots,
    TooManyRows,
    WrongDiffType {
        found: u8,
    },
    #[snafu(display("It looks like you have mixed different databases: this=0x{:x}, before=0x{:x}, after=0x{:x}", this, before, after))]
    DatabasesMixed {
        backtrace: snafu::Backtrace,
        this: usize,
        before: usize,
        after: usize,
    },
}

#[derive(Debug, Snafu)]
pub struct NotAValidSqlName {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlName(String);

impl SqlName {
    pub fn new(name: String) -> Result<SqlName, NotAValidSqlName> {
        let mut chars = name.chars();
        if matches!(chars.next(), Some('A'..='Z' | 'a'..='z'))
            && chars.all(|c| matches!(c, '0'..='9' | 'A'..='Z' | 'a'..='z' | '_'))
        {
            Ok(SqlName(name))
        } else {
            Err(NotAValidSqlName { name })
        }
    }

    pub fn now() -> SqlName {
        let name = time::OffsetDateTime::now_utc().format("at%Y_%m_%d_%H_%M_%S_%N");
        SqlName::new(name).unwrap()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SqlName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'a> From<&'a SqlName> for SqlName {
    fn from(x: &'a SqlName) -> SqlName {
        x.clone()
    }
}

fn generate_id(table_id: u64, row_id: u64) -> Result<u64, Error> {
    // Single database can have up to 2^23 snapshots.
    // That's enough for 100 years of making snapshots every 6 minutes.
    // (it should be easy to reset the counter too)
    // Each snapshot can have up to 2^(63-23) = 2^40 of files.
    // That is 256 times more than NTFS supports.
    // So it should be enough for any practical use.
    ensure!(table_id < (1 << 23), TooManySnapshots);
    ensure!(row_id < (1 << 40), TooManyRows);
    Ok((table_id << 23) | row_id)
}

pub struct Database {
    snapshot_count: usize,
    conn: rusqlite::Connection,
    root: PathBuf,
}

impl Database {
    fn attach(&self, name: &SqlName) -> Result<String, Error> {
        let mut root = self.root.clone();
        root.push(name.as_str());
        root.set_extension("db");
        let path = root
            .into_os_string()
            .into_string()
            .map_err(|str| CantBuildPath { str }.build())?;
        Ok(fmt_sql!("ATTACH DATABASE '{path}' AS {name}"))
    }

    pub fn open<P: AsRef<Path>>(root: P) -> Result<Self, Error> {
        let mut root = root.as_ref().to_owned();
        root.push("db.sqlite3");
        let db = rusqlite::Connection::open(&root).context(SqliteFailed)?;
        root.pop();

        db.execute(
            "CREATE TABLE IF NOT EXISTS snapshots (
                name TEXT NOT NULL,
                created_at DATETIME NOT NULL,
                filled_at DATETIME,
                uploaded BOOLEAN
            )",
            params![],
        )
        .context(SqliteFailed)?;
        let snapshot_count = db
            .query_row("SELECT COUNT(*) FROM snapshots", params![], |r| r.get(0))
            .context(SqliteFailed)?;
        Ok(Self {
            snapshot_count,
            conn: db,
            root,
        })
    }

    pub fn readonly_snapshot<'a>(&'a self, name: SqlName) -> Result<Snapshot<&'a Database>, Error> {
        self.conn
            .execute(&self.attach(&name)?, params![])
            .context(SqliteFailed)?;
        // FIXME: We should check is snapshot exists.
        Ok(Snapshot { db: self, name })
    }

    pub fn open_snapshot(&mut self, name: SqlName) -> Result<Snapshot<&mut Database>, Error> {
        // Attach database:
        self.conn
            .execute(&self.attach(&name)?, params![])
            .context(SqliteFailed)?;
        // Maybe we should create a table then.
        let is_exists = self
            .conn
            .execute(
                &fmt_sql!(
                    "SELECT name FROM {name}.sqlite_master
                    WHERE type='table' AND name='snap'",
                ),
                params![],
            )
            .context(SqliteFailed)?;
        if is_exists == 0 {
            // Ok, let's initialize it then
            let txn = self.conn.unchecked_transaction().context(SqliteFailed)?;
            let first_id = generate_id(self.snapshot_count as _, 0)?;
            txn.execute_batch(&fmt_sql!(
                "
                    CREATE TABLE {name}.snap (
                        id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
                        path STRING,
                        identifier BLOB,   /* binary data */
                        info TEXT          /* json */
                    );
                    INSERT INTO {name}.snap(id) VALUES ({first_id});
                    DELETE FROM {name}.snap WHERE id={first_id};
                "
            ))
            .context(SqliteFailed)?;
            txn.execute(
                "INSERT INTO snapshots(name, created_at, filled) VALUES (:name, :created_at, 0)",
                named_params![
                    ":name": name.0,
                    ":created_at": time::OffsetDateTime::now_utc().format(time::Format::Rfc3339),
                ],
            )
            .context(SqliteFailed)?;
            txn.commit().context(SqliteFailed)?;
            self.snapshot_count += 1;
        }
        Ok(Snapshot { db: self, name })
    }

    pub fn compare_snapshots<'a, D1: Borrow<Database>, D2: Borrow<Database>>(
        &'a self,
        before: &Snapshot<D1>,
        after: &Snapshot<D2>,
    ) -> Result<Diff<'a>, Error> {
        {
            let this = self as *const Self as usize;
            let before = std::borrow::Borrow::borrow(&before.db) as *const Self as usize;
            let after = std::borrow::Borrow::borrow(&after.db) as *const Self as usize;
            snafu::ensure!(
                this == before && before == after,
                DatabasesMixed {
                    this,
                    before,
                    after,
                }
            );
        }
        let name = SqlName::new(format!("diff_{}_vs_{}", &before.name, &after.name)).context(
            CantBuildDiffName {
                before: &before.name,
                after: &after.name,
            },
        )?;
        let diff = Diff::new(self, name)?;
        diff.fill(before, after)?;
        Ok(diff)
    }
}

pub struct Snapshot<D: Borrow<Database>> {
    db: D,
    name: SqlName,
}

pub struct SnapshotFiller<'a> {
    snap_name: &'a SqlName,
    transaction: rusqlite::Transaction<'a>,
    sql: String,
}

impl<'a> SnapshotFiller<'a> {
    pub fn new(snapshot: &'a mut Snapshot<&'a mut Database>) -> Result<Self, Error> {
        let txn = snapshot.db.conn.transaction().context(SqliteFailed)?;
        let sql = fmt_sql!(
            "INSERT INTO {0}.snap(path, identifier, info)
            VALUES(:path, :identifier, :info)",
            snapshot.name
        );
        txn.prepare_cached(&sql).context(SqliteFailed)?;
        Ok(SnapshotFiller {
            snap_name: &snapshot.name,
            transaction: txn,
            sql,
        })
    }

    fn get_statement(&self) -> Result<rusqlite::CachedStatement, Error> {
        self.transaction
            .prepare_cached(&self.sql)
            .context(SqliteFailed)
    }
}

impl Drop for SnapshotFiller<'_> {
    fn drop(&mut self) {
        // FIXME: Why do I need SnapshotFiller?
    }
}

impl<'a> Snapshot<&'a mut Database> {
    pub fn fill(&mut self, root: &Path) -> Result<(), Error> {
        let walk = walkdir::WalkDir::new(root).into_iter();
        log!(time: "Walking over {}", root = root.to_string_lossy());
        let txn = self.db.conn.unchecked_transaction().context(SqliteFailed)?;
        {
            let mut stmt = txn
                .prepare(&fmt_sql!(
                    "INSERT INTO {0}.snap(path, identifier, info)
                    VALUES(:path, :identifier, :info)",
                    self.name
                ))
                .context(SqliteFailed)?;
            for i in walk {
                let i = i.context(CantWalkdir)?;
                let metadata = i.metadata().context(CantWalkdir)?;
                let path = EncodedPath::from_path(i.into_path());
                let info = Info::with_metadata(path, metadata);
                stmt.execute(named_params![
                    ":path": info.path.as_bytes(),
                    ":identifier": info.identifier().as_ref().map(|i| i.as_bytes()).unwrap_or_default(),
                    ":info": serde_json::to_string(&info).context(JsonFailed)?,
                ])
                .context(SqliteFailed)?;
            }
        }
        txn.execute(
            "UPDATE snapshots SET filled_at=? WHERE name=?",
            params![
                time::OffsetDateTime::now_utc().format(time::Format::Rfc3339),
                self.name.as_str()
            ],
        )
        .context(SqliteFailed)?;
        txn.commit().context(SqliteFailed)?;
        log!(time: "Done walking ({})", root = root.to_string_lossy());
        Ok(())
    }
}

impl<'a, D: Borrow<Database>> Snapshot<D> {
    pub fn name(&self) -> &SqlName {
        &self.name
    }
}

impl<'a, D: Borrow<Database>> Drop for Snapshot<D> {
    fn drop(&mut self) {
        let db: &Database = self.db.borrow();
        let _ = db.conn.execute(&fmt_sql!("DETACH DATABASE {0}", self.name), params![]);
    }
}

pub struct Diff<'a> {
    db: &'a Database,
    name: SqlName,
}

#[derive(Debug, Eq, PartialEq, num_enum::TryFromPrimitive)]
#[repr(u8)]
pub enum DiffType {
    Deleted = 0,
    Created = 1,
    Changed = 2,
}

impl DiffType {
    fn parse(num: u8) -> Option<DiffType> {
        use std::convert::TryInto;
        num.try_into().ok()
    }
}

impl<'a> Diff<'a> {
    pub fn new(db: &'a Database, name: SqlName) -> Result<Self, Error> {
        {
            db.conn
                .execute(&db.attach(&name)?, params![])
                .context(SqliteFailed)?;
            db.conn
                .execute(
                    &fmt_sql!(
                        "
                        CREATE TABLE IF NOT EXISTS {name}.diff (
                            before INTEGER,  -- REFERENCES <before>.snap(id)
                            after  INTEGER,  -- REFERENCES <after>.snap(id),
                            type   INTEGER,  -- see `DiffType`
                            info   TEXT      -- same as snap.info 
                        )
                        "
                    ),
                    params![],
                )
                .context(SqliteFailed)?;
        }
        Ok(Diff { db, name })
    }

    pub fn fill<D1: Borrow<Database>, D2: Borrow<Database>>(
        &self,
        before: &Snapshot<D1>,
        after: &Snapshot<D2>,
    ) -> Result<(), Error> {
        let b = &before.name;
        let a = &after.name;

        {
            self.db
                .conn
                .execute_batch(&fmt_sql!(
                    "
                    CREATE INDEX IF NOT EXISTS {a}.idx_ident ON snap ( identifier );
                    CREATE INDEX IF NOT EXISTS {b}.idx_ident ON snap ( identifier );
                    CREATE INDEX IF NOT EXISTS {a}.idx_info ON snap ( info );
                    CREATE INDEX IF NOT EXISTS {b}.idx_info ON snap ( info );
                "
                ))
                .context(SqliteFailed)?;
        }

        let name = &self.name;
        let deleted = DiffType::Deleted as u8;
        let created = DiffType::Created as u8;
        let changed = DiffType::Changed as u8;
        self.db
            .conn
            .execute_batch(&fmt_sql!(
                r#"
                    DELETE FROM {name}.diff;

                    INSERT INTO {name}.diff (before, after, type, info)
                    SELECT
                        id,
                        NULL,
                        {deleted},
                        info
                    FROM {b}.snap
                    WHERE identifier NOT IN (SELECT identifier FROM {a}.snap);

                    INSERT INTO {name}.diff (before, after, type, info)
                    SELECT
                        NULL,
                        id,
                        {created},
                        info
                    FROM {a}.snap
                    WHERE identifier NOT IN (SELECT identifier FROM {b}.snap);

                    INSERT INTO {name}.diff (before, after, type, info)
                    SELECT
                        {b}.snap.id,
                        {a}.snap.id,
                        {changed},
                        {a}.snap.info
                    FROM {a}.snap
                        INNER JOIN {b}.snap
                        USING (identifier)
                    WHERE {a}.snap.info != {b}.snap.info;
                "#
            ))
            .context(SqliteFailed)?;

        Ok(())
    }

    pub fn for_each<F, E>(&'a self, mut func: F) -> Result<Result<(), E>, Error>
    where
        F: FnMut(DiffType, Info<External>) -> Result<(), E>,
    {
        let name = &self.name;
        let mut statement = self
            .db
            .conn
            .prepare(&fmt_sql!(
                "
            SELECT type, info
            FROM {name}.diff
            "
            ))
            .context(SqliteFailed)?;

        let mut rows = statement.query(params![]).context(SqliteFailed)?;
        loop {
            let row = rows.next().context(SqliteFailed)?;
            let row = match row {
                Some(x) => x,
                None => break,
            };
            let kind = row.get(0).context(SqliteFailed)?;
            let kind = DiffType::parse(kind).context(WrongDiffType { found: kind })?;

            let info: String = row.get(1).context(SqliteFailed)?;
            let info = serde_json::from_str(&info).context(JsonFailed)?;

            match func(kind, info) {
                Ok(_) => {}
                res @ Err(_) => return Ok(res),
            }
        }

        Ok(Ok(()))
    }

    pub fn of_kind<F, E>(&'a self, kind: DiffType, mut func: F) -> Result<Result<(), E>, Error>
    where
        F: FnMut(Info<External>) -> Result<(), E>,
    {
        // It's way easier to filter inside of Rust instead of passing `WHERE type = {kind}` to sqlite.
        self.for_each(|k, i| if k == kind { func(i) } else { Ok(()) })
    }
}

impl Drop for Diff<'_> {
    fn drop(&mut self) {
        let _ = self
            .db
            .conn
            .execute(&fmt_sql!("DETACH DATABASE {0}", self.name), params![]);
    }
}
