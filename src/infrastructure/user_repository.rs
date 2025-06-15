use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize}; // Added for potential future use, not strictly required by sqlx::FromRow
use sqlx::{postgres::PgDatabaseError, FromRow, PgPool};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, FromRow, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub roles: Vec<String>, // Stored as TEXT[] in PostgreSQL
    pub is_active: bool,
    pub registered_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub failed_login_attempts: i32,
    pub locked_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct NewUser<'a> {
    pub username: &'a str,
    pub email: &'a str,
    pub password_hash: String,
    pub roles: Vec<String>,
    // is_active and registered_at will use database defaults or be set by INSERT query
}

#[derive(Error, Debug)]
pub enum UserRepositoryError {
    #[error("User not found for ID: {0}")]
    NotFoundById(Uuid),
    #[error("User not found for username: {0}")]
    NotFoundByUsername(String),
    #[error("Username '{0}' already exists")]
    UsernameExists(String),
    #[error("Email '{0}' already exists")]
    EmailExists(String),
    #[error("Database error: {0}")]
    DatabaseError(#[from] sqlx::Error),
    #[error("An unexpected error occurred: {0}")]
    Unexpected(String), // For any other kind of error
}

#[derive(Clone)]
pub struct UserRepository {
    pool: PgPool,
}

impl UserRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, new_user: &NewUser<'_>) -> Result<User, UserRepositoryError> {
        let user_id = Uuid::new_v4(); // Application generates UUID

        let user = sqlx::query_as!(
            User,
            r#"
            INSERT INTO users (id, username, email, password_hash, roles)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING
                id, username, email, password_hash, roles,
                is_active, registered_at, last_login_at,
                failed_login_attempts, locked_until
            "#,
            user_id,
            new_user.username,
            new_user.email,
            new_user.password_hash,
            &new_user.roles // Pass as slice for TEXT[]
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e: sqlx::Error| {
            if let Some(db_err) = e.as_database_error() {
                if let Some(pg_err) = db_err.try_downcast_ref::<PgDatabaseError>() {
                    if pg_err.code() == "23505" {
                        // unique_violation
                        // Check constraint name to differentiate between username and email
                        if let Some(constraint_name) = pg_err.constraint() {
                            if constraint_name == "users_username_key" {
                                return UserRepositoryError::UsernameExists(
                                    new_user.username.to_string(),
                                );
                            }
                            if constraint_name == "users_email_key" {
                                return UserRepositoryError::EmailExists(
                                    new_user.email.to_string(),
                                );
                            }
                        }
                    }
                }
            }
            UserRepositoryError::DatabaseError(e)
        })?;

        Ok(user)
    }

    pub async fn find_by_username(
        &self,
        username: &str,
    ) -> Result<Option<User>, UserRepositoryError> {
        sqlx::query_as!(
            User,
            r#"
            SELECT
                id, username, email, password_hash, roles,
                is_active, registered_at, last_login_at,
                failed_login_attempts, locked_until
            FROM users
            WHERE username = $1
            "#,
            username
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)
    }

    pub async fn find_by_id(&self, user_id: Uuid) -> Result<Option<User>, UserRepositoryError> {
        sqlx::query_as!(
            User,
            r#"
            SELECT
                id, username, email, password_hash, roles,
                is_active, registered_at, last_login_at,
                failed_login_attempts, locked_until
            FROM users
            WHERE id = $1
            "#,
            user_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)
    }

    pub async fn update_login_info(
        &self,
        user_id: Uuid,
        last_login_at: DateTime<Utc>,
        failed_login_attempts: i32,
        locked_until: Option<DateTime<Utc>>,
    ) -> Result<(), UserRepositoryError> {
        let result = sqlx::query!(
            r#"
            UPDATE users
            SET
                last_login_at = $1,
                failed_login_attempts = $2,
                locked_until = $3
            WHERE id = $4
            "#,
            Some(last_login_at),
            failed_login_attempts,
            locked_until,
            user_id
        )
        .execute(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)?;

        if result.rows_affected() == 0 {
            return Err(UserRepositoryError::NotFoundById(user_id));
        }
        Ok(())
    }

    pub async fn update_password_hash(
        &self,
        user_id: Uuid,
        new_password_hash: &str,
    ) -> Result<(), UserRepositoryError> {
        let result = sqlx::query!(
            r#"
            UPDATE users
            SET password_hash = $1
            WHERE id = $2
            "#,
            new_password_hash,
            user_id
        )
        .execute(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)?;

        if result.rows_affected() == 0 {
            return Err(UserRepositoryError::NotFoundById(user_id));
        }
        Ok(())
    }

    pub async fn update_status(
        &self,
        user_id: Uuid,
        is_active: bool,
    ) -> Result<(), UserRepositoryError> {
        let result = sqlx::query!(
            r#"
            UPDATE users
            SET is_active = $1
            WHERE id = $2
            "#,
            is_active,
            user_id
        )
        .execute(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)?;

        if result.rows_affected() == 0 {
            return Err(UserRepositoryError::NotFoundById(user_id));
        }
        Ok(())
    }

    pub async fn increment_failed_attempts(
        &self,
        user_id: Uuid,
    ) -> Result<i32, UserRepositoryError> {
        let record = sqlx::query!(
            r#"
            UPDATE users
            SET failed_login_attempts = failed_login_attempts + 1
            WHERE id = $1
            RETURNING failed_login_attempts
            "#,
            user_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)?;

        match record {
            Some(rec) => Ok(rec.failed_login_attempts),
            None => Err(UserRepositoryError::NotFoundById(user_id)),
        }
    }

    pub async fn update_lockout(
        &self,
        user_id: Uuid,
        locked_until: Option<DateTime<Utc>>,
        failed_attempts_value: i32,
    ) -> Result<(), UserRepositoryError> {
        let result = sqlx::query!(
            r#"
            UPDATE users
            SET
                locked_until = $1,
                failed_login_attempts = $2
            WHERE id = $3
            "#,
            locked_until,
            failed_attempts_value,
            user_id
        )
        .execute(&self.pool)
        .await
        .map_err(UserRepositoryError::DatabaseError)?;

        if result.rows_affected() == 0 {
            return Err(UserRepositoryError::NotFoundById(user_id));
        }
        Ok(())
    }

    pub async fn delete(&self, user_id: Uuid) -> Result<u64, UserRepositoryError> {
        let result = sqlx::query!("DELETE FROM users WHERE id = $1", user_id)
            .execute(&self.pool)
            .await
            .map_err(UserRepositoryError::DatabaseError)?;

        Ok(result.rows_affected())
    }
}
