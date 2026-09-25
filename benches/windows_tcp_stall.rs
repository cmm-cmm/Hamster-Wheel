//! Pure-socket reproducer for the Windows loopback send stall behind the
//! reverse proxy's buffer-bound collapse and its ~15 ms tail. No HTTP, no
//! Tokio, no Hamster: one sender thread and one receiver thread on a loopback
//! connection.
//!
//! The sender writes 458,752-byte pieces (what Hyper's queued writev offered
//! in the proxy traces) on a nonblocking socket and retries `WSAEWOULDBLOCK` in
//! a spin loop, so the time it reports is the time the kernel refused to take
//! more data, not a readiness notification delay. The receiver reads into a
//! buffer of the given size and then busy-waits a fixed delay per read, which
//! stands in for a proxy that forwards each read before reading again.
//!
//! ```text
//! cargo bench --target x86_64-pc-windows-msvc --bench windows_tcp_stall --features profiling
//! cargo bench --target x86_64-pc-windows-msvc --bench windows_tcp_stall --features profiling -- <read bytes> <delay us> <SO_RCVBUF bytes|->
//! ```
//!
//! Without arguments it runs the cells documented in
//! `benchmarks/results/reverse_proxy_v1_20260924.md`. Stalls are sender waits
//! of at least 5 ms; on the measured machine they cluster at 10-17 ms and end
//! when a TCP timer fires (ETW), and they appear when one read can drain the
//! whole socket receive buffer while the reader is paused.

#[cfg(windows)]
fn main() {
    let arguments: Vec<String> = std::env::args()
        .skip(1)
        .filter(|argument| !argument.starts_with('-') || argument == "-")
        .collect();
    println!("# Windows loopback send stall, 64 MiB per run, 3 runs per cell");
    println!();
    println!(
        "| read bytes | delay us | SO_RCVBUF | ms per 64 MiB (runs) | sender waits >= 5 ms (runs) | total stalled ms (runs) |"
    );
    println!("|---:|---:|---|---|---|---|");
    if arguments.len() >= 3 {
        let read = arguments[0].parse().expect("read bytes");
        let delay = arguments[1].parse().expect("delay us");
        let receive_buffer = arguments[2].parse().ok();
        stall::cell(read, delay, receive_buffer);
        return;
    }
    for (read, delay, receive_buffer) in [
        (65_536, 0, None),
        (417_792, 0, None),
        (65_536, 200, None),
        (417_792, 200, None),
        (417_792, 200, Some(1_048_576)),
    ] {
        stall::cell(read, delay, receive_buffer);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("windows_tcp_stall measures Windows loopback TCP only");
}

#[cfg(windows)]
mod stall {
    use std::{
        io::{ErrorKind, Read, Write},
        net::{Ipv4Addr, TcpListener, TcpStream},
        os::windows::io::AsRawSocket,
        time::{Duration, Instant},
    };

    const TOTAL: usize = 64 << 20;
    const SEND: usize = 458_752;
    const RUNS: usize = 3;
    const SOL_SOCKET: i32 = 0xffff;
    const SO_RCVBUF: i32 = 0x1002;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn setsockopt(socket: usize, level: i32, name: i32, value: *const u8, length: i32) -> i32;
    }

    fn set_receive_buffer(stream: &TcpStream, size: i32) {
        // SAFETY: the socket is open for the duration of the call and the
        // option value is a live i32 of the length passed.
        let result = unsafe {
            setsockopt(
                stream.as_raw_socket() as usize,
                SOL_SOCKET,
                SO_RCVBUF,
                (&raw const size).cast(),
                4,
            )
        };
        assert_eq!(result, 0, "setsockopt(SO_RCVBUF)");
    }

    struct Run {
        elapsed_ms: f64,
        stalls: usize,
        stalled_ms: f64,
    }

    pub fn cell(read: usize, delay_us: u64, receive_buffer: Option<i32>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let mut sender =
            TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
        let (mut receiver, _) = listener.accept().expect("accept");
        sender.set_nodelay(true).expect("nodelay");
        if let Some(size) = receive_buffer {
            set_receive_buffer(&receiver, size);
        }
        sender.set_nonblocking(true).expect("nonblocking");
        let delay = Duration::from_micros(delay_us);
        let reader = std::thread::spawn(move || {
            let mut buffer = vec![0_u8; read];
            for _ in 0..RUNS {
                let mut received = 0;
                while received < TOTAL {
                    received += receiver.read(&mut buffer).expect("read");
                    let started = Instant::now();
                    while started.elapsed() < delay {
                        std::hint::spin_loop();
                    }
                }
            }
        });
        let data = vec![b'x'; SEND];
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            runs.push(send_run(&mut sender, &data));
            std::thread::sleep(Duration::from_millis(50));
        }
        reader.join().expect("reader");
        let join =
            |value: fn(&Run) -> String| runs.iter().map(value).collect::<Vec<_>>().join(" / ");
        println!(
            "| {read} | {delay_us} | {} | {} | {} | {} |",
            receive_buffer.map_or_else(|| "default".to_owned(), |size| size.to_string()),
            join(|run| format!("{:.0}", run.elapsed_ms)),
            join(|run| run.stalls.to_string()),
            join(|run| format!("{:.0}", run.stalled_ms)),
        );
    }

    fn send_run(sender: &mut TcpStream, data: &[u8]) -> Run {
        let started = Instant::now();
        let mut stalls = 0;
        let mut stalled_ms = 0.0;
        let mut sent = 0;
        while sent < TOTAL {
            let mut piece = &data[..(TOTAL - sent).min(SEND)];
            sent += piece.len();
            while !piece.is_empty() {
                let waiting = Instant::now();
                let written = loop {
                    match sender.write(piece) {
                        Ok(written) => break written,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            std::hint::spin_loop();
                        }
                        Err(error) => panic!("send: {error}"),
                    }
                };
                let waited = waiting.elapsed().as_secs_f64() * 1_000.0;
                if waited >= 5.0 {
                    stalls += 1;
                    stalled_ms += waited;
                }
                piece = &piece[written..];
            }
        }
        Run {
            elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
            stalls,
            stalled_ms,
        }
    }
}
