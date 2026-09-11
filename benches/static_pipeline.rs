use hamster_wheel::{config, profiling};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let results = profiling::profile_static_pipeline(config::profiling_document_root()).await?;

    println!("Static pipeline micro-profile (release bench profile)");
    for result in results {
        let samples = result
            .samples_ns_per_operation
            .iter()
            .map(|sample| format!("{sample:.1}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{} | iterations={} | samples_ns=[{}] | median_ns={:.1}",
            result.name, result.iterations, samples, result.median_ns_per_operation
        );
    }

    Ok(())
}
