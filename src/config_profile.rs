use std::{hint::black_box, path::Path, time::Instant};

use crate::config::{ConfigError, compile_validated, load_file, parse_raw, validate_raw};

const TRIALS: usize = 3;

#[derive(Debug)]
pub struct StartupProfileResult {
    pub name: &'static str,
    pub hosts: usize,
    pub locations: usize,
    pub parse_us: Vec<f64>,
    pub validation_us: Vec<f64>,
    pub router_compile_us: Vec<f64>,
    pub total_us: Vec<f64>,
}

impl StartupProfileResult {
    pub fn parse_median_us(&self) -> f64 {
        median(&self.parse_us)
    }

    pub fn validation_median_us(&self) -> f64 {
        median(&self.validation_us)
    }

    pub fn router_compile_median_us(&self) -> f64 {
        median(&self.router_compile_us)
    }

    pub fn total_median_us(&self) -> f64 {
        median(&self.total_us)
    }

    pub fn total_range_percent(&self) -> f64 {
        range_percent(&self.total_us)
    }
}

pub fn profile_startup(
    name: &'static str,
    hosts: usize,
    locations: usize,
    source: &str,
    base_directory: &Path,
    config_path: &Path,
) -> Result<StartupProfileResult, ConfigError> {
    let mut parse_us = Vec::with_capacity(TRIALS);
    let mut validation_us = Vec::with_capacity(TRIALS);
    let mut router_compile_us = Vec::with_capacity(TRIALS);
    let mut total_us = Vec::with_capacity(TRIALS);

    for _ in 0..TRIALS {
        let started = Instant::now();
        black_box(parse_raw(black_box(source), "<profile>")?);
        parse_us.push(elapsed_us(started));

        let raw = parse_raw(source, "<profile>")?;
        let started = Instant::now();
        black_box(validate_raw(raw, black_box(base_directory))?);
        validation_us.push(elapsed_us(started));

        let raw = parse_raw(source, "<profile>")?;
        let validated = validate_raw(raw, base_directory)?;
        let started = Instant::now();
        black_box(compile_validated(validated)?);
        router_compile_us.push(elapsed_us(started));

        let started = Instant::now();
        black_box(load_file(black_box(config_path))?);
        total_us.push(elapsed_us(started));
    }

    parse_us.sort_by(f64::total_cmp);
    validation_us.sort_by(f64::total_cmp);
    router_compile_us.sort_by(f64::total_cmp);
    total_us.sort_by(f64::total_cmp);
    Ok(StartupProfileResult {
        name,
        hosts,
        locations,
        parse_us,
        validation_us,
        router_compile_us,
        total_us,
    })
}

fn elapsed_us(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000_000.0
}

fn median(samples: &[f64]) -> f64 {
    samples[TRIALS / 2]
}

fn range_percent(samples: &[f64]) -> f64 {
    (samples[TRIALS - 1] - samples[0]) * 100.0 / median(samples)
}
