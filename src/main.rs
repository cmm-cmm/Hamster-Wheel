use std::{error::Error, io::Write, process::ExitCode};

use hamster_wheel::{config, server};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Not `eprintln!`: with stderr closed it would panic and replace
            // this exit code with 101. See `src/output.rs`.
            let _ = writeln!(std::io::stderr().lock(), "hamster-wheel: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let selection = config::parse_arguments(std::env::args_os().skip(1))?;
    let runtime = config::load(selection)?;
    server::run(runtime).await?;
    Ok(())
}
