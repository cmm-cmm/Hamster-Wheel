use std::{error::Error, process::ExitCode};

use hamster_wheel::{config, server};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hamster-wheel: {error}");
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
