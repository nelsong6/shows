# shows

App repo on the romaine.life AKS cluster. Cosmos-backed playlist API at `shows.romaine.life` plus a Wails-based Windows desktop app that drives libmpv for round-robin playback.

## Quality timeframe

This repo follows the long-term, heavy-solution operating mode codified in `nelsong6/glimmung/docs/quality-timeframes.md`. Compatibility layers are prohibited per `nelsong6/tank-operator/docs/migration-policy.md` — `legacy`, `compatibility`, `fallback`, `temporary`, and `exception` are deletion targets, not design options.

When extending a feature documented at `docs/feature-contracts/`, name the affected contract in the PR and explain how the implementation proves the invariants still hold.

## Layout

```
cmd/
  shows-api/          HTTP server, runs in AKS
  shows-migrate/      one-shot import from the legacy play_show JSON files
                      (kept until the desktop absorbs in-app import)
internal/
  ordering/           deterministic-random round ordering (SHA-256 → uint32)
  auth/               JWKS verifier + chi middleware for auth.romaine.life JWTs
  device/             auth.romaine.life CLI device flow (used by shows-migrate)
  store/              Cosmos SDK store layer (shows + watch_history containers)
  api/                chi routes, handlers, Prometheus metrics
desktop/
  app.go              Wails-bound App struct; methods auto-exposed to TS frontend
  main.go             wails.Run entry; window options
  internal/
    player/           libmpv cgo wrapper (supersonic-app/go-mpv)
    win32/            HWND lookup so libmpv parents into the Wails window
    oauth/            user-login flow against auth.romaine.life (PKCE + loopback)
    apiclient/        shows.romaine.life HTTP client + 401 refresh hook
    playlist/         round-robin runner (fetch → queue → wait → advance)
  frontend/           Vite + React + TS using glimmung's design-system tokens
  scripts/            setup-libmpv.ps1 + build.ps1
  third_party/        gitignored; libmpv DLL + headers per-machine
docs/
  feature-contracts/  durable invariant docs (round-and-advance, …)
k8s/                  Helm chart (Deployment, Service, HTTPRoute, XListenerSet,
                      Certificate, PodMonitor) — ArgoCD-synced
tofu/                 shows-rg, shows-identity UAMI, Cosmos DB + role assignment
```

## Bootstrap

This repo was created by `infra-bootstrap`'s app module. Per-repo Azure identity, OIDC federated creds, ACR push, and tfstate access are provisioned there. Per-runtime resources (Cosmos database, runtime UAMI) are in `tofu/` here.

Per the fleet convention:

- **`tofu/`** — applied on push to main by `.github/workflows/tofu.yaml`. Creates `shows-rg`, the `shows-identity` UAMI federated to `system:serviceaccount:shows:shows`, the `shows` Cosmos SQL database, and the data-plane role assignment scoped to `dbs/shows`.
- **`k8s/`** — Helm chart consumed by the ArgoCD `Application` defined in `infra-bootstrap/k8s/apps/shows.yaml`. ArgoCD auto-syncs on push.
- **`.github/workflows/build-and-deploy.yaml`** — builds `cmd/shows-api`, pushes to `romainecr.azurecr.io/shows:<sha>`, bumps `k8s/values.yaml`, commits. ArgoCD picks up the new tag.

The desktop app (`desktop/`) and the migrate tool (`cmd/shows-migrate`) are **not** deployed by CI. They build locally per-machine; the desktop's build pipeline is `desktop/scripts/build.ps1`.

## Auth

Every `/api/*` route requires an auth.romaine.life JWT with `role in {admin, user}`. Tokens are verified against the JWKS at `https://auth.romaine.life/api/auth/jwks` with required claims `["exp", "iat", "iss", "role"]` (mirrors `nelsong6/romaine-auth-py`).

The desktop app uses the **user-login** path at `GET /api/auth/cli/user-login` (PKCE + loopback `redirect_uri`). If the user has no `.romaine.life` session cookie, auth.romaine.life bounces them through Microsoft/Google and returns; the server then redirects to the loopback with a one-time `?code=...`. The desktop POSTs `{grant_type: authorization_code, code, code_verifier, redirect_uri}` to `/api/auth/cli/user-token` and receives the user's own JWT (`role=user|admin`, no `purpose` claim — same shape the browser session would yield). The JWT never travels through the browser. Token caches at `%APPDATA%\shows\token.json`; on 401 the apiclient calls back to `oauth.EnsureToken` for an in-place refresh.

`cmd/shows-migrate` still uses the **bot-token** CLI flow (`internal/device` → `/api/cli/device` + `/api/cli/token`) because it's an unattended import script, not a user-facing app. Both go away when the desktop grows an in-app import flow.

## Cosmos store

Two containers on `infra-cosmos-serverless` / `dbs/shows`:

- **`shows`** — one doc per show, partitioned by `/playlist`. Episodes are embedded as a nested array; each `/advance` is a single point-write.
- **`watch_history`** — append-only event log, partitioned by `/show_id`. Source of truth for the "took N days to watch" reveal.

The runtime pod attaches to Cosmos via workload identity: `serviceaccount/shows:shows` ↔ `shows-identity` UAMI (federated cred in `tofu/identity.tf`) ↔ Cosmos data-plane `Built-in Data Contributor` role scoped to `dbs/shows` only.

## Ordering invariant

The deterministic-random round order is computed by `internal/ordering`:

```
hash := SHA-256(UTF-8 bytes of: root_path + "\" + relative_path)
order_value := uint32(first 4 hex chars of hash, parsed as base 16)
shows in round are sorted by order_value ascending
```

Bit-for-bit reproduces `Get-FileHash -InputStream` + `SubString(0,4)` + `[uint32]` from the legacy `play_ordered_show.ps1`. Tests in `internal/ordering/ordering_test.go` lock the contract against three public SHA-256 fixtures. See [`docs/feature-contracts/round-and-advance.md`](docs/feature-contracts/round-and-advance.md) for the six invariants the round + advance pair satisfies.

## Migration source data

Legacy state at `D:\Downloads\Group-Nelson\nelson.json` + the per-show JSONs it points at:

```json
{
  "Name": "Dr. Katz",
  "Episodes": ["Dr. Katz S06\\Dr.Katz.S06E11.Big.TV.avi", ...],
  "DateAdded": "1/29/2024 8:34:00 AM"
}
```

Episode paths are relative to the parent directory of the per-show JSON. `cmd/shows-migrate` joins them with that parent to produce the absolute path stored as `episodes.relative_path` and the show's `root_path`. Run once on any machine that has the legacy `nelson.json` reachable; idempotent re-runs are not supported (would create duplicate show docs).

## Observability

`shows_*` Prometheus metrics exposed at `/metrics` (no auth) on the AKS pod, scraped by the kube-prometheus-stack via `k8s/templates/podmonitor.yaml`. Catalog in [`docs/feature-contracts/round-and-advance.md`](docs/feature-contracts/round-and-advance.md). Grafana sees them in the `monitoring` namespace's dashboards.

## Related

- `nelsong6/auth` — JWT issuer; CLI device flow contract this repo's auth depends on
- `nelsong6/romaine-auth-py` — canonical JWT verifier (this repo's `internal/auth` is the Go port)
- `nelsong6/glimmung` — design system reference (`design-system/colors_and_type.css`) + quality-timeframes / migration-policy / feature-contract patterns
- `nelsong6/tank-operator` — PodMonitor + observability pattern
- `nelsong6/infra-bootstrap` — the cluster + per-app Azure identity provisioning
- `nelsong6/play_show` — the deprecated PowerShell predecessor this repo replaces
