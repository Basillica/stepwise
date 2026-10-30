//! Tiny Prometheus-text metrics. A single mutex is fine at mock-server scale; swap for
//! the `metrics` crate or atomics if this ever shows up in a profile.

use std::{collections::BTreeMap, fmt::Write, sync::Mutex, time::Instant};

const BUCKETS: [f64; 10] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0,
];

#[derive(Default)]
struct Hist {
    buckets: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct Inner {
    requests: BTreeMap<(String, String, u16), u64>,
    duration: BTreeMap<String, Hist>,
    rate_limited: BTreeMap<String, u64>,
    auth_failures: u64,
    effects: BTreeMap<&'static str, u64>,
    hook_calls: BTreeMap<(String, &'static str), u64>,
    hook_duration: BTreeMap<String, Hist>,
    config_applies: BTreeMap<&'static str, u64>,
}

pub struct Metrics {
    inner: Mutex<Inner>,
    started: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            started: Instant::now(),
        }
    }
}

impl Inner {
    fn duration_hist(&mut self, hook: &str) -> &mut Hist {
        self.hook_duration.entry(hook.to_string()).or_default()
    }
}

impl Metrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn observe_request(&self, route: &str, method: &str, status: u16, secs: f64) {
        let mut g = self.lock();
        *g.requests
            .entry((route.into(), method.into(), status))
            .or_default() += 1;
        let h = g.duration.entry(route.into()).or_default();
        h.sum += secs;
        h.count += 1;
        for (i, b) in BUCKETS.iter().enumerate() {
            if secs <= *b {
                h.buckets[i] += 1;
            }
        }
    }

    /// `result`: ok | respond | fail | timeout | trap | busy | contract
    pub fn observe_hook(&self, hook: &str, result: &'static str, secs: f64) {
        let mut g = self.lock();
        *g.hook_calls.entry((hook.into(), result)).or_default() += 1;
        let h = g.duration_hist(hook);
        h.sum += secs;
        h.count += 1;
        for (i, b) in BUCKETS.iter().enumerate() {
            if secs <= *b {
                h.buckets[i] += 1;
            }
        }
    }

    /// `kind`: in_tx_committed | in_tx_rolled_back | after_commit_ok | after_commit_failed
    pub fn add_effects(&self, kind: &'static str, n: u64) {
        *self.lock().effects.entry(kind).or_default() += n;
    }

    pub fn inc_rate_limited(&self, route: &str) {
        *self.lock().rate_limited.entry(route.into()).or_default() += 1;
    }

    pub fn inc_auth_failure(&self) {
        self.lock().auth_failures += 1;
    }

    pub fn inc_config_apply(&self, result: &'static str) {
        *self.lock().config_applies.entry(result).or_default() += 1;
    }

    pub fn render(&self, config_version: u64) -> String {
        let g = self.lock();
        let mut o = String::new();
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");

        let _ = writeln!(
            o,
            "# HELP stepwise_build_info Build information.\n# TYPE stepwise_build_info gauge"
        );
        let _ = writeln!(
            o,
            "stepwise_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        let _ = writeln!(
            o,
            "# HELP stepwise_uptime_seconds Process uptime.\n# TYPE stepwise_uptime_seconds gauge"
        );
        let _ = writeln!(
            o,
            "stepwise_uptime_seconds {}",
            self.started.elapsed().as_secs()
        );
        let _ = writeln!(o, "# HELP stepwise_config_version Version of the active configuration.\n# TYPE stepwise_config_version gauge");
        let _ = writeln!(o, "stepwise_config_version {config_version}");

        let _ = writeln!(o, "# HELP stepwise_requests_total Requests handled.\n# TYPE stepwise_requests_total counter");
        for ((route, method, status), n) in &g.requests {
            let _ = writeln!(
                o,
                "stepwise_requests_total{{route=\"{}\",method=\"{method}\",status=\"{status}\"}} {n}",
                esc(route)
            );
        }
        let _ = writeln!(o, "# HELP stepwise_request_duration_seconds Request latency.\n# TYPE stepwise_request_duration_seconds histogram");
        for (route, h) in &g.duration {
            let r = esc(route);
            for (i, b) in BUCKETS.iter().enumerate() {
                let _ = writeln!(
                    o,
                    "stepwise_request_duration_seconds_bucket{{route=\"{r}\",le=\"{b}\"}} {}",
                    h.buckets[i]
                );
            }
            let _ = writeln!(
                o,
                "stepwise_request_duration_seconds_bucket{{route=\"{r}\",le=\"+Inf\"}} {}",
                h.count
            );
            let _ = writeln!(
                o,
                "stepwise_request_duration_seconds_sum{{route=\"{r}\"}} {}",
                h.sum
            );
            let _ = writeln!(
                o,
                "stepwise_request_duration_seconds_count{{route=\"{r}\"}} {}",
                h.count
            );
        }
        let _ = writeln!(o, "# HELP stepwise_rate_limited_total Requests rejected by rate limiting.\n# TYPE stepwise_rate_limited_total counter");
        for (route, n) in &g.rate_limited {
            let _ = writeln!(
                o,
                "stepwise_rate_limited_total{{route=\"{}\"}} {n}",
                esc(route)
            );
        }
        let _ = writeln!(o, "# HELP stepwise_hook_calls_total WASM hook invocations by outcome.\n# TYPE stepwise_hook_calls_total counter");
        for ((hook, result), n) in &g.hook_calls {
            let _ = writeln!(
                o,
                "stepwise_hook_calls_total{{hook=\"{}\",result=\"{result}\"}} {n}",
                esc(hook)
            );
        }
        let _ = writeln!(o, "# HELP stepwise_hook_duration_seconds WASM hook latency.\n# TYPE stepwise_hook_duration_seconds histogram");
        for (hook, h) in &g.hook_duration {
            let r = esc(hook);
            for (i, b) in BUCKETS.iter().enumerate() {
                let _ = writeln!(
                    o,
                    "stepwise_hook_duration_seconds_bucket{{hook=\"{r}\",le=\"{b}\"}} {}",
                    h.buckets[i]
                );
            }
            let _ = writeln!(
                o,
                "stepwise_hook_duration_seconds_bucket{{hook=\"{r}\",le=\"+Inf\"}} {}",
                h.count
            );
            let _ = writeln!(
                o,
                "stepwise_hook_duration_seconds_sum{{hook=\"{r}\"}} {}",
                h.sum
            );
            let _ = writeln!(
                o,
                "stepwise_hook_duration_seconds_count{{hook=\"{r}\"}} {}",
                h.count
            );
        }
        let _ = writeln!(o, "# HELP stepwise_effects_total Script-queued writes by outcome.\n# TYPE stepwise_effects_total counter");
        for (kind, n) in &g.effects {
            let _ = writeln!(o, "stepwise_effects_total{{outcome=\"{kind}\"}} {n}");
        }
        let _ = writeln!(o, "# HELP stepwise_auth_failures_total Failed authentication attempts.\n# TYPE stepwise_auth_failures_total counter");
        let _ = writeln!(o, "stepwise_auth_failures_total {}", g.auth_failures);
        let _ = writeln!(o, "# HELP stepwise_config_applies_total Config change attempts.\n# TYPE stepwise_config_applies_total counter");
        for (res, n) in &g.config_applies {
            let _ = writeln!(o, "stepwise_config_applies_total{{result=\"{res}\"}} {n}");
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_and_histogram() {
        let m = Metrics::default();
        m.observe_request("GET /pets", "GET", 200, 0.003);
        m.observe_request("GET /pets", "GET", 200, 0.3);
        m.inc_rate_limited("GET /pets");
        let text = m.render(7);
        assert!(text.contains(
            "stepwise_requests_total{route=\"GET /pets\",method=\"GET\",status=\"200\"} 2"
        ));
        assert!(text.contains(
            "stepwise_request_duration_seconds_bucket{route=\"GET /pets\",le=\"0.005\"} 1"
        ));
        assert!(text.contains("stepwise_request_duration_seconds_count{route=\"GET /pets\"} 2"));
        assert!(text.contains("stepwise_rate_limited_total{route=\"GET /pets\"} 1"));
        assert!(text.contains("stepwise_config_version 7"));
    }
}
