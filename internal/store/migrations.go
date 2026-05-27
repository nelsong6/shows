package store

import (
	"context"
	"embed"
	"fmt"

	"github.com/jackc/pgx/v5/pgxpool"
	"github.com/jackc/pgx/v5/stdlib"
	"github.com/pressly/goose/v3"
)

//go:embed migrations/*.sql
var migrationsFS embed.FS

// Migrate runs every pending migration. Idempotent — `goose up` is a
// no-op when the DB is current. Called from shows-api at startup so a
// freshly-provisioned CNPG cluster is ready to serve requests on its
// first roll.
func Migrate(ctx context.Context, pool *pgxpool.Pool) error {
	// goose wants a *sql.DB. stdlib.OpenDBFromPool gives us one backed by
	// the existing pgxpool — no second connection pool, no second auth
	// round-trip.
	db := stdlib.OpenDBFromPool(pool)
	defer db.Close()

	if err := goose.SetDialect("postgres"); err != nil {
		return fmt.Errorf("goose dialect: %w", err)
	}
	goose.SetBaseFS(migrationsFS)
	if err := goose.UpContext(ctx, db, "migrations"); err != nil {
		return fmt.Errorf("goose up: %w", err)
	}
	return nil
}
