use super::*;

pub(super) async fn api_metrics(
    headers: HeaderMap,
    State(state): State<WebState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    require_scope(&state, &headers, AuthScope::Read).await?;
    persist_metrics_snapshot(&state).await?;

    let snapshot = state.metrics.lock().await.clone();
    let active_sessions = state.request_hub.sessions.lock().await.len() as i64;
    Ok(Json(json!({
        "ok": true,
        "metrics": {
            "http_requests": snapshot.http_requests,
            "llm_completions": snapshot.llm_completions,
            "llm_input_tokens": snapshot.llm_input_tokens,
            "llm_output_tokens": snapshot.llm_output_tokens,
            "tool_executions": snapshot.tool_executions,
            "mcp_calls": snapshot.mcp_calls,
            "active_sessions": active_sessions
        }
    })))
}

pub(super) async fn api_metrics_summary(
    headers: HeaderMap,
    State(state): State<WebState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    api_metrics(headers, State(state)).await
}

pub(super) async fn api_metrics_history(
    headers: HeaderMap,
    State(state): State<WebState>,
    Query(query): Query<MetricsHistoryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    require_scope(&state, &headers, AuthScope::Read).await?;
    persist_metrics_snapshot(&state).await?;

    let minutes = query.minutes.unwrap_or(24 * 60).clamp(1, 24 * 60 * 30);
    let limit = query.limit.unwrap_or(2000).clamp(1, 20000);
    let since = (chrono::Utc::now() - chrono::Duration::minutes(minutes)).timestamp_millis();
    let rows = call_blocking(state.app_state.db.clone(), move |db| {
        db.get_metrics_history(since, limit)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({
        "ok": true,
        "minutes": minutes,
        "points": rows.into_iter().map(|r| json!({
            "timestamp_ms": r.timestamp_ms,
            "llm_completions": r.llm_completions,
            "llm_input_tokens": r.llm_input_tokens,
            "llm_output_tokens": r.llm_output_tokens,
            "http_requests": r.http_requests,
            "tool_executions": r.tool_executions,
            "mcp_calls": r.mcp_calls,
            "active_sessions": r.active_sessions
        })).collect::<Vec<_>>()
    })))
}
