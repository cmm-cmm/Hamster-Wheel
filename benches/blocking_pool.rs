use hamster_wheel::{blocking_profile, config};

const CONCURRENCY_LEVELS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
const TRIALS: usize = 3;
const WARMUP_PIPELINES: u64 = 1_000;
const MEASURED_PIPELINES: u64 = 10_000;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    println!(
        "Blocking pool profile: warmup={} measured={} trials={}",
        WARMUP_PIPELINES, MEASURED_PIPELINES, TRIALS
    );

    for concurrency in CONCURRENCY_LEVELS {
        let _ = blocking_profile::profile_noop_blocking(concurrency, WARMUP_PIPELINES).await?;
        let _ = blocking_profile::profile_filesystem_pipeline(
            config::profiling_document_root(),
            concurrency,
            WARMUP_PIPELINES,
        )
        .await?;

        let mut noop_runs = Vec::with_capacity(TRIALS);
        let mut filesystem_runs = Vec::with_capacity(TRIALS);
        for run in 1..=TRIALS {
            let noop =
                blocking_profile::profile_noop_blocking(concurrency, MEASURED_PIPELINES).await?;
            let filesystem = blocking_profile::profile_filesystem_pipeline(
                config::profiling_document_root(),
                concurrency,
                MEASURED_PIPELINES,
            )
            .await?;
            println!(
                "FS_RUN concurrency={} run={} pipeline_p50_us={:.3} pipeline_p95_us={:.3} pipeline_p99_us={:.3}",
                concurrency,
                run,
                filesystem.pipeline_p50_us,
                filesystem.pipeline_p95_us,
                filesystem.pipeline_p99_us
            );
            noop_runs.push(noop);
            filesystem_runs.push(filesystem);
        }

        print_operation_median(concurrency, "CONTROL", &noop_runs);
        for stage_index in 0..filesystem_runs[0].stages.len() {
            let stage_runs = filesystem_runs
                .iter()
                .map(|run| run.stages[stage_index].clone())
                .collect::<Vec<_>>();
            print_operation_median(concurrency, "FILESYSTEM", &stage_runs);
        }
        println!(
            "FS_MEDIAN concurrency={} pipeline_p50_us={:.3} pipeline_p95_us={:.3} pipeline_p99_us={:.3}",
            concurrency,
            median(
                filesystem_runs
                    .iter()
                    .map(|run| run.pipeline_p50_us)
                    .collect()
            ),
            median(
                filesystem_runs
                    .iter()
                    .map(|run| run.pipeline_p95_us)
                    .collect()
            ),
            median(
                filesystem_runs
                    .iter()
                    .map(|run| run.pipeline_p99_us)
                    .collect()
            )
        );
    }

    Ok(())
}

fn print_operation_median(
    concurrency: usize,
    kind: &str,
    runs: &[blocking_profile::OperationSummary],
) {
    println!(
        "{}_MEDIAN concurrency={} stage={} queue_p50_us={:.3} queue_p95_us={:.3} queue_p99_us={:.3} execution_p50_us={:.3} execution_p95_us={:.3} execution_p99_us={:.3} completion_p50_us={:.3} completion_p95_us={:.3} completion_p99_us={:.3} total_p50_us={:.3} total_p95_us={:.3} total_p99_us={:.3}",
        kind,
        concurrency,
        runs[0].name,
        median(runs.iter().map(|run| run.queue_p50_us).collect()),
        median(runs.iter().map(|run| run.queue_p95_us).collect()),
        median(runs.iter().map(|run| run.queue_p99_us).collect()),
        median(runs.iter().map(|run| run.execution_p50_us).collect()),
        median(runs.iter().map(|run| run.execution_p95_us).collect()),
        median(runs.iter().map(|run| run.execution_p99_us).collect()),
        median(runs.iter().map(|run| run.completion_p50_us).collect()),
        median(runs.iter().map(|run| run.completion_p95_us).collect()),
        median(runs.iter().map(|run| run.completion_p99_us).collect()),
        median(runs.iter().map(|run| run.total_p50_us).collect()),
        median(runs.iter().map(|run| run.total_p95_us).collect()),
        median(runs.iter().map(|run| run.total_p99_us).collect()),
    );
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}
