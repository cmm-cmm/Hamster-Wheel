use std::{error::Error, io::Write, process::ExitCode};

use hamster_wheel::{config, control, server, validate};

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
    match config::parse_invocation(std::env::args_os().skip(1))? {
        config::Invocation::Serve(selection) => {
            let runtime = config::load(selection)?;
            server::run(runtime).await?;
            Ok(())
        }
        config::Invocation::Control { selection, command } => control(selection, command).await,
        config::Invocation::Validate(selection) => validate(selection),
    }
}

/// Compiles a configuration and reports what it would serve, without
/// starting a listener, a worker or a log file.
///
/// The report goes to stdout so it can be piped; a configuration error goes to
/// stderr through `main` with exit code 1, which is what a deployment script
/// checks.
fn validate(selection: config::ConfigSelection) -> Result<(), Box<dyn Error>> {
    let runtime = config::load(selection)?;
    let report = validate::report(&runtime);
    // Not `println!`: a closed stdout must not turn a successful validation
    // into a panic and exit code 101. See `src/output.rs`.
    let _ = write!(std::io::stdout().lock(), "{report}");
    Ok(())
}

/// Sends one command to a running server's control endpoint and prints its
/// answer verbatim, so an operator sees exactly what the server reported.
async fn control(
    selection: config::ConfigSelection,
    command: control::Command,
) -> Result<(), Box<dyn Error>> {
    let Some(name) = config::control_endpoint(&selection)? else {
        return Err("this configuration has no [server.control] endpoint".into());
    };
    #[cfg(windows)]
    {
        let response = control::request(&name, command).await?;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(response.as_bytes());
        let _ = stdout.flush();
        if response.starts_with("ERR ") {
            return Err(format!("control command {} was refused", command.name()).into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (&name, command);
        Err("the control endpoint is a Windows named pipe".into())
    }
}
