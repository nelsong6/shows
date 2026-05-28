"""mpv rendered into the QML scene graph via the render API — the
compositing approach proven in the spike. A QQuickFramebufferObject item
renders mpv into an FBO that QML composites; a transparent WebEngineView
layered on top in QML draws the chrome. Exposes `.mpv` for the runner."""

from __future__ import annotations

from PySide6.QtCore import Qt, Signal
from PySide6.QtGui import QOpenGLContext
from PySide6.QtQuick import QQuickFramebufferObject

import mpv


@mpv.MpvGlGetProcAddressFn
def _get_proc_address(_ctx, name):
    glctx = QOpenGLContext.currentContext()
    if glctx is None:
        return 0
    addr = glctx.getProcAddress(name)
    return int(addr) if addr else 0


class _MpvRenderer(QQuickFramebufferObject.Renderer):
    def __init__(self, item: "MpvItem"):
        super().__init__()
        self._item = item
        self._ctx = None

    def createFramebufferObject(self, size):
        if self._ctx is None:
            self._ctx = mpv.MpvRenderContext(
                self._item.mpv, "opengl",
                opengl_init_params={"get_proc_address": _get_proc_address},
            )
            self._ctx.update_cb = self._item.mpvUpdate.emit
            # Signal that the render context exists. Loading a file BEFORE
            # this point makes mpv come up with no video output (vid=no,
            # dwidth=None) — so the runner must wait for this.
            self._item.renderReady.emit()
        return super().createFramebufferObject(size)

    def render(self):
        w = self._item.window()
        fbo = self.framebufferObject()
        if w is not None:
            w.beginExternalCommands()
        # flip_y is False here: a QQuickFramebufferObject's FBO uses the
        # opposite vertical convention from a plain QOpenGLWidget's default
        # framebuffer, so flip_y=True renders the video upside down.
        self._ctx.render(
            flip_y=False,
            opengl_fbo={"fbo": int(fbo.handle()), "w": fbo.width(), "h": fbo.height()},
        )
        if w is not None:
            w.endExternalCommands()


class MpvItem(QQuickFramebufferObject):
    mpvUpdate = Signal()
    renderReady = Signal()  # emitted once when the render context is created

    def __init__(self, parent=None):
        super().__init__(parent)
        self.mpv = mpv.MPV(vo="libmpv", hwdec="no", keep_open="no", osc="yes")
        # mpv's new-frame callback fires off-thread; marshal to GUI thread.
        self.mpvUpdate.connect(self.update, Qt.ConnectionType.QueuedConnection)

    def createRenderer(self):
        return _MpvRenderer(self)
