//! Criterion setup shared by the host benches (`#[path]`-included): the benchmark thread's CPU time instead of wall
//! time, so time the loaded host gives other processes is not counted, and flat sampling in short samples.

use std::time::Duration;

use criterion::measurement::{Measurement, ValueFormatter, WallTime};
use criterion::{BenchmarkGroup, Criterion, SamplingMode};

pub struct ThreadTime;

impl Measurement for ThreadTime {
    type Intermediate = cpu_time::ThreadTime;
    type Value = Duration;

    fn start(&self) -> cpu_time::ThreadTime {
        cpu_time::ThreadTime::now()
    }

    fn end(&self, start: cpu_time::ThreadTime) -> Duration {
        start.elapsed()
    }

    fn add(&self, a: &Duration, b: &Duration) -> Duration {
        *a + *b
    }

    fn zero(&self) -> Duration {
        Duration::ZERO
    }

    fn to_f64(&self, value: &Duration) -> f64 {
        value.as_nanos() as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        WallTime.formatter()
    }
}

pub fn config() -> Criterion<ThreadTime> {
    Criterion::default().with_measurement(ThreadTime)
}

pub fn group<'a>(c: &'a mut Criterion<ThreadTime>, name: &str) -> BenchmarkGroup<'a, ThreadTime> {
    let mut group = c.benchmark_group(name);
    group.sampling_mode(SamplingMode::Flat);
    group
}
