# Desktop Local Testing

The desktop app is built under `desktop-rs\target`, but the local test install
lives at `D:\Downloads\shows\shows-desktop.exe`. Rebuilding does not update the
test install path. Every release build that should be tested from the local
install location must be copied over.

1. Build the embedded React overlay:

```powershell
cd D:\repos\shows\desktop-rs\frontend
npm run build
```

2. Build the release desktop binary:

```powershell
cd D:\repos\shows\desktop-rs
cargo build --release -p shows-desktop
```

3. Close the running desktop app, then replace the local test binary:

```powershell
Copy-Item -Force `
  D:\repos\shows\desktop-rs\target\release\shows-desktop.exe `
  D:\Downloads\shows\shows-desktop.exe
```

The frontend assets are embedded into `shows-desktop.exe`; do not copy the
frontend directory for release testing.
