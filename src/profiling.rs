use std::{hint::black_box, io, path::PathBuf, time::Instant};

use bytes::Bytes;

use crate::{response_body::ResponseBody, static_files};

const CPU_ITERATIONS: u64 = 500_000;
const FILESYSTEM_ITERATIONS: u64 = 10_000;
const TRIALS: usize = 3;
const PROFILE_URI: &str = "/index.html";

#[derive(Debug)]
pub struct StageResult {
    pub name: &'static str,
    pub iterations: u64,
    pub samples_ns_per_operation: Vec<f64>,
    pub median_ns_per_operation: f64,
}

pub async fn profile_static_pipeline(document_root: PathBuf) -> io::Result<Vec<StageResult>> {
    let canonical_root = tokio::fs::canonicalize(document_root).await?;
    let mut samples: Vec<(&'static str, u64, Vec<f64>)> = vec![
        (
            "A constant response",
            CPU_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
        (
            "B decode + normalize",
            CPU_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
        (
            "C B + root validation",
            CPU_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
        (
            "D C + first metadata",
            FILESYSTEM_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
        (
            "E D + target canonicalize",
            FILESYSTEM_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
        (
            "F full static HEAD, no body read",
            FILESYSTEM_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
        (
            "G full static GET",
            FILESYSTEM_ITERATIONS,
            Vec::with_capacity(TRIALS),
        ),
    ];

    for _ in 0..TRIALS {
        let started = Instant::now();
        for _ in 0..CPU_ITERATIONS {
            black_box(static_files::constant_response());
        }
        samples[0].2.push(ns_per_operation(started, CPU_ITERATIONS));

        let started = Instant::now();
        for _ in 0..CPU_ITERATIONS {
            black_box(
                static_files::normalize_path(black_box(PROFILE_URI))
                    .expect("the profiling URI is valid"),
            );
        }
        samples[1].2.push(ns_per_operation(started, CPU_ITERATIONS));

        let started = Instant::now();
        for _ in 0..CPU_ITERATIONS {
            let relative = static_files::normalize_path(black_box(PROFILE_URI))
                .expect("the profiling URI is valid");
            let candidate = canonical_root.join(relative);
            black_box(candidate.starts_with(&canonical_root));
            black_box(candidate);
        }
        samples[2].2.push(ns_per_operation(started, CPU_ITERATIONS));

        let started = Instant::now();
        for _ in 0..FILESYSTEM_ITERATIONS {
            let relative = static_files::normalize_path(black_box(PROFILE_URI))
                .expect("the profiling URI is valid");
            let candidate = canonical_root.join(relative);
            black_box(candidate.starts_with(&canonical_root));
            black_box(tokio::fs::metadata(candidate).await?);
        }
        samples[3]
            .2
            .push(ns_per_operation(started, FILESYSTEM_ITERATIONS));

        let started = Instant::now();
        for _ in 0..FILESYSTEM_ITERATIONS {
            let relative = static_files::normalize_path(black_box(PROFILE_URI))
                .expect("the profiling URI is valid");
            let candidate = canonical_root.join(relative);
            black_box(candidate.starts_with(&canonical_root));
            black_box(tokio::fs::metadata(&candidate).await?);
            let canonical = tokio::fs::canonicalize(candidate).await?;
            black_box(canonical.starts_with(&canonical_root));
            black_box(canonical);
        }
        samples[4]
            .2
            .push(ns_per_operation(started, FILESYSTEM_ITERATIONS));

        let started = Instant::now();
        for _ in 0..FILESYSTEM_ITERATIONS {
            let relative = static_files::normalize_path(black_box(PROFILE_URI))
                .expect("the profiling URI is valid");
            let candidate = canonical_root.join(relative);
            black_box(candidate.starts_with(&canonical_root));
            black_box(tokio::fs::metadata(&candidate).await?);
            let canonical = tokio::fs::canonicalize(candidate).await?;
            black_box(canonical.starts_with(&canonical_root));
            let metadata = tokio::fs::metadata(&canonical).await?;
            black_box(static_files::file_response(
                &canonical,
                ResponseBody::buffered(Bytes::new()),
                metadata.len(),
            ));
        }
        samples[5]
            .2
            .push(ns_per_operation(started, FILESYSTEM_ITERATIONS));

        let started = Instant::now();
        for _ in 0..FILESYSTEM_ITERATIONS {
            let relative = static_files::normalize_path(black_box(PROFILE_URI))
                .expect("the profiling URI is valid");
            let candidate = canonical_root.join(relative);
            black_box(candidate.starts_with(&canonical_root));
            black_box(tokio::fs::metadata(&candidate).await?);
            let canonical = tokio::fs::canonicalize(candidate).await?;
            black_box(canonical.starts_with(&canonical_root));
            let file = tokio::fs::File::open(&canonical).await?;
            let content_length = file.metadata().await?.len();
            black_box(static_files::file_response(
                &canonical,
                ResponseBody::file(file, content_length),
                content_length,
            ));
        }
        samples[6]
            .2
            .push(ns_per_operation(started, FILESYSTEM_ITERATIONS));
    }

    Ok(samples
        .into_iter()
        .map(|(name, iterations, samples_ns_per_operation)| {
            let median_ns_per_operation = median(&samples_ns_per_operation);
            StageResult {
                name,
                iterations,
                samples_ns_per_operation,
                median_ns_per_operation,
            }
        })
        .collect())
}

fn ns_per_operation(started: Instant, iterations: u64) -> f64 {
    started.elapsed().as_secs_f64() * 1_000_000_000.0 / iterations as f64
}

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}
