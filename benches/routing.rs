use std::{path::PathBuf, sync::Arc};

use hamster_wheel::{router::HostRouter, routing_profile};

fn main() -> std::io::Result<()> {
    let router = HostRouter::default_static(Arc::new(PathBuf::from("public")))
        .expect("the profiling router should compile");
    for result in routing_profile::profile_routing(&router)? {
        println!(
            "ROUTING_PROFILE stage={} iterations={} samples_ns={:?} median_ns={:.3} samples_cpu_ns={:?} median_cpu_ns={:.3} range_pct={:.3}",
            result.name,
            result.iterations,
            result.samples_ns_per_operation,
            result.median_ns_per_operation,
            result.samples_cpu_ns_per_operation,
            result.median_cpu_ns_per_operation,
            result.range_percent,
        );
    }
    Ok(())
}
