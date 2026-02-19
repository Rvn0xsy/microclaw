# Execution Model (Current)

## Default posture

- `sandbox.mode` remains `off` by default to keep first-run setup friction low.
- High-risk actions are guarded by tool risk + approval gates.
- File tools are protected by path guards, sensitive-path blocking, symlink validation, and optional external allowlists.

## Sandbox posture

- Runtime: Docker backend (`auto` / `docker`).
- Enable quickly: `microclaw setup --enable-sandbox`.
- Verify readiness: `microclaw doctor sandbox`.
- If sandbox is enabled but runtime is unavailable:
  - `require_runtime = true`: fail closed.
  - `require_runtime = false`: warn and fall back to host execution.

## Tool execution policy tags

- `bash`: `dual`
- `write_file`: `host-only`
- `edit_file`: `host-only`
- all others: `host-only` (current baseline)

Policy metadata is enforced before tool execution and surfaced in web config self-check.

## Mount and path controls

- Sandbox mount validation:
  - sensitive component blocklist (`.ssh`, `.aws`, `.gnupg`, `.kube`, `.docker`, `.env`, keys, etc.)
  - symlink component rejection
  - optional external mount allowlist (`~/.config/microclaw/mount-allowlist.txt`)
- File path guard:
  - sensitive path deny list
  - symlink validation on existing path prefix
  - optional external path allowlist (`~/.config/microclaw/path-allowlist.txt`)

## Operational recommendation

- Keep default `sandbox=off` for onboarding.
- For production or higher-risk deployments, enable sandbox and require an explicit allowlist.
