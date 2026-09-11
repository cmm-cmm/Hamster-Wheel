# Windows Static File ETW Profiling

This guide captures production static-file behavior without changing the
Hamster Wheel runtime. Run Windows Performance Recorder (WPR) from an elevated
PowerShell terminal. The repository's benchmark client uses HTTP/1.1,
keep-alive, a 1,000-request warm-up, and the same `/index.html` payload used by
the 0.0.2 investigations.

## Prepare the Server

Build and start the uninstrumented release binary in one terminal:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\hamster-wheel.exe
```

Confirm no unrelated build, antivirus scan, or benchmark is running. Record
the Windows build, power plan, CPU model, storage device, and whether the
document root is covered by Microsoft Defender or another minifilter.

## Capture C4 and C8

Use a separate trace for each concurrency level. The first command clears a
stale WPR session; it may report that no recording is active.

```powershell
wpr -cancel
wpr -start GeneralProfile -filemode
.\benchmarks\static_http.ps1 -Runs 1 -Requests 100000 -Concurrency 4 -WarmupRequests 1000
wpr -stop .\benchmarks\static-c4.etl "Hamster Wheel static GET C4"
```

Repeat with `-Concurrency 8` and `static-c8.etl`. Capture C32 only to confirm a
tail-latency trend; do not substitute it for C4/C8. Keep each trace short and
run the normal three-trial benchmark separately—the ETW capture itself adds
overhead.

## Analyze in WPA

Open each ETL in Windows Performance Analyzer and filter every table to
`hamster-wheel.exe`. Add these graphs:

- **CPU Usage (Sampled):** group by process, stack, and module. Compare user
  code with `ntoskrnl`, `ntdll`, and filesystem/minifilter stacks.
- **CPU Usage (Precise):** inspect context-switch count, ready time, wait time,
  wait reason, and the threads that wake Tokio workers.
- **Disk Usage** and **File I/O:** filter paths to the canonical document root;
  separate metadata/query/open activity from reads and note whether physical
  disk I/O occurs after warm-up.
- **Process Lifetime** and **Thread Activity:** count Tokio async/blocking
  threads, inspect migrations between processors, and compare C4 with C8.

Use the same table columns and time selection for both traces. A warm-cache
run can show expensive filesystem/kernel calls even when Disk Usage reports no
physical reads; File I/O and CPU stacks are therefore both required.

## Interpretation Checklist

- Rising filesystem call duration with little ready time supports kernel,
  cache-manager, or minifilter contention.
- Rising ready time, context switches, and cross-core migrations with stable
  filesystem execution supports scheduling/handoff overhead.
- Both patterns mean the causes are combined. Do not infer serialization from
  one hot kernel stack alone.

ETL files can contain machine and path information and should not be committed.
