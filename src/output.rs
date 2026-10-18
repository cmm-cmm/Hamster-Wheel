//! Operational output that cannot take the server down.
//!
//! `println!` and `eprintln!` panic when the write fails, and it does fail
//! once the reader of a piped stream is gone: Windows reports `The pipe is
//! being closed` (os error 232), Unix `EPIPE`. A supervisor or log shipper
//! that exits or closes its end first would then turn the next line into a
//! panic wherever it happens to be written. On the main task, including the
//! accept loop reporting a failed connection task, it ends the process with
//! exit code 101 before PHP workers are shut down. In a pool's lifecycle
//! supervisor it ends replacement, and the pool refuses every request once its
//! last worker is gone. In the certificate reload task it leaves Ctrl+Break
//! unhandled, and Windows ends a process on the next one. (A generation task
//! was already contained: its panic is reported as a failed generation.)
//!
//! The server reports; it never depends on anyone reading the report. A line
//! that cannot be written is dropped, as a line written with no reader at all
//! would be. Writes remain synchronous and unbuffered beyond the standard
//! line buffer, exactly as before, so ordering and blocking behavior toward a
//! slow reader are unchanged.
//!
//! This crate's own unit tests keep the standard macros, because only they
//! reach libtest's per-test output capture; a direct write would interleave
//! every expected diagnostic with the test report. The process-level
//! regressions in `tests/` run the real binary, which uses the macros below.

/// `println!` that drops the line when stdout cannot be written.
#[cfg(not(test))]
macro_rules! outln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stdout().lock(), $($arg)*);
    }};
}

/// `eprintln!` that drops the line when stderr cannot be written.
#[cfg(not(test))]
macro_rules! errln {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!(::std::io::stderr().lock(), $($arg)*);
    }};
}

#[cfg(test)]
macro_rules! outln {
    ($($arg:tt)*) => {
        ::std::println!($($arg)*)
    };
}

#[cfg(test)]
macro_rules! errln {
    ($($arg:tt)*) => {
        ::std::eprintln!($($arg)*)
    };
}
