FROM golang:1.25-alpine AS builder

WORKDIR /src

COPY go.mod go.sum ./
RUN go mod download

COPY . .

# Static build so the final image can be FROM scratch.
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -ldflags="-s -w" -o /out/shows-api ./cmd/shows-api

FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=builder /out/shows-api /shows-api

# Migrations are embedded into the binary via embed.FS — no need to copy them
# into the image. See internal/store/migrations.go.

EXPOSE 8080
USER nonroot:nonroot
ENTRYPOINT ["/shows-api"]
