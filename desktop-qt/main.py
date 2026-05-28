"""shows-desktop (PySide6) entry point. Cached auth -> API client ->
round-robin runner (background thread) driving the mpv QML item, in a
single composited window (mpv video bottom, transparent web chrome top).

A localhost control server (shows.webserver) serves the overlay and a
/status + /pause + /skip surface; the overlay polls it same-origin. This
replaces QWebChannel, which doesn't wire cleanly into a QML WebEngineView
under PySide6 (registerObject isn't QML-callable; a QWebChannel can't be
assigned to the QQmlWebChannel-typed property).

The overlay is still minimal HTML; swapping in the built React frontend
is the remaining step (it talks to the same HTTP surface)."""

import logging
import os
import sys
import threading

logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")

def _add_libmpv_to_path() -> None:
    """Make libmpv discoverable by python-mpv before `import mpv`. Checks,
    in order: $SHOWS_LIBMPV_DIR, the Go build's bundled DLL (sibling
    desktop/build/bin), and the scoop mpv install. Falls back to the
    existing PATH if none match (lets a system-installed libmpv work)."""
    here = os.path.dirname(os.path.abspath(__file__))
    candidates = [
        os.environ.get("SHOWS_LIBMPV_DIR", ""),
        os.path.normpath(os.path.join(here, "..", "desktop", "build", "bin")),
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

from PySide6.QtCore import Qt, QUrl
from PySide6.QtGui import QGuiApplication, QDesktopServices
from PySide6.QtQml import QQmlApplicationEngine, qmlRegisterType
from PySide6.QtQuick import QQuickWindow, QSGRendererInterface
from PySide6.QtWebEngineQuick import QtWebEngineQuick

QQuickWindow.setGraphicsApi(QSGRendererInterface.GraphicsApi.OpenGL)
QtWebEngineQuick.initialize()
QGuiApplication.setAttribute(Qt.ApplicationAttribute.AA_ShareOpenGLContexts)

from shows import oauth  # noqa: E402
from shows.apiclient import Client  # noqa: E402
from shows.mpv_item import MpvItem  # noqa: E402
from shows.player import Player  # noqa: E402
from shows.runner import Runner  # noqa: E402
from shows.webserver import ControlServer, OVERLAY_HTML  # noqa: E402

PLAYLIST = "nelson"

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
        // loadHtml (not url:) so the overlay composites fully over the live
        // mpv GL layer — a url:-loaded page only composites its top band
        // over actively-rendering video. baseUrl is the control server so
        // the page's fetch('/status') etc. are same-origin.
        Component.onCompleted: loadHtml(overlayHtml, overlayBase)
    }
}
"""


def _opener(url: str) -> None:
    QDesktopServices.openUrl(QUrl(url))


def main() -> int:
    app = QGuiApplication(sys.argv)

    tok = oauth.ensure_token(opener=_opener)
    client = Client(tok.token, refresh_token=lambda: oauth.ensure_token(opener=_opener).token)

    server = ControlServer()
    port = server.start()
    overlay_url = f"http://127.0.0.1:{port}/"
    logging.info("control server on %s", overlay_url)

    qmlRegisterType(MpvItem, "shows", 1, 0, "MpvItem")
    engine = QQmlApplicationEngine()
    engine.rootContext().setContextProperty("overlayHtml", OVERLAY_HTML)
    engine.rootContext().setContextProperty("overlayBase", QUrl(overlay_url))
    engine.loadData(QML.encode("utf-8"))
    if not engine.rootObjects():
        print("QML failed to load", file=sys.stderr)
        return 1

    root = engine.rootObjects()[0]
    mpv_item = root.findChild(MpvItem, "mpvItem")
    if mpv_item is None:
        print("MpvItem not found", file=sys.stderr)
        return 1

    stop = threading.Event()
    started = {"v": False}

    def start_runner():
        if started["v"]:
            return
        started["v"] = True
        player = Player(mpv_item.mpv)
        server.set_player(player)
        runner = Runner(
            client, player, PLAYLIST, stop,
            on_round=lambda r: server.push(phase="playing", message=f"round of {len(r)}", round=r),
            on_advance=lambda res: server.push(last_advance=res),
            on_drained=lambda: server.push(phase="drained", message="every show finished", round=[]),
            on_error=lambda e: server.push(phase="error", message=e),
        )
        threading.Thread(target=runner.run, name="runner", daemon=True).start()

    mpv_item.renderReady.connect(start_runner, Qt.ConnectionType.QueuedConnection)
    app.aboutToQuit.connect(stop.set)
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
