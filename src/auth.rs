use crate::state::{AppState, User};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct AuthRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub username: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub fn create_auth_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/auth/register", post(register_handler))
        .route("/api/auth/login", post(login_handler))
        .route("/api/auth/me", get(me_handler))
        .route("/api/auth/status", get(status_handler))
}

async fn status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "registration_disabled": state.disable_registration
    }))
}

fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize())
}

async fn register_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AuthRequest>,
) -> impl IntoResponse {
    if state.disable_registration {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::to_value(ErrorResponse {
                error: "New user registration has been disabled by the server administrator.".into(),
            }).unwrap()),
        ).into_response();
    }

    let username = payload.username.trim().to_string();
    if username.is_empty() || payload.password.len() < 3 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::to_value(ErrorResponse {
                error: "Username required and password must be at least 3 characters".into(),
            }).unwrap()),
        ).into_response();
    }

    let mut users = state.users.write().await;
    if users.contains_key(&username) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::to_value(ErrorResponse {
                error: "Username is already registered. Please log in.".into(),
            }).unwrap()),
        ).into_response();
    }

    let token = Uuid::new_v4().to_string();
    let user = User {
        username: username.clone(),
        password_hash: hash_password(&payload.password),
        token: token.clone(),
        created_at: Utc::now(),
    };

    users.insert(username.clone(), user);
    {
        let mut tokens = state.tokens.write().await;
        tokens.insert(token.clone(), username.clone());
    }
    drop(users);
    state.save_auth().await;

    (
        StatusCode::CREATED,
        Json(serde_json::to_value(AuthResponse {
            token,
            username,
            message: "Successfully registered and logged in!".into(),
        }).unwrap()),
    ).into_response()
}

async fn login_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AuthRequest>,
) -> impl IntoResponse {
    let username = payload.username.trim().to_string();
    let password_hash = hash_password(&payload.password);

    let mut users = state.users.write().await;
    if let Some(user) = users.get_mut(&username) {
        if user.password_hash == password_hash {
            // Generate a fresh session token
            let new_token = Uuid::new_v4().to_string();
            let old_token = user.token.clone();
            user.token = new_token.clone();

            {
                let mut tokens = state.tokens.write().await;
                tokens.remove(&old_token);
                tokens.insert(new_token.clone(), username.clone());
            }
            drop(users);
            state.save_auth().await;

            return (
                StatusCode::OK,
                Json(serde_json::to_value(AuthResponse {
                    token: new_token,
                    username,
                    message: "Login successful".into(),
                }).unwrap()),
            ).into_response();
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::to_value(ErrorResponse {
            error: "Invalid username or password".into(),
        }).unwrap()),
    ).into_response()
}

async fn me_handler(
    State(state): State<Arc<AppState>>,
    headers: header::HeaderMap,
) -> impl IntoResponse {
    let auth_header = headers.get("Authorization").and_then(|h| h.to_str().ok());
    let token = if let Some(h) = auth_header {
        h.trim_start_matches("Bearer ").to_string()
    } else {
        String::new()
    };

    if let Some(user) = state.verify_token(&token).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "username": user.username,
                "created_at": user.created_at,
            })),
        ).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        ).into_response()
    }
}
