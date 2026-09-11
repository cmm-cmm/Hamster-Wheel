use std::{
    fmt::Write,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use hamster_wheel::{
    config, config_profile::profile_startup, router::HostRouter,
    routing_profile::profile_runtime_config,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let working_directory = std::env::current_dir()?;
    let profile_directory = ProfileDirectory::new()?;
    std::fs::create_dir(profile_directory.path.join("public"))?;
    for (name, hosts, locations_per_host) in [("small", 2, 4), ("medium", 50, 4), ("large", 500, 4)]
    {
        let source = synthetic_config(hosts, locations_per_host);
        let config_path = profile_directory.path.join(format!("{name}.toml"));
        std::fs::write(&config_path, &source)?;
        let result = profile_startup(
            name,
            hosts,
            hosts * locations_per_host,
            &source,
            &profile_directory.path,
            &config_path,
        )?;
        println!(
            "CONFIG_STARTUP size={} hosts={} locations={} parse_us={:?} parse_median_us={:.3} validation_us={:?} validation_median_us={:.3} router_compile_us={:?} router_compile_median_us={:.3} total_us={:?} total_median_us={:.3} total_range_pct={:.3}",
            result.name,
            result.hosts,
            result.locations,
            result.parse_us,
            result.parse_median_us(),
            result.validation_us,
            result.validation_median_us(),
            result.router_compile_us,
            result.router_compile_median_us(),
            result.total_us,
            result.total_median_us(),
            result.total_range_percent(),
        );
    }

    let runtime = config::compile_str(&hot_path_config(), &working_directory)?;
    let reference = HostRouter::default_static(Arc::new(std::fs::canonicalize(
        working_directory.join("public"),
    )?))?;
    for result in profile_runtime_config(&reference, &runtime)? {
        println!(
            "CONFIG_HOT_PATH stage={} iterations={} samples_ns={:?} median_ns={:.3} samples_cpu_ns={:?} median_cpu_ns={:.3} range_pct={:.3}",
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

struct ProfileDirectory {
    path: PathBuf,
}

impl ProfileDirectory {
    fn new() -> std::io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hamster-wheel-config-profile-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for ProfileDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "warning: could not remove profile directory {}: {error}",
                self.path.display()
            );
        }
    }
}

fn synthetic_config(hosts: usize, locations_per_host: usize) -> String {
    let mut source = String::from("[server]\nlisten = \"127.0.0.1:8080\"\n");
    for host in 0..hosts {
        write!(
            source,
            "\n[[hosts]]\nname = \"host{host}.test\"\naliases = [\"alias{host}.test\"]\n"
        )
        .unwrap();
        for location in 0..locations_per_host {
            if host == 0 && location == 0 {
                source.push_str(
                    "\n[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = \"public\"\n",
                );
            } else {
                write!(
                    source,
                    "\n[[hosts.locations]]\npath = \"/route{location}\"\nhandler = \"not_found\"\n"
                )
                .unwrap();
            }
        }
    }
    source
}

fn hot_path_config() -> String {
    "[server]\nlisten = \"127.0.0.1:8080\"\n\n[[hosts]]\nname = \"localhost\"\naliases = [\"127.0.0.1\"]\n\n[[hosts.locations]]\npath = \"/api\"\nhandler = \"not_found\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"static\"\nroot = \"public\"\n"
        .to_owned()
}
