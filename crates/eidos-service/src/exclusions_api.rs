use crate::api::{blocking, ApiResult};
use crate::api_json::ApiJson;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use eidos_catalog::exclusions::{
    ApplyExclusions, ExclusionPolicy, ExclusionPreview, PreviewExclusions,
};
use eidos_domain::SourceId;
use std::sync::Arc;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/sources/{id}/policy", get(status).post(apply))
        .route("/sources/{id}/policy/preview", post(preview))
        .route("/sources/{id}/policy/retry", post(retry))
}

async fn status(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<ExclusionPolicy> {
    blocking(move || Ok(ApiJson(st.catalog.exclusion_policy(SourceId(id))?))).await
}

async fn apply(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(request): Json<ApplyExclusions>,
) -> ApiResult<ExclusionPolicy> {
    blocking(move || {
        Ok(ApiJson(
            st.catalog.apply_exclusions(SourceId(id), &request)?,
        ))
    })
    .await
}

async fn preview(
    State(st): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(request): Json<PreviewExclusions>,
) -> ApiResult<Vec<ExclusionPreview>> {
    blocking(move || {
        Ok(ApiJson(
            st.catalog.preview_exclusions(SourceId(id), &request)?,
        ))
    })
    .await
}

async fn retry(State(st): State<Arc<AppState>>, Path(id): Path<i64>) -> ApiResult<ExclusionPolicy> {
    blocking(move || {
        st.catalog.set_policy_error(SourceId(id), None)?;
        Ok(ApiJson(st.catalog.exclusion_policy(SourceId(id))?))
    })
    .await
}
