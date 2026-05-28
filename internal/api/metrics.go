package api

import (
	"net/http"
	"strconv"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

// Closed-set Prometheus instrumentation for shows-api. The `shows_*`
// namespace matches the fleet convention (auth_*, tank_*, glimmung_*);
// the kube-prometheus-stack in the monitoring namespace scrapes them
// via k8s/templates/podmonitor.yaml.
//
// Naming follows the prometheus best-practice "unit suffix on values":
//   _seconds (histograms with time)
//   _total   (monotonic counters)
//   _count   would be reserved for current-state gauges (not used here yet).

var (
	requestDuration = promauto.NewHistogramVec(prometheus.HistogramOpts{
		Name:    "shows_request_duration_seconds",
		Help:    "HTTP request duration in seconds, partitioned by method + path + status.",
		Buckets: []float64{0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10},
	}, []string{"method", "path", "status"})

	syncedRecordsTotal = promauto.NewCounter(prometheus.CounterOpts{
		Name: "shows_synced_records_total",
		Help: "Cumulative records (shows+episodes+history) accepted via /sync.",
	})
)

// metricsMiddleware wraps every chi-routed request with a duration
// histogram. The path label uses chi's route pattern (e.g.
// `/api/playlists/{name}/next-round`) rather than the raw URL so the
// label cardinality stays bounded.
func metricsMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(rec, r)

		path := chi.RouteContext(r.Context()).RoutePattern()
		if path == "" {
			path = "unmatched"
		}
		requestDuration.
			WithLabelValues(r.Method, path, strconv.Itoa(rec.status)).
			Observe(time.Since(start).Seconds())
	})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
	wrote  bool
}

func (s *statusRecorder) WriteHeader(code int) {
	if s.wrote {
		return
	}
	s.status = code
	s.wrote = true
	s.ResponseWriter.WriteHeader(code)
}

func (s *statusRecorder) Write(b []byte) (int, error) {
	if !s.wrote {
		s.wrote = true
	}
	return s.ResponseWriter.Write(b)
}
