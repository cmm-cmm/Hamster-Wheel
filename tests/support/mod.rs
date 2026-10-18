//! Helpers shared by the integration tests that drive the repository PHP runtime.
#![allow(dead_code, reason = "each test binary uses a subset")]

/// The repository PHP runtime, vendored per platform like benches/php_ipc.rs.
/// `php/` is ignored by Git: supply the runtime locally (on Linux, a symlink to
/// the system `php8.5` works) and never vendor binaries.
#[cfg(windows)]
pub const PHP_RELATIVE_PATH: &str = "php/8.5/php.exe";
#[cfg(not(windows))]
pub const PHP_RELATIVE_PATH: &str = "php/8.5/php";

/// The CGI SAPI that the `fastcgi` tests start as `php-cgi -b`.
#[cfg(windows)]
pub const PHP_CGI_RELATIVE_PATH: &str = "php/8.5/php-cgi.exe";
#[cfg(not(windows))]
pub const PHP_CGI_RELATIVE_PATH: &str = "php/8.5/php-cgi";
