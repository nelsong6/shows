# -*- mode: python ; coding: utf-8 -*-
# PyInstaller onedir build of the PySide6 client. Bundles libmpv-2.dll at
# the app root (where main._add_libmpv_to_path looks first when frozen) and
# the built React overlay under `frontend_dist` (where main resolves
# DIST_DIR when frozen). PySide6's PyInstaller hooks pull in Qt, QML, and
# the WebEngine runtime (QtWebEngineProcess + resources) automatically.
import os

HERE = os.path.dirname(os.path.abspath(SPEC))


def _find_libmpv():
    # $SHOWS_LIBMPV_DIR, a CI-populated third_party dir, then the mpv.net
    # install used for local dev.
    cands = [
        os.environ.get("SHOWS_LIBMPV_DIR", ""),
        os.path.join(HERE, "third_party", "libmpv"),
        os.path.expandvars(r"%LOCALAPPDATA%\Programs\mpv.net"),
    ]
    for d in cands:
        if d and os.path.exists(os.path.join(d, "libmpv-2.dll")):
            return os.path.join(d, "libmpv-2.dll")
    raise SystemExit("shows-qt.spec: libmpv-2.dll not found; set SHOWS_LIBMPV_DIR")


LIBMPV = _find_libmpv()
DIST = os.path.join(HERE, "frontend", "dist")
if not os.path.isdir(DIST):
    raise SystemExit("shows-qt.spec: frontend/dist missing; run `npm run build` in frontend/")

a = Analysis(
    ["main.py"],
    pathex=[HERE],
    binaries=[(LIBMPV, ".")],
    datas=[(DIST, "frontend_dist")],
    hiddenimports=["mpv"],
)
pyz = PYZ(a.pure)
exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name="shows-qt",
    console=True,  # keep the log console for now; flip to False for release
    icon=None,
)
coll = COLLECT(exe, a.binaries, a.datas, name="shows-qt")
