use crate::{
    api::{blocking, ApiError, ApiResult},
    api_json::ApiJson,
    device_control::{DeviceLimits, DeviceView},
    state::AppState,
};
use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/devices", get(status).post(set_limits))
}

async fn status(State(state): State<Arc<AppState>>) -> ApiResult<DeviceView> {
    state.devices.refresh();
    Ok(ApiJson(state.devices.view()))
}

async fn set_limits(
    State(state): State<Arc<AppState>>,
    Json(limits): Json<DeviceLimits>,
) -> ApiResult<DeviceView> {
    limits.validate().map_err(ApiError::bad_request)?;
    blocking(move || {
        state
            .devices
            .save_limits(&state.data_dir, limits)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        Ok(ApiJson(state.devices.view()))
    })
    .await
}
