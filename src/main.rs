// Copyright (c) Mysten Labs, Inc.
// Copyright (c) The Social Proof Foundation, LLC.
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::Path,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

#[derive(Serialize)]
struct HealthResponse {
    status: String,
}

#[derive(Deserialize)]
struct FaucetRequest {
    recipient: String,
}

#[derive(Serialize)]
struct FaucetResponse {
    task_id: String,
    message: String,
}

#[tokio::main]
async fn main() {
    // Cors layer
    let cors = CorsLayer::new()
        .allow_methods(tower_http::cors::Any)
        .allow_headers(Any)
        .allow_origin(Any);

    // Build our application with routes
    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/gas", post(request_gas))
        .route("/v1/gas", post(request_gas))
        .route("/v1/status/:task_id", get(request_status))
        .layer(cors);

    // Get the port from environment variable or default to 5003
    let port = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(5003);
    
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Listening on {}", addr);
    
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn root() -> &'static str {
    "MYS Faucet"
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "OK".to_string(),
    })
}

async fn request_gas(Json(payload): Json<FaucetRequest>) -> Json<FaucetResponse> {
    // Generate a random task ID
    let task_id = Uuid::new_v4().to_string();
    
    println!("Received gas request for recipient: {}", payload.recipient);
    
    Json(FaucetResponse {
        task_id,
        message: format!("Request accepted for {}", payload.recipient),
    })
}

async fn request_status(Path(task_id): Path<String>) -> Json<FaucetResponse> {
    println!("Status request for task: {}", task_id);
    
    Json(FaucetResponse {
        task_id,
        message: "In progress".to_string(),
    })
}
