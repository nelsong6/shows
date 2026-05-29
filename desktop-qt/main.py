"""shows-desktop (PySide6) entry point. Cached auth -> API client ->
round-robin runner (background thread) driving the mpv QML item, in a
single composited window (mpv video bottom, transparent web chrome top).

A localhost control server (shows.webserver) serves the overlay and a
/status + /shows + /pause + /skip + /defer surface; the overlay polls it
same-origin. This replaces QWebChannel, which doesn't wire cleanly into a QML
WebEngineView under PySide6 (registerObject isn't QML-callable; a QWebChannel
can't be assigned to the QQmlWebChannel-typed property).

The overlay is the built React frontend (`frontend/`, `npm run build`) served
from the control server; it is required (no placeholder fallback)."""

import logging
import os
import sys
import threading

logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")


def _resource_root() -> str:
    """Root for bundled resources (libmpv, the React dist). When frozen by
    PyInstaller that's the unpack dir (sys._MEIPASS); in source it's this
    file's directory."""
    if getattr(sys, "frozen", False):
        return sys._MEIPASS  # type: ignore[attr-defined]
    return os.path.dirname(os.path.abspath(__file__))


def _add_libmpv_to_path() -> None:
    """Make libmpv discoverable by python-mpv before `import mpv`. Checks,
    in order: the frozen bundle root (PyInstaller ships libmpv-2.dll there),
    $SHOWS_LIBMPV_DIR, and the scoop mpv install. Falls back to the existing
    PATH if none match (lets a system-installed libmpv work)."""
    candidates = [
        _resource_root(),
        os.environ.get("SHOWS_LIBMPV_DIR", ""),
        os.path.expandvars(r"%USERPROFILE%\scoop\apps\mpv\current"),
    ]
    for d in candidates:
        if not d:
            continue
        if os.path.exists(os.path.join(d, "libmpv-2.dll")) or os.path.exists(os.path.join(d, "mpv-2.dll")):
            os.add_dll_directory(d)
            os.environ["PATH"] = d + os.pathsep + os.environ.get("PATH", "")
            return


_add_libmpv_to_path()

# Disable QtWebEngine's GPU compositing. On high-DPI Windows (seen on a 4K /
# 300%-scale RTX display), WebEngine's DirectComposition path fails to get a
# D3D11 device and composites the transparent overlay into only the top-left
# 1/devicePixelRatio of the window — so over live mpv video the control bar's
# right half (skip/defer/show-list + key hints) vanished. Software compositing
# routes around it; the overlay is lightweight UI, and mpv keeps its own GL, so
# there's no cost. Must be set before QtWebEngine initializes.
_cef = os.environ.get("QTWEBENGINE_CHROMIUM_FLAGS", "")
if "--disable-gpu" not in _cef:
    os.environ["QTWEBENGINE_CHROMIUM_FLAGS"] = (_cef + " --disable-gpu").strip()

from PySide6.QtCore import QObject, Qt, QUrl, Signal
from PySide6.QtGui import QDesktopServices, QGuiApplication, QWindow
from PySide6.QtNetwork import QLocalServer, QLocalSocket
from PySide6.QtQml import QQmlApplicationEngine, qmlRegisterType
from PySide6.QtQuick import QQuickWindow, QSGRendererInterface
from PySide6.QtWebEngineQuick import QtWebEngineQuick

QQuickWindow.setGraphicsApi(QSGRendererInterface.GraphicsApi.OpenGL)
QtWebEngineQuick.initialize()
QGuiApplication.setAttribute(Qt.ApplicationAttribute.AA_ShareOpenGLContexts)

from shows import oauth, update  # noqa: E402
from shows.apiclient import Client  # noqa: E402
from shows.mpv_item import MpvItem  # noqa: E402
from shows.player import Player  # noqa: E402
from shows.replica import Replica  # noqa: E402
from shows.roundlogic import parse_playlists  # noqa: E402
from shows.runner import Runner  # noqa: E402
from shows.sync import Syncer  # noqa: E402
from shows.webserver import ControlServer  # noqa: E402


def _replica_path() -> str:
    base = os.environ.get("APPDATA") or os.path.expanduser("~")
    d = os.path.join(base, "shows")
    os.makedirs(d, exist_ok=True)
    return os.path.join(d, "replica.db")

# Playlists to round-robin over. One by default (the primary single-playlist
# path); set SHOWS_PLAYLISTS=a,b,c to interleave several via cross-playlist
# rounds (contract X1/X2).
PLAYLISTS = parse_playlists(os.environ.get("SHOWS_PLAYLISTS", ""), ["nelson"])

# Built React overlay (`frontend/`, `npm run build`), served by the control
# server. Frozen builds bundle the dist under `frontend_dist` in the resource
# root; in source it's the sibling frontend/dist tree. Required — there's no
# placeholder fallback.
_HERE = os.path.dirname(os.path.abspath(__file__))
if getattr(sys, "frozen", False):
    DIST_DIR = os.path.join(_resource_root(), "frontend_dist")
else:
    DIST_DIR = os.path.join(_HERE, "frontend", "dist")

QML = """
import QtQuick
import QtWebEngine
import shows 1.0

Window {
    visible: true
    width: 1280; height: 800
    title: "shows"
    color: "black"

    MpvItem { id: mpv; objectName: "mpvItem"; anchors.fill: parent }

    WebEngineView {
        anchors.fill: parent
        backgroundColor: "transparent"
        // focus so the overlay's window keydown handler (pause/skip/toggle)
        // receives keys rather than them going to the (focusless) mpv item.
        focus: true
        // loadHtml (not url:) so the overlay composites fully over the live
        // mpv GL layer — a url:-loaded page only composites its top band
        // over actively-rendering video. baseUrl is the control server so
        // the page's fetch('/status') etc. are same-origin.
        Component.onCompleted: { loadHtml(overlayHtml, overlayBase); forceActiveFocus(); }
    }
}
"""


def _opener(url: str) -> None:
    QDesktopServices.openUrl(QUrl(url))


# Single-instance key, scoped per-user (the replica lives under each user's
# %APPDATA%, so different users are genuinely separate instances).
_SINGLE_INSTANCE_KEY = "shows-desktop-" + (os.environ.get("USERNAME") or os.environ.get("USER") or "user")


def _raise_window(root) -> None:
    """Bring the primary instance's window to the foreground — called when a
    second launch pings us. No-op until the window exists."""
    if root is None:
        return
    if root.visibility() == QWindow.Visibility.Minimized:
        root.showNormal()
    root.raise_()
    root.requestActivate()


def _signal_running_instance() -> bool:
    """If another instance already holds the single-instance socket, ping it to
    surface its window and return True (caller should exit). False when we're the
    only instance."""
    sock = QLocalSocket()
    sock.connectToServer(_SINGLE_INSTANCE_KEY)
    if not sock.waitForConnected(300):
        return False
    sock.write(b"raise")
    sock.waitForBytesWritten(500)
    sock.disconnectFromServer()
    return True


def _serve_single_instance(on_ping) -> QLocalServer:
    """Listen on the single-instance socket so a later launch can find us. A
    crashed instance leaves a stale socket, which removeServer clears before we
    listen. Keep the returned server referenced for the app's lifetime."""
    QLocalServer.removeServer(_SINGLE_INSTANCE_KEY)
    srv = QLocalServer()
    srv.listen(_SINGLE_INSTANCE_KEY)

    def _on_new_connection():
        conn = srv.nextPendingConnection()
        if conn is not None:
            conn.disconnectFromServer()
        on_ping()

    srv.newConnection.connect(_on_new_connection)
    return srv


def main() -> int:
    app = QGuiApplication(sys.argv)

    # Single-instance guard: only one copy may drive the local replica + sync at
    # a time — two would race on the SQLite file and the server account and
    # silently drop writes (e.g. a watch you just recorded). If another instance
    # is already running, ask it to come forward and exit; otherwise become the
    # instance that later launches will find.
    if _signal_running_instance():
        logging.info("shows is already running — raised the existing window; exiting")
        return 0
    _win: dict = {}
    _instance_server = _serve_single_instance(lambda: _raise_window(_win.get("root")))  # noqa: F841 — kept alive

    if not os.path.isdir(DIST_DIR):
        print(f"overlay bundle missing at {DIST_DIR}; build it: "
              "cd frontend && npm run build", file=sys.stderr)
        return 1

    tok = oauth.ensure_token(opener=_opener)
    client = Client(tok.token, refresh_token=lambda: oauth.ensure_token(opener=_opener).token)

    # Offline-first: the local replica is the working copy the runner plays from;
    # the Syncer reconciles it with the server (seed on runner start, push at
    # smart moments). The overlay reads shows/history from the replica so the
    # dashboard works offline too.
    replica = Replica(_replica_path())
    syncer = Syncer(replica, client, PLAYLISTS)

    server = ControlServer(
        dist_dir=DIST_DIR,
        shows_provider=lambda: replica.overlay_shows(PLAYLISTS),
        history_provider=replica.show_history,
        stats_provider=lambda: replica.stats(PLAYLISTS),
    )
    server.set_library(replica)  # backs the /library/* management endpoints
    port = server.start()
    server.push(playlist=", ".join(PLAYLISTS))
    overlay_url = f"http://127.0.0.1:{port}/"
    logging.info("control server on %s", overlay_url)

    # One-shot, best-effort "is a newer build out?" check — pushes an `update`
    # banner into /status if so. Off-thread so it never delays startup; a dev
    # run (no embedded SHA) or being offline simply yields no banner.
    def _check_update():
        info = update.check()
        if info:
            logging.info("update available: %s (running %s)", info["latest"], info["current"])
            server.push(update={"available": True, **info})
    threading.Thread(target=_check_update, name="update-check", daemon=True).start()

    qmlRegisterType(MpvItem, "shows", 1, 0, "MpvItem")
    engine = QQmlApplicationEngine()
    engine.rootContext().setContextProperty("overlayHtml", server.index_html().decode("utf-8"))
    engine.rootContext().setContextProperty("overlayBase", QUrl(overlay_url))
    engine.loadData(QML.encode("utf-8"))
    if not engine.rootObjects():
        print("QML failed to load", file=sys.stderr)
        return 1

    root = engine.rootObjects()[0]
    _win["root"] = root  # single-instance: a later launch raises this window
    mpv_item = root.findChild(MpvItem, "mpvItem")
    if mpv_item is None:
        print("MpvItem not found", file=sys.stderr)
        return 1

    # Fullscreen toggle. The overlay POSTs /fullscreen on the control-server
    # thread; a queued signal marshals the window flip onto the Qt thread.
    class _UiBridge(QObject):
        toggle_fullscreen = Signal()

    ui = _UiBridge()

    def _toggle_fullscreen():
        if root.visibility() == QWindow.Visibility.FullScreen:
            root.showNormal()
        else:
            root.showFullScreen()

    ui.toggle_fullscreen.connect(_toggle_fullscreen, Qt.ConnectionType.QueuedConnection)

    stop = threading.Event()
    started = {"v": False}

    def start_runner():
        if started["v"]:
            return
        started["v"] = True
        player = Player(mpv_item.mpv)
        server.set_player(player)
        server.set_syncer(syncer)
        runner = Runner(
            replica, syncer, player, PLAYLISTS, stop,
            on_round=lambda r: server.push(phase="playing", message=f"round of {len(r)}", round=r, round_pos=0),
            on_advance=lambda res: server.push(last_advance=res),
            on_drained=lambda: server.push(phase="drained", message="every show finished", round=[]),
            on_error=lambda e: server.push(phase="error", message=e),
        )

        # playlist-pos fans out to both the overlay status and the runner (so
        # skip/defer act on the entry mpv is actually playing).
        def on_pos(i):
            server.push(round_pos=i)
            runner.set_pos(i)

        player.set_on_pos(on_pos)
        player.set_on_file_loaded(runner.on_file_loaded)  # restore resume on load
        player.set_on_natural_end(runner.on_natural_end)  # per-episode advance on EOF
        server.set_command_handlers(skip=runner.skip, defer=runner.defer, fullscreen=ui.toggle_fullscreen.emit)
        started["runner"] = runner
        threading.Thread(target=runner.run, name="runner", daemon=True).start()

        # Periodically persist the current position locally so resume survives a
        # crash, not just a clean close. Local SQLite write; pushed on sync/close.
        def _resume_saver():
            while not stop.wait(15.0):
                runner.save_resume()

        threading.Thread(target=_resume_saver, name="resume-saver", daemon=True).start()

    mpv_item.renderReady.connect(start_runner, Qt.ConnectionType.QueuedConnection)

    def _on_quit():
        runner = started.get("runner")
        if runner is not None:
            runner.save_resume()  # capture where we are before exit (resume point)
        syncer.push()  # flush queued local changes (incl. that resume) on the way out
        stop.set()

    app.aboutToQuit.connect(_on_quit)
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
