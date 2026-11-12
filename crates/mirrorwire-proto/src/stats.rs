use std::fmt::Write;

/// Collects samples for one reporting window and summarizes them.
#[derive(Debug, Default, Clone)]
pub struct Series {
    samples: Vec<f64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

impl Series {
    pub fn push(&mut self, v: f64) {
        self.samples.push(v);
    }

    pub fn summary(&self) -> Summary {
        if self.samples.is_empty() {
            return Summary::default();
        }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        let pick = |q: f64| s[((s.len() - 1) as f64 * q).round() as usize];
        Summary {
            count: s.len(),
            mean: s.iter().sum::<f64>() / s.len() as f64,
            p50: pick(0.5),
            p95: pick(0.95),
            max: *s.last().unwrap(),
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// A named set of latency stages, in milliseconds.
#[derive(Debug, Default, Clone)]
pub struct StageStats {
    stages: Vec<(&'static str, Series)>,
}

impl StageStats {
    pub fn new(names: &[&'static str]) -> Self {
        Self {
            stages: names.iter().map(|n| (*n, Series::default())).collect(),
        }
    }

    pub fn record(&mut self, name: &str, ms: f64) {
        if let Some((_, s)) = self.stages.iter_mut().find(|(n, _)| *n == name) {
            s.push(ms);
        }
    }

    pub fn summaries(&self) -> Vec<(&'static str, Summary)> {
        self.stages.iter().map(|(n, s)| (*n, s.summary())).collect()
    }

    pub fn report(&self) -> String {
        let mut out = String::new();
        for (name, s) in self.summaries() {
            if s.count > 0 {
                let _ = write!(out, " {name}={:.2}/{:.2}/{:.2}", s.mean, s.p95, s.max);
            }
        }
        out
    }

    pub fn clear(&mut self) {
        for (_, s) in &mut self.stages {
            s.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut s = Series::default();
        for i in 1..=100 {
            s.push(i as f64);
        }
        let x = s.summary();
        assert_eq!(x.count, 100);
        assert_eq!(x.max, 100.0);
        assert!((x.mean - 50.5).abs() < 1e-9);
        assert!(x.p95 >= 95.0 && x.p95 <= 96.0);
    }
}
