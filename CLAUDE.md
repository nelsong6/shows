# shows

App repo on the romaine.life AKS cluster. Cosmos-backed playlist API at `shows.romaine.life` plus a Rust Windows desktop app that drives libmpv for round-robin playback.

## Quality timeframe

This repo follows the long-term, heavy-solution operating mode codified in `nelsong6/glimmung/docs/quality-timeframes.md`. Compatibility layers are prohibited per `nelsong6/tank-operator/docs/migration-policy.md` — `legacy`, `compatibility`, `fallback`, `temporary`, and `exception` are deletion targets, not design options.

When extending a feature documented at `docs/feature-contracts/`, name the affected contract in the PR and explain how the implementation proves the invariants still hold.

## Layout

Offline-first: the **desktop is the engine** (round selection / advance / defer /
resume run locally against a SQLite replica) and the **server is a durable
origin** it syncs with git-style (`GET /library` pull, `POST /sync` push,
last-write-wins). The server-side round engine + the legacy import tooling have
been removed; the round-and-advance contract now governs the desktop.

```
cmd/
  shows-api/          HTTP server, runs in AKS (dumb store: /library + /sync)
internal/
  auth/               JWKS verifier + chi middleware for auth.romaine.life JWTs
  store/              Cosmos SDK store layer (shows + watch_history; library/sync)
  api/                chi routes, handlers, Prometheus metrics
desktop-rs/           cargo workspace: the Rust client (mpv video composited
                      under a transparent React overlay in one DirectComposition
                      window) — the engine
  core/               shows-core: pure, #![forbid(unsafe_code)], all the engine
    src/
      ordering.rs     SHA-256 path ordering, bit-identical to the old Go server
      roundlogic.rs   pure helpers (advance-set minus deferred, playlist parse)
      scan.rs         scan a show's folder for episode files
      replica.rs      local SQLite working copy (seed, mutate, dirty-track, LWW)
      engine.rs       round/advance/defer engine over the replica
      runner.rs       round-robin runner (round → queue → wait → advance + push)
      apiclient.rs    shows.romaine.life client (get_library / post_sync + 401 refresh)
      oauth.rs        user-login flow against auth.romaine.life (PKCE + loopback)
      sync.rs         git-style sync: seed/push/connectivity (no polling)
      model.rs        ⟂ replica/apiclient row/wire types
      update.rs       latest-release check (GitHub Releases API)
  desktop/            shows-desktop: the Windows shell (the only unsafe code lives here)
    src/
      main.rs         entry: single-instance guard, runner + control server wiring
      compositor.rs   DirectComposition window + transparent WebView2 overlay
                      over a D3D11 composition swapchain; input forwarding
      gl.rs           WGL + WGL_NV_DX_interop2: mpv OpenGL render → shared D3D
                      texture (the production video path; no ANGLE DLLs)
      mpv.rs          libmpv FFI via libloading (vo=libmpv)
      player.rs       libmpv handle wrapper + PlayerOps impl + event pump
      webserver.rs    control server: serves the overlay + /status /pause /skip
                      /defer /seek /volume /sub /audio /sync-now /fullscreen
                      /library/* /stats
  frontend/           Vite + React + TS overlay (glimmung design-system tokens)
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

The desktop app (`desktop-rs/`) is **not** deployed to AKS. `.github/workflows/build-desktop.yaml` cargo-builds it on a Windows runner, embeds the React dist, bundles `libmpv-2.dll` next to the exe, and publishes it as a GitHub Release.

## Auth

Every `/api/*` route requires an auth.romaine.life JWT with `role in {admin, user}`. Tokens are verified against the JWKS at `https://auth.romaine.life/api/auth/jwks` with required claims `["exp", "iat", "iss", "role"]` (mirrors `nelsong6/romaine-auth-py`).

The desktop app uses the **user-login** path at `GET /api/auth/cli/user-login` (PKCE + loopback `redirect_uri`). If the user has no `.romaine.life` session cookie, auth.romaine.life bounces them through Microsoft/Google and returns; the server then redirects to the loopback with a one-time `?code=...`. The desktop POSTs `{grant_type: authorization_code, code, code_verifier, redirect_uri}` to `/api/auth/cli/user-token` and receives the user's own JWT (`role=user|admin`, no `purpose` claim — same shape the browser session would yield). The JWT never travels through the browser. Token caches at `%APPDATA%\shows\token.json`; on 401 the apiclient calls back into the oauth module's `ensure_token` for an in-place refresh.

Library import is now in-app: the desktop scans a folder (`desktop-rs/core/src/scan.rs`) and creates the show locally, which syncs up via `/sync`. The old `cmd/shows-migrate` bulk-import tool and its bot-token CLI flow (`internal/device`) have been removed.

## Cosmos store

Two containers on `infra-cosmos-serverless` / `dbs/shows`:

- **`shows`** — one doc per show, partitioned by `/playlist`. Episodes are embedded as a nested array; each `/advance` is a single point-write.
- **`watch_history`** — append-only event log, partitioned by `/show_id`. Source of truth for the "took N days to watch" reveal.

The runtime pod attaches to Cosmos via workload identity: `serviceaccount/shows:shows` ↔ `shows-identity` UAMI (federated cred in `tofu/identity.tf`) ↔ Cosmos data-plane `Built-in Data Contributor` role scoped to `dbs/shows` only.

## Ordering invariant

The deterministic-random round order is now computed **on the desktop** by `desktop-rs/core/src/ordering.rs` (the server no longer computes rounds):

```
hash := SHA-256(UTF-8 bytes of: root_path + "\" + relative_path)
order_value := uint32(first 4 hex chars of hash, parsed as base 16)
shows in round are sorted by order_value ascending
```

Bit-for-bit reproduces `Get-FileHash -InputStream` + `SubString(0,4)` + `[uint32]` from the legacy `play_ordered_show.ps1`. Locked by the in-file tests in `desktop-rs/core/src/engine.rs` (round selection/advance/defer) and `ordering.rs` (the SHA-256 path hashing). See [`docs/feature-contracts/round-and-advance.md`](docs/feature-contracts/round-and-advance.md) for the invariants the round + advance pair satisfies — the contract the desktop engine now implements.

## Migration source data

Legacy state at `D:\Downloads\Group-Nelson\nelson.json` + the per-show JSONs it points at:

```json
{
  "Name": "Dr. Katz",
  "Episodes": ["Dr. Katz S06\\Dr.Katz.S06E11.Big.TV.avi", ...],
  "DateAdded": "1/29/2024 8:34:00 AM"
}
```

Episode paths were relative to the parent directory of the per-show JSON, joined with that parent to produce the absolute path stored as `episodes.relative_path` and the show's `root_path`. This one-time legacy import is done (the `cmd/shows-migrate` tool has been removed); new shows are now added in-app by scanning a folder (`desktop-rs/core/src/scan.rs`), which derives `relative_path` the same way (backslash-joined, to match the ordering hash).

## Observability

**Server side:** `shows_*` Prometheus metrics exposed at `/metrics` (no auth) on the AKS pod, scraped by the kube-prometheus-stack via `k8s/templates/podmonitor.yaml`. Catalog in [`docs/feature-contracts/round-and-advance.md`](docs/feature-contracts/round-and-advance.md). Grafana sees them in the `monitoring` namespace's dashboards.

**Desktop side:** the running app's control server (localhost, ephemeral port logged at startup) serves `GET /status` (current status) + `GET /shows` + `GET /health`, plus the overlay's `POST /pause` `/skip` `/defer` `/fullscreen` controls, and also backs the React overlay's data layer. Dev runs log to stderr; release builds (no console subsystem) append to `%APPDATA%\shows\shows.log`. The control surface lives in `desktop-rs/desktop/src/webserver.rs`.

## Build & Local Testing

### 1. Build the React Frontend
Navigate to the frontend directory and compile the React production bundle:
```bash
cd desktop-rs/frontend
npm run build
```

### 2. Build the Desktop App
Navigate to the cargo workspace root and compile the release binary:
```bash
cd desktop-rs
cargo build --release -p shows-desktop
```

### 3. Deploy for Local Testing
To test desktop changes locally:
1. Close the running app (it is safe to close anytime; watched progress is saved per-episode and resumes cleanly on restart).
2. Copy the newly compiled binary from:
   `desktop-rs/target/release/shows-desktop.exe`
   To the testing location:
   `D:\Downloads\shows\shows-desktop.exe` to swap the code over.
3. *Note: Since the frontend assets are now embedded directly in the binary, you no longer need to copy the `frontend/` folder to the target directory for release testing.*

## Related

- `nelsong6/auth` — JWT issuer; CLI device flow contract this repo's auth depends on
- `nelsong6/romaine-auth-py` — canonical JWT verifier (this repo's `internal/auth` is the Go port)
- `nelsong6/glimmung` — design system reference (`design-system/colors_and_type.css`) + quality-timeframes / migration-policy / feature-contract patterns
- `nelsong6/tank-operator` — PodMonitor + observability pattern
- `nelsong6/infra-bootstrap` — the cluster + per-app Azure identity provisioning
- `nelsong6/play_show` — the deprecated PowerShell predecessor this repo replaces
