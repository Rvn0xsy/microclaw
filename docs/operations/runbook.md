# Operations Runbook

## Auth Issues

- Symptom: `401 unauthorized`
  - Check `Authorization: Bearer <api-key-or-legacy-token>` or `mc_session` cookie.
  - Verify key scopes via `GET /api/auth/api_keys`.
- Symptom: unsure whether deployment config is safe
  - Run `GET /api/config/self_check` and inspect `risk_level` + `warnings`.
  - Fix high-severity items first (`severity: high`).

- Symptom: login throttled
  - Login endpoint rate-limits repeated attempts per client key.
  - Wait for cooldown window and retry.

## Hook Issues

- List hooks: `microclaw hooks list`
- Inspect hook: `microclaw hooks info <name>`
- Disable bad hook quickly: `microclaw hooks disable <name>`

If a hook times out or crashes, runtime skips the hook and continues.

## Session Fork Issues

- Inspect tree: `GET /api/sessions/tree`
- Create branch: `POST /api/sessions/fork`
- Deleting parent session does not cascade to children.

## Metrics Issues

- Check snapshot: `GET /api/metrics`
- Check history: `GET /api/metrics/history?minutes=60`
- If OTLP is enabled, verify `channels.observability.otlp_endpoint` is reachable.
- If points are missing under burst traffic, raise `otlp_queue_capacity` and review retry settings.

If history is empty, generate traffic first and re-check.
