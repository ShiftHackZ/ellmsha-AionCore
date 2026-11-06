//! Independent ciphertext-only vault storage. No secrets enter the application database.
use crate::DbError;
use async_trait::async_trait;
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::{path::Path, time::Duration};

pub struct VaultSnapshot {
    pub generation: i64,
    pub ciphertext: Vec<u8>,
}

#[async_trait]
pub trait ISecretsRepository: Send + Sync {
    async fn load(&self, owner: &str) -> Result<Option<VaultSnapshot>, DbError>;
    /// Atomic owner-partition replacement. None creates, Some requires the exact generation.
    async fn replace(&self, owner: &str, expected: Option<i64>, ciphertext: &[u8]) -> Result<i64, DbError>;
}

pub struct SqliteSecretsRepository {
    pool: SqlitePool,
}

impl SqliteSecretsRepository {
    pub async fn open(path: &Path) -> Result<Self, DbError> {
        // Create with private permissions before SQLite can write even the schema.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata =
                        std::fs::symlink_metadata(path).map_err(|_| DbError::Init("vault file metadata".into()))?;
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        return Err(DbError::Init("vault file must be regular".into()));
                    }
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|_| DbError::Init("vault file permissions".into()))?;
                }
                Err(_) => return Err(DbError::Init("vault file create".into())),
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePool::connect_with(options).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS secrets_vaults (owner TEXT PRIMARY KEY NOT NULL, generation INTEGER NOT NULL, ciphertext BLOB NOT NULL)")
            .execute(&pool).await?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl ISecretsRepository for SqliteSecretsRepository {
    async fn load(&self, owner: &str) -> Result<Option<VaultSnapshot>, DbError> {
        Ok(
            sqlx::query("SELECT generation,ciphertext FROM secrets_vaults WHERE owner=?")
                .bind(owner)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| VaultSnapshot {
                    generation: r.get("generation"),
                    ciphertext: r.get("ciphertext"),
                }),
        )
    }

    async fn replace(&self, owner: &str, expected: Option<i64>, ciphertext: &[u8]) -> Result<i64, DbError> {
        match expected {
            None => {
                let result = sqlx::query("INSERT INTO secrets_vaults(owner,generation,ciphertext) VALUES (?,1,?) ON CONFLICT(owner) DO NOTHING")
                    .bind(owner).bind(ciphertext).execute(&self.pool).await?;
                if result.rows_affected() != 1 {
                    return Err(DbError::Conflict("vault generation".into()));
                }
                Ok(1)
            }
            Some(generation) => {
                let next = generation
                    .checked_add(1)
                    .ok_or_else(|| DbError::Conflict("vault generation".into()))?;
                let result =
                    sqlx::query("UPDATE secrets_vaults SET generation=?,ciphertext=? WHERE owner=? AND generation=?")
                        .bind(next)
                        .bind(ciphertext)
                        .bind(owner)
                        .bind(generation)
                        .execute(&self.pool)
                        .await?;
                if result.rows_affected() != 1 {
                    return Err(DbError::Conflict("vault generation".into()));
                }
                Ok(next)
            }
        }
    }
}
