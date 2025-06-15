use crate::infrastructure::user_repository::{NewUser, User, UserRepository, UserRepositoryError};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use async_trait::async_trait;
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, RequestPartsExt, Router,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc}; // use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use redis::{AsyncCommands, RedisError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt::Display;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use strum_macros::{EnumString, ToString};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;
static KEYS: LazyLock<Keys> = LazyLock::new(|| {
    let secret = std::env::var("JWT_SECRET").expect("JWT_SECRET must be set");
    Keys::new(secret.as_bytes())
});

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Invalid credentials")]
    InvalidCredentials,
    #[error("Token expired")]
    TokenExpired,
    #[error("Invalid token")]
    InvalidToken,
    #[error("Token blacklisted")]
    TokenBlacklisted,
    #[error("Rate limit exceeded")]
    RateLimitExceeded,
    #[error("User not found")]
    UserNotFound,
    #[error("Password hash error: {0}")]
    PasswordHashError(String),
    #[error("Redis error: {0}")]
    RedisError(#[from] RedisError),
    #[error("JWT error: {0}")]
    JwtError(#[from] jsonwebtoken::errors::Error),
    #[error("Internal error: {0}")]
    InternalError(String),
    #[error("Wrong credentials")]
    WrongCredentials,
    #[error("Missing credentials")]
    MissingCredentials,
    #[error("Token creation error")]
    TokenCreation,
    #[error("Username '{0}' already exists")]
    UsernameAlreadyExists(String),
    #[error("Email '{0}' already exists")]
    EmailAlreadyExists(String),
    #[error("User repository error: {0}")]
    UserRepositoryError(String),
    #[error("Account is locked")]
    AccountLocked,
}

impl From<UserRepositoryError> for AuthError {
    fn from(err: UserRepositoryError) -> Self {
        match err {
            UserRepositoryError::NotFoundById(_) => AuthError::UserNotFound,
            UserRepositoryError::NotFoundByUsername(_) => AuthError::UserNotFound,
            UserRepositoryError::UsernameExists(username) => {
                AuthError::UsernameAlreadyExists(username)
            }
            UserRepositoryError::EmailExists(email) => AuthError::EmailAlreadyExists(email),
            UserRepositoryError::DatabaseError(db_err) => {
                AuthError::UserRepositoryError(db_err.to_string())
            }
            UserRepositoryError::Unexpected(msg) => AuthError::InternalError(msg),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // username
    pub exp: i64,
    pub iat: i64,
    pub roles: Vec<UserRole>,
    pub token_type: TokenType,
    pub jti: String,
    pub company: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy, ToString, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum TokenType {
    Access,
    Refresh,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, ToString, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum UserRole {
    Admin,
    BankManager,
    Customer,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub refresh_token_secret: String,
    pub access_token_expiry: i64,
    pub refresh_token_expiry: i64,
    pub rate_limit_requests: u32,
    pub rate_limit_window: u32,
    pub max_failed_attempts: u32,
    pub lockout_duration_minutes: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub user_id: String,
    pub roles: Vec<UserRole>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RefreshTokenRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PasswordResetRequest {
    pub email: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PasswordResetResponse {
    pub reset_token: String,
    pub expires_in: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogoutRequest {
    pub token: String,
}

#[derive(Clone)]
pub struct AuthService {
    redis_client: Arc<redis::Client>,
    config: AuthConfig,
    // users: Arc<RwLock<Vec<User>>>,
    user_repository: Arc<UserRepository>,
}

impl AuthService {
    pub fn new(
        redis_client: Arc<redis::Client>,
        config: AuthConfig,
        user_repository: Arc<UserRepository>,
    ) -> Self {
        Self {
            redis_client,
            config,
            // users: Arc::new(RwLock::new(Vec::new())),
            user_repository,
        }
    }

    pub async fn register_user(
        &self,
        username: &str,
        email: &str,
        password: &str,
        roles: Vec<UserRole>,
    ) -> Result<User, AuthError> {
        // let mut users = self.user_repository.write().await;

        // // Check if username already exists
        // if users.iter().any(|u| u.username == username) {
        //     return Err(AuthError::InternalError("Username already exists".into()));
        // }

        // Hash password
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let password_hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| AuthError::PasswordHashError(e.to_string()))?
            .to_string();

        // let user = User {
        //     id: Uuid::new_v4().to_string(),
        //     username: username.to_string(),
        //     password_hash,
        //     roles,
        //     is_active: true,
        //     last_login: None,
        //     failed_login_attempts: 0,
        //     locked_until: None,
        // };
        let user = NewUser {
            username,
            email,
            password_hash,
            roles: roles.iter().map(|r| r.to_string()).collect(),
            // is_active: Some(true),
        };

        // users.push(user.clone());
        let user = self.user_repository.create(&user).await?;
        Ok(user)
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<LoginResponse, AuthError> {
        // let mut users = self.users.write().await;
        // let user = users
        //     .iter_mut()
        //     .find(|u| u.username == username)
        let user = self
            .user_repository
            .find_by_username(username)
            .await?
            .ok_or(AuthError::UserNotFound)?;
        if !user.is_active {
            return Err(AuthError::InternalError("Account is not active".into()));
        }
        if let Some(locked_until_ts) = user.locked_until {
            if Utc::now() < locked_until_ts {
                return Err(AuthError::InternalError("Account is locked".into()));
            }
            self.user_repository
                .update_lockout(user.id, None, 0)
                .await?;
        }

        // Check if account is locked
        // if let Some(locked_until) = user.locked_until {
        //     if Utc::now() < locked_until {
        //         return Err(AuthError::InternalError("Account is locked".into()));
        //     }
        //     user.locked_until = None;
        //     user.failed_login_attempts = 0;
        // }

        // Verify password
        let parsed_hash = PasswordHash::new(&user.password_hash)
            .map_err(|e| AuthError::PasswordHashError(e.to_string()))?;

        if !Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_ok()
        {
            // user.failed_login_attempts += 1;
            // if user.failed_login_attempts >= self.config.max_failed_attempts {
            //     user.locked_until = Some(
            //         Utc::now() + Duration::minutes(self.config.lockout_duration_minutes as i64),
            //     );
            //     return Err(AuthError::InternalError(
            //         "Account locked due to too many failed attempts".into(),
            //     ));
            let new_failed_attempts = self
                .user_repository
                .increment_failed_attempts(user.id)
                .await?;
            if new_failed_attempts >= self.config.max_failed_attempts.try_into().unwrap() {
                let locked_until = Utc::now()
                    + ChronoDuration::minutes(self.config.lockout_duration_minutes.into());

                self.user_repository
                    .update_lockout(user.id, Some(locked_until), new_failed_attempts)
                    .await?;

                return Err(AuthError::AccountLocked);
            }
            return Err(AuthError::InvalidCredentials);
        }

        // Reset failed attempts and update last login
        // user.failed_login_attempts = 0;
        // user.last_login = Some(Utc::now());
        // Reset failed attempts and update last login on successful loginAdd commentMore actions

        self.user_repository
            .update_login_info(user.id, Utc::now(), 0, None)
            .await?;
        let user_roles: Vec<UserRole> = user
            .roles
            .iter()
            .map(|s| UserRole::from_str(s).unwrap_or(UserRole::Customer)) // Default to Customer on parse error
            .collect();
        // Generate tokens
        let access_token = self
            .generate_token(&user.username, &user_roles, TokenType::Access)
            .await?;
        let refresh_token = self
            .generate_token(&user.username, &user_roles, TokenType::Refresh)
            .await?;

        Ok(LoginResponse {
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: self.config.access_token_expiry,
            user_id: user.id.to_string(),
            roles: user_roles,
        })
    }

    pub async fn refresh_token(&self, refresh_token_str: &str) -> Result<LoginResponse, AuthError> {
        let claims = self
            .validate_token(refresh_token_str, TokenType::Refresh)
            .await?;

        let user = self
            .user_repository
            .find_by_username(&claims.sub)
            .await?
            .ok_or(AuthError::UserNotFound)?;

        if !user.is_active {
            return Err(AuthError::InternalError("Account is inactive".into()));
        }
        let user_roles: Vec<UserRole> = user
            .roles
            .iter()
            .map(|s| UserRole::from_str(s).unwrap_or(UserRole::Customer))
            .collect();

        let access_token = self
            .generate_token(&user.username, &user_roles, TokenType::Access)
            .await?;
        let new_refresh_token = self
            .generate_token(&user.username, &user_roles, TokenType::Refresh)
            .await?;

        Ok(LoginResponse {
            access_token,
            refresh_token: new_refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: self.config.access_token_expiry,
            user_id: user.id.to_string(),
            roles: user_roles,
        })
    }

    pub async fn change_password(
        &self,
        username: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        let user = self
            .user_repository
            .find_by_username(username)
            .await?
            .ok_or(AuthError::UserNotFound)?;

        // Verify current password
        let parsed_hash = PasswordHash::new(&user.password_hash)
            .map_err(|e| AuthError::PasswordHashError(e.to_string()))?;

        if !Argon2::default()
            .verify_password(current_password.as_bytes(), &parsed_hash)
            .is_ok()
        {
            return Err(AuthError::InvalidCredentials);
        }

        // Hash new password
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let new_password_hash = argon2
            .hash_password(new_password.as_bytes(), &salt)
            .map_err(|e| AuthError::PasswordHashError(e.to_string()))?
            .to_string();

        // user.password_hash = new_password_hash;
        self.user_repository
            .update_password_hash(user.id, &new_password_hash)
            .await?;
        Ok(())
    }

    // pub async fn request_password_reset(
    //     &self,
    //     email: &str,
    // ) -> Result<PasswordResetResponse, AuthError> {
    //     let users = self.users.read().await;
    //     let user = users
    //         .iter()
    //         .find(|u| u.username == email)
    //         .ok_or(AuthError::UserNotFound)?;

    //     // Generate reset token
    //     let reset_token = self
    //         .generate_token(&user.username, &user.roles, TokenType::Access)
    //         .await?;

    //     // Store reset token in Redis with expiration
    //     let mut conn = self.redis_client.get_async_connection().await?;
    //     conn.set_ex(
    //         format!("reset_token:{}", user.id),
    //         &reset_token,
    //         self.config.access_token_expiry as u64,
    //     )
    //     .await?;

    //     Ok(PasswordResetResponse {
    //         reset_token,
    //         expires_in: self.config.access_token_expiry,
    //     })
    // }

    async fn generate_token(
        &self,
        username: &str,
        roles: &[UserRole],
        token_type: TokenType,
    ) -> Result<String, AuthError> {
        let now = Utc::now();
        let exp_duration = match token_type {
            TokenType::Access => ChronoDuration::seconds(self.config.access_token_expiry),
            TokenType::Refresh => ChronoDuration::seconds(self.config.refresh_token_expiry),
        };
        let exp = (now + exp_duration).timestamp();
        let claims = Claims {
            sub: username.to_string(),
            exp,
            iat: now.timestamp(),
            roles: roles.to_vec(),
            token_type,
            jti: Uuid::new_v4().to_string(),
            company: "ACME".to_string(),
        };

        let secret_key = match claims.token_type {
            TokenType::Access => &self.config.jwt_secret,
            TokenType::Refresh => &self.config.refresh_token_secret,
        };

        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret_key.as_bytes()),
        )
        .map_err(AuthError::JwtError)
    }

    pub async fn validate_token(
        &self,
        token: &str,
        expected_type: TokenType,
    ) -> Result<Claims, AuthError> {
        // Check if token is blacklisted
        let mut conn = self.redis_client.get_async_connection().await?;
        let is_blacklisted: bool = conn
            .get(format!("blacklist:{}", token))
            .await
            .unwrap_or(false);

        if is_blacklisted {
            return Err(AuthError::TokenBlacklisted);
        }

        // Validate token
        let validation = Validation::new(Algorithm::HS256);

        let secret = match expected_type {
            TokenType::Access => &self.config.jwt_secret,
            TokenType::Refresh => &self.config.refresh_token_secret,
        };

        let token_data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &validation,
        )?;

        if token_data.claims.token_type != expected_type {
            return Err(AuthError::InvalidToken);
        }

        Ok(token_data.claims)
    }

    pub async fn blacklist_token(&self, token: &str) -> Result<(), AuthError> {
        let claims = self.validate_token(token, TokenType::Access).await?;
        let mut conn = self.redis_client.get_async_connection().await?;

        // Blacklist token until it expires
        let ttl = claims.exp - Utc::now().timestamp();
        if ttl > 0 {
            conn.set_ex(format!("blacklist:{}", token), true, ttl as u64)
                .await?;
        }

        Ok(())
    }

    pub async fn check_rate_limit(&self, key: &str) -> Result<(), AuthError> {
        let mut conn = self.redis_client.get_async_connection().await?;
        let current: i64 = conn.incr(format!("rate_limit:{}", key), 1).await?;

        if current == 1 {
            conn.expire(
                format!("rate_limit:{}", key),
                self.config.rate_limit_window as i64,
            )
            .await?;
        }

        if current > self.config.rate_limit_requests as i64 {
            return Err(AuthError::RateLimitExceeded);
        }

        Ok(())
    }
}

// Add Default implementation for AuthService
// impl Default for AuthService {
//     fn default() -> Self {
//         let redis_client = Arc::new(
//             redis::Client::open("redis://localhost:6379").expect("Failed to connect to Redis"),
//         );
//         let auth_config = AuthConfig {
//             jwt_secret: "default_secret".to_string(),
//             refresh_token_secret: "default_refresh_secret".to_string(),
//             access_token_expiry: 3600,
//             refresh_token_expiry: 604800,
//             rate_limit_requests: 100,
//             rate_limit_window: 60,
//             max_failed_attempts: 5,
//             lockout_duration_minutes: 30,
//         };
//         let user_repository = Arc::new(UserRepository::new(
//             "postgres://postgres:postgres@localhost:5432/postgres".to_string(),
//         ));
//         AuthService::new(redis_client, auth_config, user_repository)
//     }
// }

#[async_trait]
impl<S> FromRequestParts<S> for Claims
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    fn from_request_parts<'a, 'b>(
        parts: &'a mut Parts,
        _state: &'b S,
    ) -> impl futures::Future<Output = Result<Self, <Self as FromRequestParts<S>>::Rejection>>
           + std::marker::Send {
        async move {
            let TypedHeader(Authorization(bearer)) = parts
                .extract::<TypedHeader<Authorization<Bearer>>>()
                .await
                .map_err(|_| AuthError::InvalidToken)?;
            // Decode the user data
            let token_data =
                decode::<Claims>(bearer.token(), &KEYS.decoding, &Validation::default())
                    .map_err(|_| AuthError::InvalidToken)?;

            Ok(token_data.claims)
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, error_message) = match &self {
            AuthError::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, "Invalid credentials".to_string())
            }
            AuthError::TokenExpired => (StatusCode::UNAUTHORIZED, "Token expired".to_string()),
            AuthError::InvalidToken => (StatusCode::UNAUTHORIZED, "Invalid token".to_string()),
            AuthError::TokenBlacklisted => (
                StatusCode::UNAUTHORIZED,
                "Token has been revoked".to_string(),
            ),
            AuthError::RateLimitExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded".to_string(),
            ),
            AuthError::UserNotFound => (StatusCode::NOT_FOUND, "User not found".to_string()),
            AuthError::PasswordHashError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Password processing error".to_string(),
            ),
            AuthError::RedisError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            ),
            AuthError::JwtError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Token processing error".to_string(),
            ),
            AuthError::InternalError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ),
            AuthError::WrongCredentials => {
                (StatusCode::UNAUTHORIZED, "Wrong credentials".to_string())
            }
            AuthError::MissingCredentials => {
                (StatusCode::BAD_REQUEST, "Missing credentials".to_string())
            }
            AuthError::TokenCreation => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Token creation error".to_string(),
            ),
            AuthError::UsernameAlreadyExists(_) => (
                StatusCode::BAD_REQUEST,
                "Username already exists".to_string(),
            ),
            AuthError::EmailAlreadyExists(_) => {
                (StatusCode::BAD_REQUEST, "Email already exists".to_string())
            }
            AuthError::UserRepositoryError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            ),
            AuthError::AccountLocked => (StatusCode::UNAUTHORIZED, "Account is locked".to_string()),
        };

        let body = Json(serde_json::json!({
            "error": error_message
        }));

        (status, body).into_response()
    }
}

struct Keys {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl Keys {
    fn new(secret: &[u8]) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
        }
    }
}

pub async fn authorize(Json(payload): Json<AuthPayload>) -> Result<Json<AuthBody>, AuthError> {
    if payload.client_id.is_empty() || payload.client_secret.is_empty() {
        return Err(AuthError::MissingCredentials);
    }

    // Here you would typically validate against a database
    if payload.client_id != "foo" || payload.client_secret != "bar" {
        return Err(AuthError::WrongCredentials);
    }

    let claims = Claims {
        sub: "b@b.com".to_owned(),
        company: "ACME".to_owned(),
        exp: 2000000000, // May 2033
        iat: chrono::Utc::now().timestamp(),
        roles: vec![UserRole::Customer],
        token_type: TokenType::Access,
        jti: Uuid::new_v4().to_string(),
    };

    let token = encode(&Header::default(), &claims, &KEYS.encoding)
        .map_err(|_| AuthError::TokenCreation)?;

    Ok(Json(AuthBody::new(token)))
}

pub async fn protected(claims: Claims) -> Result<String, AuthError> {
    Ok(format!(
        "Welcome to the protected area :)\nYour data:\n{claims:?}",
    ))
}

#[derive(Debug, Deserialize)]
pub struct AuthPayload {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Serialize)]
pub struct AuthBody {
    access_token: String,
    token_type: String,
}

impl AuthBody {
    pub fn new(access_token: String) -> Self {
        Self {
            access_token,
            token_type: "Bearer".to_string(),
        }
    }
}
