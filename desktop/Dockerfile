# Cross-compiles shows-desktop for Windows from Linux. Output is a scratch
# image whose root contains shows.exe + libmpv-2.dll alongside each other —
# the same layout desktop/scripts/build.ps1 produces locally on Windows.
#
# Pushed to ACR with a fingerprint tag by .github/workflows/desktop-build-
# check.yaml. To install:
#
#   docker pull romainecr.azurecr.io/shows-desktop:desktop-<fingerprint>
#   docker create --name x romainecr.azurecr.io/shows-desktop:desktop-<fingerprint>
#   docker cp x:/shows.exe . && docker cp x:/libmpv-2.dll .
#   docker rm x
#
# Local Windows dev still uses desktop/scripts/build.ps1 directly; this
# Dockerfile is CI-only.

FROM golang:1.25-bookworm AS builder

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      mingw-w64 \
      p7zip-full \
      curl \
      ca-certificates \
      nodejs \
      npm \
 && rm -rf /var/lib/apt/lists/*

# Wails CLI generates the TS bindings + drives `go build` with the right
# embed/cgo flags. -skipbindings keeps it from trying to run the Windows
# binary on Linux to regenerate them; bindings under desktop/frontend/
# wailsjs/ are committed.
RUN go install github.com/wailsapp/wails/v2/cmd/wails@latest

WORKDIR /src

# libmpv runtime + headers. Mirrors desktop/scripts/setup-libmpv.ps1's
# version pin — bump together. Pre-download before any source COPY so this
# layer caches even when desktop/ files change.
ARG LIBMPV_VERSION=20250420
ARG LIBMPV_HASH=3600c71
RUN curl -fSL -o /tmp/libmpv.7z \
      "https://master.dl.sourceforge.net/project/mpv-player-windows/libmpv/mpv-dev-x86_64-${LIBMPV_VERSION}-git-${LIBMPV_HASH}.7z?viasf=1" \
 && mkdir -p /opt/libmpv \
 && 7z x /tmp/libmpv.7z -o/opt/libmpv -y >/dev/null \
 && rm /tmp/libmpv.7z \
 && test -f /opt/libmpv/libmpv-2.dll

# Frontend deps go before the source COPY so npm install caches across
# source-only changes (Vite source edits don't invalidate node_modules).
COPY desktop/frontend/package.json desktop/frontend/package-lock.json desktop/frontend/
RUN cd desktop/frontend && npm ci

# Everything else.
COPY desktop/ desktop/

ENV CGO_ENABLED=1 \
    GOOS=windows \
    GOARCH=amd64 \
    CC=x86_64-w64-mingw32-gcc \
    CXX=x86_64-w64-mingw32-g++ \
    CGO_CFLAGS="-I/opt/libmpv/include" \
    CGO_LDFLAGS="-L/opt/libmpv -lmpv"

RUN cd desktop && /go/bin/wails build -platform windows/amd64 -skipbindings -clean

# Stage the outputs together so the scratch base just copies one directory.
RUN mkdir -p /out \
 && cp desktop/build/bin/shows.exe /out/shows.exe \
 && cp /opt/libmpv/libmpv-2.dll /out/libmpv-2.dll

# Minimal final layer. The image is content for `docker cp`-style extraction,
# not for `docker run` — the .exe is a Windows binary, not Linux.
FROM scratch
COPY --from=builder /out/ /
