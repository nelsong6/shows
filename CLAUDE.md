# shows

App repo on the romaine.life AKS cluster. Postgres-backed playlist API at `shows.romaine.life` plus a local Windows client that drives mpv.

## Layout

```
cmd/
  shows-api/        HTTP server, runs in AKS
  shows-client/     local Windows binary; spawns mpv, plays episodes forever
  shows-migrate/    one-shot import from the legacy play_show JSON files
internal/
  ordering/         deterministic-random round ordering (SHA-256 → uint32)
  auth/             JWKS verifier + chi middleware for auth.romaine.life JWTs
  device/           auth.romaine.life CLI device flow (used by client + migrate)
  store/            pgx-based DB layer
  api/              chi routes, handlers
  mpv/              named-pipe JSON IPC controller (client only)
migrations/         SQL migrations applied by shows-api on start (goose)
k8s/                Helm chart (CNPG cluster, deployment, HTTPRoute, backups)
tofu/               per-app Azure resources (resource group, backup storage, UAMI)
```

## Bootstrap

This repo is created by `infra-bootstrap`'s app module. Per-repo Azure identity, OIDC federated creds, ACR push, and tfstate access are provisioned there. Don't put those concerns here.

Per the fleet convention (see `infra-bootstrap/CLAUDE.md`):

- **`tofu/`** — applied on push to main by `.github/workflows/tofu.yaml`. Creates `shows-rg`, the CNPG backup storage account + container, and the UAMI that the `shows-db` ServiceAccount federates to.
- **`k8s/`** — Helm chart consumed by the ArgoCD `Application` defined in `infra-bootstrap/k8s/apps/shows.yaml`. ArgoCD auto-syncs on push.
- **`build-and-deploy.yaml`** — builds `cmd/shows-api`, pushes to `romainecr.azurecr.io/shows:<sha>`, bumps `k8s/values.yaml`, commits. ArgoCD picks up the new tag.

The local client (`cmd/shows-client`) and the migrate tool (`cmd/shows-migrate`) are **not** deployed by CI. They run on the user's PC against `https://shows.romaine.life`.

## Auth

Every `/api/*` route requires an auth.romaine.life JWT with `role in {admin, user}`. Tokens are verified against the JWKS at `https://auth.romaine.life/api/auth/jwks` with required claims `["exp", "iat", "iss", "role"]` (mirrors `nelsong6/romaine-auth-py`).

The local client uses the CLI device flow at `POST /api/cli/device` → browser approval → `POST /api/cli/token` (see `auth/src/server.ts:2461` and `auth/src/cli-device-flow.ts`). The token is cached at `%APPDATA%\shows\token.json`; on expiry the client re-runs the flow.

## Postgres (CloudNativePG)

`k8s/templates/cluster.yaml` declares a 2-instance `shows-db` Cluster CR. The CNPG operator (installed cluster-wide via `infra-bootstrap/k8s/cloudnative-pg/`) reconciles the pods, generates the `shows-db-app` Secret, and continuously archives WAL + base backups to the Azure Storage container provisioned in `tofu/backups.tf`. Workload-identity flow mirrors the auth repo: UAMI in `tofu/workload-identity.tf` is federated to `system:serviceaccount:shows:shows-db`.

## Ordering invariant

The deterministic-random round order is computed by `internal/ordering`:

```
hash := SHA-256(UTF-8 bytes of: root_path + "\" + relative_path)
order_value := uint32(first 4 hex chars of hash, parsed as base 16)
shows in round are sorted by order_value ascending
```

This bit-for-bit reproduces the PowerShell `Get-FileHash -InputStream` + `SubString(0,4)` + `[uint32]` cast from the legacy `play_ordered_show.ps1`. The migrate tool relies on this — preserving the same ordering means a partially-watched playlist resumes in the same shuffle order it would have under the old scripts.

## Migration source data

Legacy state lives in `D:\Downloads\Group-Nelson\nelson.json` and the per-show JSONs it points at. Schema:

```json
{
  "Name": "Dr. Katz",
  "Episodes": ["Dr. Katz S06\\Dr.Katz.S06E11.Big.TV.avi", ...],
  "DateAdded": "1/29/2024 8:34:00 AM"
}
```

Episode paths are relative to the parent directory of the per-show JSON file. `cmd/shows-migrate` joins them with that parent to produce the absolute path stored as `episodes.relative_path` and the show's `root_path`.
