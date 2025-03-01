#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use std::collections::HashSet;

use anyhow::{anyhow, Context, Error as ErrReport};
use clap::Parser;
use sqlx::migrate::{Migrate, MigrateDatabase, Migrator};
use sqlx::pool::PoolOptions;
use sqlx::{Executor, Pool, Postgres, Row};
use thiserror::Error;
use tracing::{error, info, instrument, warn};

use self::prover::ProverConfiguration;
use crate::identity_tree::{Hash, RootItem, Status, TreeItem, TreeUpdate};

pub mod prover;
pub mod types;
use crate::secret::SecretUrl;

// Statically link in migration files
static MIGRATOR: Migrator = sqlx::migrate!("schemas/database");

const MAX_UNPROCESSED_FETCH_COUNT: i64 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Parser)]
pub struct Options {
    /// Database server connection string.
    /// Example: `postgres://user:password@localhost:5432/database`
    #[clap(long, env)]
    pub database: SecretUrl,

    /// Allow creation or migration of the database schema.
    #[clap(long, default_value = "true")]
    pub database_migrate: bool,

    /// Maximum number of connections in the database connection pool
    #[clap(long, env, default_value = "10")]
    pub database_max_connections: u32,
}

pub struct Database {
    pool: Pool<Postgres>,
}

impl Database {
    #[instrument(skip_all)]
    pub async fn new(options: Options) -> Result<Self, ErrReport> {
        info!(url = %&options.database, "Connecting to database");

        // Create database if requested and does not exist
        if options.database_migrate && !Postgres::database_exists(options.database.expose()).await?
        {
            warn!(url = %&options.database, "Database does not exist, creating database");
            Postgres::create_database(options.database.expose()).await?;
        }

        // Create a connection pool
        let pool = PoolOptions::<Postgres>::new()
            .max_connections(options.database_max_connections)
            .connect(options.database.expose())
            .await
            .context("error connecting to database")?;

        let version = pool
            .fetch_one("SELECT version()")
            .await
            .context("error getting database version")?
            .get::<String, _>(0);
        info!(url = %&options.database, ?version, "Connected to database");

        // Run migrations if requested.
        let latest = MIGRATOR
            .migrations
            .last()
            .expect("Missing migrations")
            .version;

        if options.database_migrate {
            info!(url = %&options.database, "Running migrations");
            MIGRATOR.run(&pool).await?;
        }

        // Validate database schema version
        #[allow(deprecated)] // HACK: No good alternative to `version()`?
        if let Some((version, dirty)) = pool.acquire().await?.version().await? {
            if dirty {
                error!(
                    url = %&options.database,
                    version,
                    expected = latest,
                    "Database is in incomplete migration state.",
                );
                return Err(anyhow!("Database is in incomplete migration state."));
            } else if version < latest {
                error!(
                    url = %&options.database,
                    version,
                    expected = latest,
                    "Database is not up to date, try rerunning with --database-migrate",
                );
                return Err(anyhow!(
                    "Database is not up to date, try rerunning with --database-migrate"
                ));
            } else if version > latest {
                error!(
                    url = %&options.database,
                    version,
                    latest,
                    "Database version is newer than this version of the software, please update.",
                );
                return Err(anyhow!(
                    "Database version is newer than this version of the software, please update."
                ));
            }
            info!(
                url = %&options.database,
                version,
                latest,
                "Database version is up to date.",
            );
        } else {
            error!(url = %&options.database, "Could not get database version");
            return Err(anyhow!("Could not get database version."));
        }

        Ok(Self { pool })
    }

    pub async fn insert_pending_identity(
        &self,
        leaf_index: usize,
        identity: &Hash,
        root: &Hash,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;

        let insert_pending_identity_query = sqlx::query(
            r#"
            INSERT INTO identities (leaf_index, commitment, root, status, pending_as_of)
            VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP)
            ON CONFLICT (root) DO NOTHING;
            "#,
        )
        .bind(leaf_index as i64)
        .bind(identity)
        .bind(root)
        .bind(<&str>::from(Status::Pending));

        tx.execute(insert_pending_identity_query).await?;

        tx.commit().await?;

        Ok(())
    }

    pub async fn get_leaf_index_by_root(
        tx: impl Executor<'_, Database = Postgres>,
        root: &Hash,
    ) -> Result<Option<usize>, Error> {
        let root_leaf_index_query = sqlx::query(
            r#"
            SELECT leaf_index FROM identities WHERE root = $1
            "#,
        )
        .bind(root);

        let row = tx.fetch_optional(root_leaf_index_query).await?;

        let Some(row) = row else { return Ok(None) };
        let root_leaf_index = row.get::<i64, _>(0);

        Ok(Some(root_leaf_index as usize))
    }

    /// Marks the identities and roots from before a given root hash as mined
    /// Also marks following roots as pending
    #[instrument(skip(self), level = "debug")]
    pub async fn mark_root_as_processed(&self, root: &Hash) -> Result<(), Error> {
        let mined_status = Status::Mined;
        let processed_status = Status::Processed;
        let pending_status = Status::Pending;

        let mut tx = self.pool.begin().await?;

        let root_leaf_index = Self::get_leaf_index_by_root(&mut tx, root).await?;

        let Some(root_leaf_index) = root_leaf_index else {
            return Err(Error::MissingRoot { root: *root });
        };

        let root_leaf_index = root_leaf_index as i64;

        // TODO: Can I get rid of line `AND    status <> $2
        let update_previous_roots = sqlx::query(
            r#"
            UPDATE identities
            SET    status = $2, mined_at = CURRENT_TIMESTAMP
            WHERE  leaf_index <= $1
            AND    status <> $2
            AND    status <> $3
            "#,
        )
        .bind(root_leaf_index)
        .bind(<&str>::from(processed_status))
        .bind(<&str>::from(mined_status));

        let update_next_roots = sqlx::query(
            r#"
            UPDATE identities
            SET    status = $2, mined_at = NULL
            WHERE  leaf_index > $1
            "#,
        )
        .bind(root_leaf_index)
        .bind(<&str>::from(pending_status));

        tx.execute(update_previous_roots).await?;
        tx.execute(update_next_roots).await?;

        tx.commit().await?;

        Ok(())
    }

    /// Marks the identities and roots from before a given root hash as
    /// finalized
    #[instrument(skip(self), level = "debug")]
    pub async fn mark_root_as_mined(&self, root: &Hash) -> Result<(), Error> {
        let mined_status = Status::Mined;

        let mut tx = self.pool.begin().await?;

        let root_leaf_index = Self::get_leaf_index_by_root(&mut tx, root).await?;

        let Some(root_leaf_index) = root_leaf_index else {
            return Err(Error::MissingRoot { root: *root });
        };

        let root_leaf_index = root_leaf_index as i64;

        let update_previous_roots = sqlx::query(
            r#"
            UPDATE identities
            SET    status = $2
            WHERE  leaf_index <= $1
            AND    status <> $2
            "#,
        )
        .bind(root_leaf_index)
        .bind(<&str>::from(mined_status));

        tx.execute(update_previous_roots).await?;

        tx.commit().await?;

        Ok(())
    }

    pub async fn get_next_leaf_index(&self) -> Result<usize, Error> {
        let query = sqlx::query(
            r#"
            SELECT leaf_index FROM identities
            ORDER BY leaf_index DESC
            LIMIT 1
            "#,
        );

        let row = self.pool.fetch_optional(query).await?;

        let Some(row) = row else { return Ok(0) };
        let leaf_index = row.get::<i64, _>(0);

        Ok((leaf_index + 1) as usize)
    }

    pub async fn get_identity_leaf_index(
        &self,
        identity: &Hash,
    ) -> Result<Option<TreeItem>, Error> {
        let query = sqlx::query(
            r#"
            SELECT leaf_index, status
            FROM identities
            WHERE commitment = $1
            LIMIT 1;
            "#,
        )
        .bind(identity);

        let Some(row) = self.pool.fetch_optional(query).await? else {
            return Ok(None);
        };

        let leaf_index = row.get::<i64, _>(0) as usize;

        let status = row
            .get::<&str, _>(1)
            .parse()
            .expect("Status is unreadable, database is corrupt");

        Ok(Some(TreeItem { status, leaf_index }))
    }

    pub async fn get_commitments_by_status(
        &self,
        status: Status,
    ) -> Result<Vec<TreeUpdate>, Error> {
        let query = sqlx::query(
            r#"
            SELECT leaf_index, commitment
            FROM identities
            WHERE status = $1
            ORDER BY leaf_index ASC;
            "#,
        )
        .bind(<&str>::from(status));

        let rows = self.pool.fetch_all(query).await?;

        Ok(rows
            .into_iter()
            .map(|row| TreeUpdate {
                leaf_index: row.get::<i64, _>(0) as usize,
                element:    row.get::<Hash, _>(1),
            })
            .collect::<Vec<_>>())
    }

    pub async fn get_root_state(&self, root: &Hash) -> Result<Option<RootItem>, Error> {
        // This tries really hard to do everything in one query to prevent race
        // conditions.
        let query = sqlx::query(
            r#"
            SELECT
                status,
                pending_as_of as pending_valid_as_of,
                mined_at as mined_valid_as_of
            FROM identities
            WHERE root = $1;
            "#,
        )
        .bind(root);

        let row = self.pool.fetch_optional(query).await?;

        Ok(row.map(|r| {
            let status = r
                .get::<&str, _>(0)
                .parse()
                .expect("Status is unreadable, database is corrupt");

            let pending_valid_as_of = r.get::<_, _>(1);
            let mined_valid_as_of = r.get::<_, _>(2);

            RootItem {
                root: *root,
                status,
                pending_valid_as_of,
                mined_valid_as_of,
            }
        }))
    }

    pub async fn count_unprocessed_identities(&self) -> Result<i32, Error> {
        let query = sqlx::query(
            r#"
            SELECT COUNT(*) as unprocessed
            FROM unprocessed_identities
            "#,
        );
        let result = self.pool.fetch_one(query).await?;
        Ok(result.get::<i64, _>(0) as i32)
    }

    pub async fn count_pending_identities(&self) -> Result<i32, Error> {
        let query = sqlx::query(
            r#"
            SELECT COUNT(*) as pending
            FROM identities
            WHERE status = $1
            "#,
        )
        .bind(<&str>::from(Status::Pending));
        let result = self.pool.fetch_one(query).await?;
        Ok(result.get::<i64, _>(0) as i32)
    }

    pub async fn get_provers(&self) -> Result<prover::Provers, Error> {
        let query = sqlx::query(
            r#"
                SELECT batch_size, url, timeout_s
                FROM provers
            "#,
        );

        let result = self.pool.fetch_all(query).await?;

        Ok(result
            .iter()
            .map(|row| {
                let batch_size = row.get::<i64, _>(0) as usize;
                let url = row.get::<String, _>(1);
                let timeout_s = row.get::<i64, _>(2) as u64;
                prover::ProverConfiguration {
                    url,
                    batch_size,
                    timeout_s,
                }
            })
            .collect::<prover::Provers>())
    }

    pub async fn insert_prover_configuration(
        &self,
        batch_size: usize,
        url: impl ToString,
        timeout_seconds: u64,
    ) -> Result<(), Error> {
        let url = url.to_string();

        let query = sqlx::query(
            r#"
                INSERT INTO provers (batch_size, url, timeout_s)
                VALUES ($1, $2, $3)
                ON CONFLICT (batch_size)
                DO UPDATE SET (url, timeout_s) = ($2, $3)
            "#,
        )
        .bind(batch_size as i64)
        .bind(url)
        .bind(timeout_seconds as i64);

        self.pool.execute(query).await?;

        Ok(())
    }

    pub async fn insert_provers(&self, provers: HashSet<ProverConfiguration>) -> Result<(), Error> {
        if provers.is_empty() {
            return Ok(());
        }

        let mut query_builder = sqlx::QueryBuilder::new(
            r#"
                  INSERT INTO provers (batch_size, url, timeout_s)
            "#,
        );

        query_builder.push_values(provers, |mut b, prover| {
            b.push_bind(prover.batch_size as i64)
                .push_bind(prover.url)
                .push_bind(prover.timeout_s as i64);
        });

        let query = query_builder.build();

        self.pool.execute(query).await?;
        Ok(())
    }

    pub async fn remove_prover(&self, batch_size: usize) -> Result<(), Error> {
        let query = sqlx::query(
            r#"
              DELETE FROM provers WHERE batch_size = $1
            "#,
        )
        .bind(batch_size as i64);

        self.pool.execute(query).await?;

        Ok(())
    }

    pub async fn insert_new_identity(&self, identity: Hash) -> Result<Hash, Error> {
        let query = sqlx::query(
            r#"
            INSERT INTO unprocessed_identities (commitment, status, created_at)
            VALUES ($1, $2, CURRENT_TIMESTAMP)
            "#,
        )
        .bind(identity)
        .bind(<&str>::from(Status::New));
        self.pool.execute(query).await?;
        Ok(identity)
    }

    pub async fn get_unprocessed_commitments(
        &self,
        status: Status,
    ) -> Result<Vec<types::UnprocessedCommitment>, Error> {
        let query = sqlx::query(
            r#"
                SELECT * FROM unprocessed_identities
                WHERE status = $1
                LIMIT $2
            "#,
        )
        .bind(<&str>::from(status))
        .bind(MAX_UNPROCESSED_FETCH_COUNT);

        let result = self.pool.fetch_all(query).await?;

        Ok(result
            .into_iter()
            .map(|row| types::UnprocessedCommitment {
                commitment: row.get::<Hash, _>(0),
                status,
                created_at: row.get::<_, _>(2),
                processed_at: row.get::<_, _>(3),
                error_message: row.get::<_, _>(4),
            })
            .collect::<Vec<_>>())
    }

    pub async fn get_unprocessed_commit_status(
        &self,
        commitment: &Hash,
    ) -> Result<Option<(Status, String)>, Error> {
        let query = sqlx::query(
            r#"
                SELECT status, error_message FROM unprocessed_identities WHERE commitment = $1
            "#,
        )
        .bind(commitment);

        let result = self.pool.fetch_optional(query).await?;

        if let Some(row) = result {
            return Ok(Some((
                row.get::<&str, _>(0).parse().expect("couldn't read status"),
                row.get::<Option<String>, _>(1).unwrap_or_default(),
            )));
        };
        Ok(None)
    }

    pub async fn remove_unprocessed_identity(&self, commitment: &Hash) -> Result<(), Error> {
        let query = sqlx::query(
            r#"
                DELETE FROM unprocessed_identities WHERE commitment = $1
            "#,
        )
        .bind(commitment);

        self.pool.execute(query).await?;

        Ok(())
    }

    pub async fn update_err_unprocessed_commitment(
        &self,
        commitment: Hash,
        message: String,
    ) -> Result<(), Error> {
        let query = sqlx::query(
            r#"
                UPDATE unprocessed_identities SET error_message = $1, status = $2
                WHERE commitment = $3
            "#,
        )
        .bind(message)
        .bind(<&str>::from(Status::Failed))
        .bind(commitment);

        self.pool.execute(query).await?;

        Ok(())
    }

    pub async fn identity_exists(&self, commitment: Hash) -> Result<bool, Error> {
        let query_unprocessed_identity = sqlx::query(
            r#"SELECT exists(SELECT 1 from unprocessed_identities where commitment = $1)"#,
        )
        .bind(commitment);

        let row_unprocessed = self.pool.fetch_one(query_unprocessed_identity).await?;

        let query_processed_identity =
            sqlx::query(r#"SELECT exists(SELECT 1 from identities where commitment = $1)"#)
                .bind(commitment);

        let row_processed = self.pool.fetch_one(query_processed_identity).await?;

        let exists = row_unprocessed.get::<bool, _>(0) || row_processed.get::<bool, _>(0);

        Ok(exists)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    InternalError(#[from] sqlx::Error),

    #[error("Tried to mine missing root {root:?}")]
    MissingRoot { root: Hash },
}

#[cfg(test)]
mod test {
    use std::str::FromStr;
    use std::time::Duration;

    use anyhow::Context;
    use chrono::Utc;
    use ethers::types::U256;
    use postgres_docker_utils::DockerContainerGuard;
    use semaphore::Field;

    use super::{Database, Options};
    use crate::identity_tree::{Hash, Status};
    use crate::secret::SecretUrl;

    macro_rules! assert_same_time {
        ($a:expr, $b:expr, $diff:expr) => {
            assert!(
                abs_duration($a - $b) < $diff,
                "Difference between {} and {} is larger than {:?}",
                $a,
                $b,
                $diff
            );
        };

        ($a:expr, $b:expr) => {
            assert_same_time!($a, $b, chrono::Duration::milliseconds(500));
        };
    }

    fn abs_duration(x: chrono::Duration) -> chrono::Duration {
        chrono::Duration::milliseconds(x.num_milliseconds().abs())
    }

    async fn setup_db() -> anyhow::Result<(Database, DockerContainerGuard)> {
        let db_container = postgres_docker_utils::setup().await?;
        let port = db_container.port();

        let url = format!("postgres://postgres:postgres@localhost:{port}/database");

        let db = Database::new(Options {
            database:                 SecretUrl::from_str(&url)?,
            database_migrate:         true,
            database_max_connections: 1,
        })
        .await?;

        Ok((db, db_container))
    }

    fn mock_roots(n: usize) -> Vec<Field> {
        (1..=n).map(Field::from).collect()
    }

    fn mock_identities(n: usize) -> Vec<Field> {
        (1..=n).map(Field::from).collect()
    }

    async fn assert_roots_are(
        db: &Database,
        roots: impl IntoIterator<Item = &Field>,
        expected_state: Status,
    ) -> anyhow::Result<()> {
        for root in roots {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, expected_state,);
        }

        Ok(())
    }

    #[tokio::test]
    async fn insert_identity() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;
        let dec = "1234500000000000000";
        let commit_hash: Hash = U256::from_dec_str(dec)
            .expect("cant convert to u256")
            .into();
        let hash = db.insert_new_identity(commit_hash).await?;

        assert_eq!(commit_hash, hash);

        let commit = db
            .get_unprocessed_commit_status(&commit_hash)
            .await?
            .expect("expected commitment status");
        assert_eq!(commit.0, Status::New);

        let identity_count = db.get_unprocessed_commitments(Status::New).await?.len();
        assert_eq!(identity_count, 1);

        assert!(db.remove_unprocessed_identity(&commit_hash).await.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn get_last_leaf_index() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(1);
        let roots = mock_roots(1);

        let next_leaf_index = db.get_next_leaf_index().await?;

        assert_eq!(next_leaf_index, 0, "Db should contain not leaf indexes");

        db.insert_pending_identity(0, &identities[0], &roots[0])
            .await?;

        let next_leaf_index = db.get_next_leaf_index().await?;
        assert_eq!(next_leaf_index, 1);

        Ok(())
    }

    #[tokio::test]
    async fn mark_root_as_processed_marks_previous_roots() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(5);
        let roots = mock_roots(5);

        for i in 0..5 {
            db.insert_pending_identity(i, &identities[i], &roots[i])
                .await
                .context("Inserting identity")?;
        }

        db.mark_root_as_processed(&roots[2]).await?;

        for root in roots.iter().take(3) {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, Status::Processed);
        }

        for root in roots.iter().skip(3).take(2) {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, Status::Pending);
        }

        let pending_identities = db.count_pending_identities().await?;
        assert_eq!(
            pending_identities, 2,
            "There should be 2 pending identities"
        );

        Ok(())
    }

    #[tokio::test]
    async fn mark_root_as_mined_marks_previous_roots() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(5);
        let roots = mock_roots(5);

        for i in 0..5 {
            db.insert_pending_identity(i, &identities[i], &roots[i])
                .await
                .context("Inserting identity")?;
        }

        db.mark_root_as_mined(&roots[2]).await?;

        for root in roots.iter().take(3) {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, Status::Mined);
        }

        for root in roots.iter().skip(3).take(2) {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, Status::Pending);
        }

        let pending_identities = db.count_pending_identities().await?;
        assert_eq!(
            pending_identities, 2,
            "There should be 2 pending identities"
        );

        Ok(())
    }

    #[tokio::test]
    async fn mark_root_as_mined_interaction_with_mark_root_as_processed() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let num_identities = 6;

        let identities = mock_identities(num_identities);
        let roots = mock_roots(num_identities);

        for i in 0..num_identities {
            db.insert_pending_identity(i, &identities[i], &roots[i])
                .await
                .context("Inserting identity")?;
        }

        println!("Marking roots up to 2nd as processed");
        db.mark_root_as_processed(&roots[2]).await?;

        assert_roots_are(&db, &roots[..3], Status::Processed).await?;
        assert_roots_are(&db, &roots[3..], Status::Pending).await?;

        println!("Marking roots up to 1st as mined");
        db.mark_root_as_mined(&roots[1]).await?;

        assert_roots_are(&db, &roots[..2], Status::Mined).await?;
        assert_roots_are(&db, &[roots[2]], Status::Processed).await?;
        assert_roots_are(&db, &roots[3..], Status::Pending).await?;

        println!("Marking roots up to 4th as processed");
        db.mark_root_as_processed(&roots[4]).await?;

        assert_roots_are(&db, &roots[..2], Status::Mined).await?;
        assert_roots_are(&db, &roots[2..5], Status::Processed).await?;
        assert_roots_are(&db, &roots[5..], Status::Pending).await?;

        println!("Marking all roots as mined");
        db.mark_root_as_mined(&roots[num_identities - 1]).await?;

        assert_roots_are(&db, &roots, Status::Mined).await?;

        Ok(())
    }

    #[tokio::test]
    async fn mark_root_as_processed_marks_next_roots() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(5);
        let roots = mock_roots(5);

        for i in 0..5 {
            db.insert_pending_identity(i, &identities[i], &roots[i])
                .await
                .context("Inserting identity")?;
        }

        // root[2] is somehow erroneously marked as mined
        db.mark_root_as_processed(&roots[2]).await?;

        // Later we correctly mark the previous root as mined
        db.mark_root_as_processed(&roots[1]).await?;

        for root in roots.iter().take(2) {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, Status::Processed);
        }

        for root in roots.iter().skip(2).take(3) {
            let root = db
                .get_root_state(root)
                .await?
                .context("Fetching root state")?;

            assert_eq!(root.status, Status::Pending);
        }

        let pending_identities = db.count_pending_identities().await?;
        assert_eq!(pending_identities, 3, "There should be 3 pending roots");

        Ok(())
    }

    #[tokio::test]
    async fn root_history_timing() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(5);
        let roots = mock_roots(5);

        let root = db.get_root_state(&roots[0]).await?;

        assert!(root.is_none(), "Root should not exist");

        db.insert_pending_identity(0, &identities[0], &roots[0])
            .await?;

        let root = db
            .get_root_state(&roots[0])
            .await?
            .context("Fetching root state")?;
        let time_of_insertion = Utc::now();

        assert_same_time!(
            root.pending_valid_as_of,
            time_of_insertion,
            chrono::Duration::milliseconds(100)
        );

        assert!(
            root.mined_valid_as_of.is_none(),
            "Root has not yet been mined"
        );

        db.mark_root_as_processed(&roots[0]).await?;

        let root = db
            .get_root_state(&roots[0])
            .await?
            .context("Fetching root state")?;
        let time_of_mining = Utc::now();

        let mined_valid_as_of = root
            .mined_valid_as_of
            .context("Root should have been mined")?;

        assert_same_time!(
            root.pending_valid_as_of,
            time_of_insertion,
            chrono::Duration::milliseconds(100)
        );
        assert_same_time!(
            mined_valid_as_of,
            time_of_mining,
            chrono::Duration::milliseconds(100)
        );

        Ok(())
    }

    #[tokio::test]
    async fn get_commitments_by_status() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(5);
        let roots = mock_roots(5);

        for i in 0..5 {
            db.insert_pending_identity(i, &identities[i], &roots[i])
                .await
                .context("Inserting identity")?;
        }

        db.mark_root_as_processed(&roots[2]).await?;

        let mined_tree_updates = db.get_commitments_by_status(Status::Processed).await?;
        let pending_tree_updates = db.get_commitments_by_status(Status::Pending).await?;

        assert_eq!(mined_tree_updates.len(), 3);
        for i in 0..3 {
            assert_eq!(mined_tree_updates[i].element, identities[i]);
            assert_eq!(mined_tree_updates[i].leaf_index, i);
        }

        assert_eq!(pending_tree_updates.len(), 2);
        for i in 0..2 {
            assert_eq!(pending_tree_updates[i].element, identities[i + 3]);
            assert_eq!(pending_tree_updates[i].leaf_index, i + 3);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_root_invalidation() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(5);
        let roots = mock_roots(5);

        db.insert_pending_identity(0, &identities[0], &roots[0])
            .await
            .context("Inserting identity 1")?;

        tokio::time::sleep(Duration::from_secs(2)).await; // sleep enough for the database time resolution

        // Invalid root returns None
        assert!(db.get_root_state(&roots[1]).await?.is_none());

        // Basic scenario, latest pending root
        let root_item = db.get_root_state(&roots[0]).await?.unwrap();
        assert_eq!(roots[0], root_item.root);
        assert!(matches!(root_item.status, Status::Pending));
        assert!(root_item.mined_valid_as_of.is_none());

        // Inserting a new pending root sets invalidation time for the
        // previous root
        db.insert_pending_identity(1, &identities[1], &roots[1])
            .await?;
        db.insert_pending_identity(2, &identities[2], &roots[2])
            .await?;

        let root_1_inserted_at = Utc::now();

        tokio::time::sleep(Duration::from_secs(2)).await; // sleep enough for the database time resolution

        let root_item_0 = db.get_root_state(&roots[0]).await?.unwrap();
        let root_item_1 = db.get_root_state(&roots[1]).await?.unwrap();

        assert!(root_item_0.pending_valid_as_of < root_1_inserted_at);
        println!("root_1_inserted_at = {root_1_inserted_at:?}");
        println!(
            "root_item_1.pending_valid_as_of = {:?}",
            root_item_1.pending_valid_as_of
        );

        assert_same_time!(root_item_1.pending_valid_as_of, root_1_inserted_at);

        // Test mined roots
        db.insert_pending_identity(3, &identities[3], &roots[3])
            .await?;

        db.mark_root_as_processed(&roots[0])
            .await
            .context("Marking root as mined")?;

        let root_2_mined_at = Utc::now();

        tokio::time::sleep(Duration::from_secs(2)).await; // sleep enough for the database time resolution

        let root_item_2 = db.get_root_state(&roots[2]).await?.unwrap();
        assert!(matches!(root_item_2.status, Status::Pending));
        assert!(root_item_2.mined_valid_as_of.is_none());

        let root_item_1 = db.get_root_state(&roots[1]).await?.unwrap();
        assert_eq!(root_item_1.status, Status::Pending);
        assert!(root_item_1.mined_valid_as_of.is_none());
        assert!(root_item_1.pending_valid_as_of < root_2_mined_at);

        let root_item_0 = db.get_root_state(&roots[0]).await?.unwrap();
        assert!(root_item_0.pending_valid_as_of < root_1_inserted_at);
        assert_eq!(root_item_0.status, Status::Processed);
        assert!(root_item_0.mined_valid_as_of.unwrap() < root_2_mined_at);
        assert!(root_item_0.mined_valid_as_of.unwrap() > root_1_inserted_at);
        assert!(root_item_0.pending_valid_as_of < root_1_inserted_at);

        Ok(())
    }

    #[tokio::test]
    async fn check_identity_existance() -> anyhow::Result<()> {
        let (db, _db_container) = setup_db().await?;

        let identities = mock_identities(2);
        let roots = mock_roots(1);

        // When there's no identity
        assert!(!db.identity_exists(identities[0]).await?);

        // When there's only unprocessed identity

        db.insert_new_identity(identities[0])
            .await
            .context("Inserting new identity")?;
        assert!(db.identity_exists(identities[0]).await?);

        // When there's only processed identity
        db.insert_pending_identity(0, &identities[1], &roots[0])
            .await
            .context("Inserting identity")?;

        assert!(db.identity_exists(identities[1]).await?);

        Ok(())
    }
}
