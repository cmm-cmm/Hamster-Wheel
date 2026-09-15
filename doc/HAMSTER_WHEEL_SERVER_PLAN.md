# Hamster Wheel - Kế hoạch phát triển Web Server

## 1. Tổng quan

**Hamster Wheel** là dự án Web/Application Server hiệu suất cao viết bằng
**Rust**, hướng tới một kiến trúc web hiện đại và cross-platform. Static,
reverse proxy và application backends là các capability ngang hàng; PHP là
workload first-class và Laravel là một framework adapter được hỗ trợ.

Tên **Hamster Wheel** lấy cảm hứng từ hình ảnh **hamster running on a
wheel**: một worker được khởi động một lần, chạy trong vòng lặp, liên
tục nhận request, xử lý, reset state và sẵn sàng cho request tiếp theo.

``` text
BOOTSTRAP ONCE
      ↓
    READY
      ↓
┌─────────────────┐
│ Receive Request │
│       ↓         │
│    Execute      │
│       ↓         │
│    Response     │
│       ↓         │
│     Reset       │
│       ↓         │
│     READY ──────┼──→ vòng tiếp theo
└─────────────────┘
```

Hình tượng "wheel" vì vậy không chỉ là tên gọi mà còn phản ánh trực tiếp
triết lý **persistent worker loop** của dự án.

### Tầm nhìn

> **Hamster Wheel --- Fast. Simple. Adaptive. Observable. Measured.**

Hamster Wheel không đặt mục tiêu "viết lại Apache bằng Rust". Mục tiêu
là học những điểm mạnh đã được chứng minh từ các web server hiện đại,
loại bỏ complexity không cần thiết và xây dựng một PHP Application
Server/Web Server mới có kiến trúc riêng.

------------------------------------------------------------------------

## 2. Bối cảnh dự án

Hệ thống Hamster/XServer hiện tại đã có:

``` text
Apache
  ↓
FastCGI
  ↓
PHP
```

Do đó phiên bản Rust mới không cần bắt đầu bằng việc tái tạo lại chính
mô hình Apache + FastCGI.

Kiến trúc mới sẽ ưu tiên:

``` text
Hamster Wheel
      ↓
PHP Worker Manager
      ↓
Persistent PHP Workers
      ↓
Persistent Application
```

Apache hiện tại có thể tiếp tục đóng vai trò compatibility path trong
thời gian Hamster Wheel phát triển.

------------------------------------------------------------------------

## 3. Triết lý thiết kế

Hamster Wheel tuân theo năm nguyên tắc chính.

### Fast

Hot path ngắn, ít allocation, ít copy, ít lock và ít syscall không cần
thiết.

### Simple

Cấu hình đơn giản, secure defaults, không yêu cầu người dùng hiểu chi
tiết FastCGI hay process management để chạy một Laravel project.

### Adaptive

Không ép một chiến lược tối ưu cho mọi workload hoặc platform. Hamster
Wheel có thể lựa chọn implementation phù hợp dựa trên OS, kích thước
file, loại workload và benchmark.

### Observable

Server phải cho biết request đang chậm ở đâu thay vì chỉ trả về một con
số tổng latency.

### Measured

> Không tuyên bố nhanh hơn Apache, Nginx, Caddy, FrankenPHP hay Node.js
> cho đến khi benchmark chứng minh.

Mỗi abstraction và optimization mới phải có lý do tồn tại và được
benchmark.

------------------------------------------------------------------------

## 4. Học điểm mạnh từ các Web Server khác

Hamster Wheel không sao chép một sản phẩm cụ thể mà học các ý tưởng tốt
nhất.

  -----------------------------------------------------------------------
  Hệ thống                            Điểm Hamster Wheel nên học
  ----------------------------------- -----------------------------------
  Nginx                               Event-driven, concurrency, static
                                      file, reverse proxy, worker
                                      philosophy

  Apache                              VirtualHost, compatibility mindset,
                                      extensibility

  Caddy                               Automatic HTTPS, secure defaults,
                                      cấu hình đơn giản, DevX

  FrankenPHP                          Persistent PHP workers, PHP-first
                                      experience, worker lifecycle

  LiteSpeed                           PHP performance mindset, cache và
                                      production tuning

  IIS                                 Windows Service, process
                                      management, GUI/admin experience

  HAProxy                             Health check, upstream balancing,
                                      reliability, observability

  Traefik                             Dynamic config, service discovery,
                                      container awareness
  -----------------------------------------------------------------------

### Những thứ không nên sao chép

``` text
Apache       → .htaccess runtime và legacy complexity
Nginx        → configuration UX khó tiếp cận
IIS          → Windows-only architecture
FrankenPHP   → coupling HTTP core quá chặt với PHP
Traefik      → cloud complexity quá sớm
LiteSpeed    → phụ thuộc ecosystem commercial
```

Nguyên tắc:

> **Học ý tưởng, không copy complexity.**

------------------------------------------------------------------------

## 5. Kiến trúc tổng thể

``` text
                         HAMSTER WHEEL
                               │
                        HTTP / TLS Engine
                               │
                    Compiled Runtime Config
                               │
                         Request Router
                               │
       ┌───────────────────────┼───────────────────────┐
       │                       │                       │
    STATIC                  PHP APP                  PROXY
       │                       │                       │
 Adaptive I/O          PHP Execution Engine      Upstream Pool
                               │
                    ┌──────────┴──────────┐
                    │                     │
          Persistent Adapter          FastCGI
       APPLICATION-SPECIFIC        COMPATIBILITY
                    │                     │
              Worker Pool           PHP-FPM/php-cgi
                    │
             PHP Application
```

------------------------------------------------------------------------

## 6. HTTP Core

Ngôn ngữ chính:

``` text
Rust
```

Stack nền tảng dự kiến:

``` text
Tokio
Hyper
hyper-util
http
http-body-util
Bytes
Rustls
Serde
TOML
ArcSwap
matchit
Tracing
```

Các dependency chỉ được thêm khi milestone thực sự cần.

### HTTP roadmap

``` text
Phase đầu     HTTP/1.1
Sau đó        WebSocket
Tiếp theo     HTTP/2
Nghiên cứu    HTTP/3 / QUIC
```

HTTP/3 không thuộc MVP.

------------------------------------------------------------------------

## 7. Event-Driven Architecture

Hamster Wheel không sử dụng thread-per-connection.

``` text
Tokio Runtime
├ Connection 1
├ Connection 2
├ Connection 3
├ ...
└ Connection N
```

Connection chủ yếu tồn tại dưới dạng async state:

``` text
READ
PARSE
ROUTE
PROCESS
WAIT
WRITE
KEEP_ALIVE
```

Mục tiêu là xử lý lượng connection lớn với số OS thread nhỏ.

------------------------------------------------------------------------

## 8. Request Hot Path

``` text
Socket
  ↓
HTTP
  ↓
Host Lookup
  ↓
Location Lookup
  ↓
Handler
  ↓
Response
```

Không thực hiện trên hot path:

``` text
parse config file
scan .htaccess
global lock không cần thiết
sync logging
rebuild route table
reconnect PHP worker cho từng request
```

Nguyên tắc:

> Request path càng boring càng tốt.

------------------------------------------------------------------------

## 9. Runtime Configuration

``` text
hamster.toml
     ↓
Parse
     ↓
Validate
     ↓
Compile
     ↓
Arc<RuntimeConfig>
     ↓
ArcSwap
```

Runtime request chỉ đọc cấu hình đã compile.

Hot reload:

``` text
Old RuntimeConfig
       ↓
Build New RuntimeConfig
       ↓
Validate
       ↓
Atomic Swap
       ↓
New requests → New Config
Old requests → Complete normally
```

Mục tiêu là zero-downtime reload.

------------------------------------------------------------------------

## 10. Routing

Routing gồm ba tầng:

``` text
Request
   ↓
HostRouter
   ↓
LocationRouter
   ↓
Handler
```

Handler:

``` text
Static
PHP Worker
FastCGI
Proxy
Redirect
Error
```

Có thể sử dụng `matchit` hoặc radix-tree tương đương nếu benchmark phù
hợp.

Laravel application routes **không được Hamster Wheel parse**.

Laravel Mode chỉ quyết định:

``` text
File exists?
├ YES → Static Handler
└ NO  → Laravel Worker / public/index.php
```

------------------------------------------------------------------------

## 11. Virtual Host

Ví dụ:

``` text
pms.local
hr.local
api.local
localhost
```

Config:

``` toml
[[hosts]]
name = "pms.local"
root = "C:/Projects/PMS/public"
framework = "laravel"
php = "8.5"
https = "auto"
```

Hamster Wheel tự chuẩn bị runtime configuration tương ứng.

------------------------------------------------------------------------

## 12. PHP Execution Engine

Đây là subsystem quan trọng nhất tạo khác biệt cho Hamster Wheel.

``` text
PHP Execution Engine
│
├── Worker Mode
│      PERSISTENT ADAPTER / PERFORMANCE
│
└── FastCGI Mode
       COMPATIBILITY / FALLBACK
```

### Worker Mode

Worker Mode là hướng phát triển chính ngay từ đầu.

``` text
Hamster Wheel
      ↓
PHP Worker Pool
      ↓
Persistent PHP Process
      ↓
Application boot once
      ↓
Request Loop
```

### FastCGI Mode

FastCGI vẫn được giữ nhưng không còn là kiến trúc trung tâm.

Mục đích:

``` text
Legacy PHP
Incompatible libraries
PHP-FPM integration
Fallback
Migration
```

------------------------------------------------------------------------

## 13. PHP Worker Lifecycle

Worker state machine:

``` text
STARTING
   ↓
BOOTSTRAPPING
   ↓
READY
   ↓
BUSY
   ↓
RESETTING
   ↓
READY
```

Các trạng thái lỗi:

``` text
BUSY
 ↓
UNHEALTHY
 ↓
DRAINING
 ↓
EXIT
 ↓
RESPAWN
 ↓
BOOTSTRAPPING
 ↓
READY
```

Hamster Wheel theo dõi:

``` text
worker state
request count
memory
execution time
queue time
crash
timeout
health
restart count
```

------------------------------------------------------------------------

## 14. Persistent Application

Laravel truyền thống:

``` text
Request
 ↓
autoload
 ↓
bootstrap Laravel
 ↓
service providers
 ↓
router
 ↓
controller
 ↓
response
 ↓
shutdown
```

Hamster Wheel Worker Mode:

``` text
PROCESS START

autoload
 ↓
bootstrap Laravel
 ↓
READY
 ↓
┌─────────────────────┐
│ Request             │
│   ↓                 │
│ Route / Controller  │
│   ↓                 │
│ Response            │
│   ↓                 │
│ Reset State         │
│   ↓                 │
│ READY ──────────────┼──→ Next Request
└─────────────────────┘
```

Mục tiêu là loại bỏ application bootstrap cost khỏi mỗi request.

------------------------------------------------------------------------

## 15. State Isolation và Reset

Đây là một trong những vấn đề khó nhất của Worker Mode.

Persistent workers có nguy cơ giữ lại:

``` text
globals
static variables
singleton state
request objects
session references
database transaction state
temporary context
framework scoped services
```

Hamster Wheel phải bảo đảm:

``` text
Request A
   ↓
Execute
   ↓
Response
   ↓
RESET
   ↓
Clean Worker
   ↓
Request B
```

Không được để dữ liệu Request A rò sang Request B.

Worker reset/lifecycle được coi là subsystem quan trọng ngang với HTTP
Engine.

------------------------------------------------------------------------

## 16. Laravel Mode

Laravel support is one explicit persistent-worker adapter. The application
installs Laravel Octane; the PHP adapter owns Laravel bootstrap, sandbox
lifecycle, request reconstruction and cleanup. Rust owns only the generic
worker pool, protocol and process lifecycle.

Current configuration shape:

``` toml
[[hosts.locations]]
path = "/"
handler = "php_worker"

[hosts.locations.php]
executable = "php/8.5/php.exe"
script = "fixtures/php-worker/laravel_worker.php"
workers = 4
queue_capacity = 32

[hosts.locations.php.env]
HAMSTER_LARAVEL_PATH = "www/pms"
```

The core does not:

``` text
detect Laravel
parse Laravel routes
infer framework state ownership
provide a generic application reset
start Octane transports such as RoadRunner or Swoole
```

Future project-detection or config-generation UX requires a separate concrete
use case. It must generate ordinary Hamster configuration rather than couple
the core router to Laravel.

------------------------------------------------------------------------

## 17. Adaptive PHP Worker Pool

Config:

``` text
min_workers = 2
max_workers = 16
```

Load thấp:

``` text
W1 READY
W2 READY
```

Load tăng:

``` text
W1 BUSY
W2 BUSY
W3 BUSY
W4 BUSY
W5 READY
```

Load giảm:

``` text
Idle Timeout
    ↓
Drain Worker
    ↓
Shutdown
```

Các thuật toán autoscaling phải được benchmark trước khi trở thành
default.

------------------------------------------------------------------------

## 18. FastCGI Compatibility Layer

FastCGI được thiết kế tách khỏi PHP supervisor.

``` text
hamster-fcgi
│
├ Protocol
├ Encoder / Decoder
├ Pool
└ Transport
```

Transport abstraction:

``` text
FastCgiTransport
├ TCP
├ Unix Domain Socket
└ Named Pipe [experimental]
```

### Windows

Fallback ban đầu:

``` text
Persistent TCP Loopback
      ↓
php-cgi.exe
```

### Linux

``` text
Unix Domain Socket
      ↓
PHP-FPM
```

Named Pipe chỉ là candidate optimization nếu compatibility và benchmark
chứng minh có lợi.

------------------------------------------------------------------------

## 19. Platform Abstraction

Docker không phải OS platform riêng.

``` text
OS
├ Windows
└ Linux / Unix

Deployment
├ Native
├ Service
└ Container
```

Platform capabilities:

``` text
Process
Signal / Control
File Transfer
Service
IPC
Networking
```

Mapping:

  Capability            Windows               Linux
  --------------------- --------------------- ----------------------
  Async networking      IOCP-backed runtime   epoll-backed runtime
  Service               Windows Service       systemd
  PHP compatibility     php-cgi               PHP-FPM
  FastCGI IPC           TCP                   Unix Socket / TCP
  Fast file candidate   TransmitFile          sendfile
  Reload                Service/control API   signal/control API

`io_uring`, CPU affinity và các platform-specific optimizations là
research/performance candidates, không phải yêu cầu MVP.

------------------------------------------------------------------------

## 20. Static File Engine

Không chọn `mmap` làm mặc định trước benchmark.

Roadmap:

``` text
Baseline
└ tokio::fs / ordinary file serving

Optimization
├ Small-file RAM cache
├ Metadata cache
├ mmap experimental
├ Windows TransmitFile candidate
└ Linux sendfile candidate
```

Adaptive strategy tương lai:

``` text
Small File
   ↓
RAM Cache

Medium File
   ↓
Buffered/File I/O

Large File
   ↓
OS-assisted fast path
```

Threshold phải được benchmark, không hard-code theo cảm tính.

------------------------------------------------------------------------

## 21. Reverse Proxy

Hamster Wheel không chỉ phục vụ PHP.

``` text
Hamster Wheel
├ Laravel / PHP
├ Node.js
├ Next.js
├ ASP.NET
├ FastAPI
├ Go
├ Rust services
└ Static SPA
```

Proxy roadmap:

``` text
Connection Pool
Keep-Alive
Timeout
WebSocket
Health Check
Round Robin
Least Connections
Retry
Circuit Breaker
```

Các tính năng nâng cao chỉ được thêm sau khi core ổn định.

------------------------------------------------------------------------

## 22. TLS và HTTPS

Sử dụng Rustls khi phù hợp.

Mục tiêu:

``` text
TLS 1.2 / 1.3
SNI
Certificate Manager
Automatic HTTPS
Certificate Reload
Secure Defaults
```

Automatic HTTPS nên học triết lý DevX của Caddy nhưng triển khai theo
kiến trúc Hamster Wheel.

------------------------------------------------------------------------

## 23. Không dùng .htaccess Runtime

Hamster Wheel không đọc `.htaccess` trên từng request.

Thay vào đó:

``` text
Config
 ↓
Compile
 ↓
Runtime Rules
```

Có thể nghiên cứu migration tool:

``` powershell
hamster import .htaccess
```

Tool chỉ convert các rule được hỗ trợ sang cấu hình Hamster Wheel.

------------------------------------------------------------------------

## 24. Security

Secure defaults:

``` text
block .env
block .git
path traversal protection
header/body limits
request timeout
TLS defaults
invalid host handling
worker isolation
crash recovery
```

Các manifest như `composer.json` hoặc `package.json` không nên bị block
cứng trong mọi trường hợp; policy phụ thuộc framework/config.

------------------------------------------------------------------------

## 25. Observability

Hamster Wheel cần metrics built-in.

Ví dụ:

``` text
/__hamster/status
```

Metrics:

``` text
requests
active connections
RPS
P50 / P95 / P99
PHP workers
busy workers
worker queue
worker memory
worker restarts
proxy health
uptime
```

Production có thể cho phép disable hoặc bảo vệ endpoint.

------------------------------------------------------------------------

## 26. Slow Request Detector

Ví dụ:

``` text
GET /reports

Total              842.0 ms
Hamster HTTP         0.3 ms
Worker Queue          1.4 ms
PHP Execution       812.0 ms
Response/Network     28.3 ms
```

Mục tiêu là giúp developer biết bottleneck thực sự nằm ở:

``` text
Hamster
Worker queue
PHP
Database/Application
Proxy
Network
```

Đây là giá trị quan trọng hơn một benchmark Hello World đơn thuần.

------------------------------------------------------------------------

## 27. Logging

Sử dụng `tracing`.

``` text
Request
 ↓
Event
 ↓
Non-blocking Channel
 ↓
Logging Worker
 ↓
Console / File
```

Không synchronous disk logging trên hot path.

Log format có thể hỗ trợ:

``` text
Human-readable
JSON
Access log
Error log
Slow request log
Worker lifecycle log
```

------------------------------------------------------------------------

## 28. Hamster Manager

GUI chỉ là management client.

``` text
Hamster Manager
      │
      ├ Projects
      ├ Virtual Hosts
      ├ PHP Versions
      ├ Worker Pools
      ├ SSL
      ├ Proxy
      ├ Logs
      └ Metrics
             │
             ↓
        Hamster Service
```

Core vẫn hoạt động độc lập qua CLI:

``` powershell
hamster serve
hamster reload
hamster status
hamster stop
```

Điều này cho phép dùng Hamster Wheel trên Windows Server/Linux không cần
GUI.

------------------------------------------------------------------------

## 29. Deployment

### Windows

``` text
Hamster Manager
      ↓
Hamster Windows Service
      ↓
IOCP-backed Tokio Runtime
      ↓
PHP Worker Manager
```

Windows là môi trường chiến lược quan trọng vì Hamster Wheel hướng tới
DevX mạnh cho PHP/Laravel.

### Linux

``` text
systemd
  ↓
Hamster Wheel
  ↓
epoll-backed Tokio Runtime
```

Worker Mode vẫn có thể được dùng, đồng thời FastCGI/PHP-FPM tồn tại như
compatibility backend.

### Docker

Docker là deployment target:

``` text
Option A
Hamster + PHP + App
```

hoặc:

``` text
Option B
Hamster
   ↓
External PHP / Services
```

Không ép người dùng vào một deployment model.

------------------------------------------------------------------------

## 30. Source Architecture

Không chia quá nhiều crate ở Phase 0.

Ban đầu:

``` text
hamster-wheel/
├ Cargo.toml
└ src/
   ├ main.rs
   ├ server.rs
   └ config.rs
```

Khi subsystem đã đủ lớn mới refactor:

``` text
crates/
├ hamster-core
├ hamster-http
├ hamster-router
├ hamster-static
├ hamster-php
├ hamster-fcgi
├ hamster-proxy
├ hamster-tls
├ hamster-config
├ hamster-platform
├ hamster-logging
└ hamster-metrics
```

Đặc biệt:

``` text
hamster-php
```

quản lý:

``` text
Runtime
Worker Pool
Lifecycle
State Reset
Health
Memory
Drain
Respawn
Scaling
```

Trong khi:

``` text
hamster-fcgi
```

chỉ chịu trách nhiệm protocol/transport/pooling FastCGI.

------------------------------------------------------------------------

## 31. Roadmap mới

### 0.0.1 - HTTP Baseline

``` text
Tokio
 ↓
TcpListener
 ↓
TokioIo
 ↓
Hyper HTTP/1.1
 ↓
200 OK
```

Response:

``` text
Server: HamsterWheel/0.0.1
Hello, Hamster Wheel!
```

Đo:

``` text
RPS
P50
P95
P99
CPU
RAM
```

### 0.0.2 - Static File

``` text
Static file baseline
MIME
Path normalization
Security baseline
```

Benchmark lại.

### 0.0.3 - Routing

``` text
HostRouter
LocationRouter
Handler
```

Benchmark lại.

### 0.0.4 - Runtime Config

``` text
TOML
Validate
Compile
ArcSwap
Reload
```

Benchmark lại.

### 0.1.0 - PHP Worker Prototype

``` text
Persistent PHP Process
Application boot once
Request loop
Basic request/response transport
```

Đây là milestone chiến lược đầu tiên.

### 0.2.0 - Fixed Worker Pool + Cancellation Safety

Status: implemented on the current `dev` branch; Windows qualification and
benchmark evidence are recorded in `doc/BENCHMARKS.md`.

``` text
Fixed-size worker pool
Bounded queue + backpressure
Scheduling
Cancellation safety
Unavailable tracking + degraded pool
Startup all-or-nothing
Graceful shutdown
```

Respawn và health management nâng cao đã chuyển sang 0.3.0. Ở 0.2.0, worker
lỗi chỉ được đánh dấu unavailable và pool tiếp tục phục vụ bằng các worker
còn khỏe. Cancellation safety là yêu cầu bắt buộc của milestone này: HTTP
client cancellation không được phép hủy một PHP transaction đã bắt đầu.
Chi tiết thiết kế trong `doc/research/WORKER_POOL_REVIEW.md`.

Giới hạn queue 0.2.0 chỉ bound số job đã được pool nhận; HTTP body vẫn được
buffer trước admission nên đây chưa phải total-memory backpressure. Total-byte
admission hoặc body streaming là backlog riêng, không được ghép vào lifecycle
work của 0.3.0 nếu chưa có design review.

### 0.3.0 - Worker Lifecycle

Status: implemented and accepted on `dev`; the lifecycle contract and Windows
qualification are recorded in `doc/WORKER_LIFECYCLE_0_3.md`.

``` text
Permanent worker slots + generation identity
Automatic respawn
Per-slot repeated-failure backoff and bounded cooldown
Optional max-requests/max-age recycle (default OFF)
Lifecycle telemetry
Coordinated drain and shutdown
Crash recovery
```

0.3.0 owns process lifecycle only. Idle crashes use passive/lazy detection;
there is no proactive stdout probe. Application lifecycle integration moved to
the 0.4.0 Laravel Worker milestone. Memory-threshold recycling, adaptive
sizing, and protocol changes remain out of scope. The accepted contract is
persisted in `doc/WORKER_LIFECYCLE_0_3.md`.

### 0.4.0 - Laravel Worker Mode

Status: investigation and implementation committed at `6e961d5`; milestone
closure, tag and release remain owner decisions.

``` text
Laravel bootstrap once
Laravel Octane request lifecycle inside the adapter
Request reconstruction and response staging
Cross-request isolation and cleanup failure handling
Real PMS validation
```

0.4 validates the persistent-worker architecture against one real framework.
Laravel Octane is not a Hamster core dependency and does not define lifecycle
semantics for other frameworks.

### Proposed 0.5.0 - HTTP and Transport Foundation

``` text
Typed transport/request context
HTTP/1.1 limits, timeouts and bounded memory
Streaming-capable response body with a real static-file consumer
Static correctness and containment investigation
Minimum request/stage instrumentation
Native Linux core qualification
```

### Proposed 0.6.0 - PHP Compatibility and Adapter Contract

``` text
FastCGI protocol
Windows php-cgi and Linux PHP-FPM transports
Multiple independent PHP backends
Generic persistent-worker adapter contract
Transport metadata mapping
At-most-once failure semantics
```

### Proposed 0.7.0 - Reverse Proxy and General-Purpose Data Plane

``` text
HTTP upstream connection pool
Streaming and backpressure
Forwarded-header trust policy
Bounded timeouts
Provably-safe retry boundary
Mixed static/PHP/proxy virtual hosts
```

### Proposed 0.8.0 - Reload, Observability and Production Operation

``` text
Transactional config publication and old-generation drain
Structured access/error/lifecycle logging
Metrics, health and readiness
Windows Service operation and child containment
Linux systemd operation
Operational CLI
```

### Proposed 0.9.0 - Secure Edge and Release Qualification

``` text
TLS 1.2 / 1.3
SNI and certificate reload
HTTP/2 when edge-capable 1.0 is selected
Windows and Linux release qualification
Security/fault/soak matrix
Packaging, upgrade and rollback documentation
```

### Proposed 1.0 - Production Web/Application Server

Mục tiêu:

``` text
Bounded HTTP core and correct transport metadata
Secure static serving
FastCGI PHP compatibility
Persistent-worker adapter contract and qualified Laravel adapter
Minimal reliable reverse proxy
Transactional reload and graceful drain
Logs, metrics, health and readiness
Windows Service and Linux systemd support
TLS/HTTP2 for an edge-capable product profile
Stable configuration and protocol compatibility policy
```

Adaptive worker sizing and Hamster Morph are no longer on the pre-1.0 critical
path. Automatic HTTPS, HTTP/3, GUI, advanced proxy policies, specialized static
I/O, service discovery and additional framework adapters also remain post-1.0
unless a concrete workload changes the evidence. The detailed proposal,
alternatives and owner decisions are in `doc/ROADMAP_0_5_TO_1_0.md`.

------------------------------------------------------------------------

## 32. Benchmark Strategy

Benchmark phải thực hiện sau từng milestone.

``` text
0.0.1 HTTP
0.0.2 Static
0.0.3 Router
0.0.4 Config
0.1.0 Single PHP Worker
0.2.0 Fixed Worker Pool + Cancellation Safety
0.3.0 Worker Lifecycle / Respawn / Recycle
0.4.0 Laravel Worker
0.5.0 HTTP / Transport / Static Foundation
0.6.0 FastCGI and Persistent Adapter Contract
0.7.0 Reverse Proxy
0.8.0 Reload / Observability / Service Operation
0.9.0 TLS / HTTP2 / Release Qualification
```

Nếu performance giảm, phải biết feature nào gây regression.

### Chỉ benchmark Release build

``` powershell
cargo build --release
```

Không sử dụng Debug build để kết luận hiệu suất.

------------------------------------------------------------------------

## 33. Benchmark Matrix

Web server comparison:

``` text
Apache
Nginx
Caddy
FrankenPHP
LiteSpeed/OpenLiteSpeed
Hamster Wheel
```

PHP comparison:

``` text
Apache + PHP
Nginx + PHP-FPM
Hamster Wheel + FastCGI
Hamster Wheel + Worker Mode
FrankenPHP Worker
```

Long-term application runtime comparison:

``` text
PHP + Laravel + Hamster Wheel Worker
vs
Node.js + Fastify
```

Các test phải dùng:

``` text
same hardware
same database
same query
same response payload
same business logic
same concurrency
same network conditions
```

Metrics:

``` text
RPS
P50
P95
P99
CPU
RAM
CPU/request
worker memory
context switches
cold request
warm request
error rate
```

------------------------------------------------------------------------

## 34. PHP đấu với Node.js

Đây là **mục tiêu nghiên cứu dài hạn**, không phải marketing claim.

FastCGI truyền thống:

``` text
HTTP
 ↓
Web Server
 ↓
IPC
 ↓
PHP
```

Hamster Wheel Worker Mode:

``` text
Hamster HTTP
 ↓
Persistent PHP Runtime
 ↓
Already-booted Application
```

Node.js:

``` text
Node HTTP
 ↓
Persistent Application Runtime
```

Worker Mode làm khoảng cách kiến trúc nhỏ đi đáng kể.

Mục tiêu nghiên cứu:

> Có thể đưa PHP/Laravel với persistent workers đến mức cạnh tranh với
> Node.js/Fastify trong các workload web thực tế hay không?

Câu trả lời phải đến từ benchmark.

Hamster Wheel không được tuyên bố PHP thắng Node.js nếu chưa có dữ liệu.

Các workload cần thử:

``` text
Hello World
JSON API
REST CRUD
Database read
Database write
Redis/cache
Template rendering
Mixed static/dynamic
High concurrency
Long-running I/O
```

Đặc biệt cần phân biệt:

``` text
Cold Laravel
vs
Warm Laravel Worker
```

------------------------------------------------------------------------

## 35. Candidate Optimizations

Các kỹ thuật sau **không phải requirement ban đầu**:

``` text
mmap
sendfile
TransmitFile
io_uring
CPU affinity
per-core runtime
NUMA tuning
custom buffer pool
Named Pipe FastCGI
lock-free custom structures
custom allocator
```

Quy trình:

``` text
Problem
 ↓
Measure
 ↓
Hypothesis
 ↓
Implement
 ↓
Benchmark
 ↓
Keep or Remove
```

Không optimization theo cảm tính.

------------------------------------------------------------------------

## 36. Definition of Done cho Phase 0

Phase 0 hoàn thành khi:

``` text
[ ] Build thành công bằng Rust stable MSVC trên Windows
[ ] Listen 127.0.0.1:8080
[ ] HTTP/1.1 hoạt động
[ ] GET / trả 200 OK
[ ] Server header = HamsterWheel
[ ] Keep-Alive hoạt động
[ ] Release build hoạt động
[ ] Ctrl+C shutdown sạch
[ ] Có baseline RPS
[ ] Có P50/P95/P99
[ ] Có CPU baseline
[ ] Có RAM baseline
[ ] Không optimization chưa được benchmark
```

------------------------------------------------------------------------

## 37. Nguyên tắc phát triển

### Không premature optimization

``` text
Correctness
 ↓
Architecture
 ↓
Measurement
 ↓
Optimization
```

### Không premature abstraction

Không tách crate hoặc tạo abstraction nếu chưa có ít nhất một use case
thực tế.

### Không benchmark marketing

Không dùng Hello World benchmark để khẳng định application performance.

### Không khóa kiến trúc vào PHP

PHP là first-class citizen của Hamster Wheel, nhưng HTTP core vẫn có thể
phục vụ static content và reverse proxy tới các runtime khác.

### Compatibility không được cản Innovation

FastCGI tồn tại để tương thích.

Worker Mode tồn tại để Hamster Wheel tiến xa hơn mô hình web server PHP
truyền thống.

------------------------------------------------------------------------

## 38. Lợi thế cạnh tranh mục tiêu

Hamster Wheel hướng tới kết hợp:

``` text
Nginx-like performance philosophy
+
Caddy-like developer experience
+
IIS-like Windows integration
+
FrankenPHP-like persistent PHP awareness
+
HAProxy-like observability/reliability
+
Hamster-specific adaptive worker management
```

Điểm khác biệt quan trọng:

``` text
Rust native core
Cross-platform
Persistent PHP Worker Mode
Adaptive Worker Pool
Multiple PHP execution modes
Framework adapters, including Laravel
FastCGI fallback
Windows-first-class support
Linux production support
Docker deployment
Built-in observability
GUI + CLI
```

------------------------------------------------------------------------

## 39. Nhận diện Hamster Wheel

Tên:

``` text
Hamster Wheel
```

Hình tượng:

``` text
Hamster running on a wheel
```

Ý nghĩa kỹ thuật:

``` text
Hamster      → Worker
Wheel        → Persistent Request Loop
Running      → Continuous Request Processing
```

Có thể phát triển nhận diện logo theo hình:

``` text
hamster + circular wheel
```

Vòng tròn đồng thời tượng trưng cho:

``` text
Event Loop
Worker Loop
Continuous Runtime
```

Tagline đang cân nhắc:

> **Hamster Wheel --- Keep the Web Running.**

Hoặc:

> **Hamster Wheel --- Small. Fast. Always Running.**

Tagline chưa phải quyết định cuối cùng.

------------------------------------------------------------------------


## 40. Open Source Reference Projects

Hamster Wheel nên chủ động nghiên cứu các web server và networking project mã nguồn mở trước khi tự phát minh một subsystem mới. Mục tiêu là học kiến trúc, trade-off và các pattern đã được chứng minh, sau đó tự triển khai theo kiến trúc và license của Hamster Wheel.

| Project | Giá trị tham khảo chính | Ưu tiên |
|---|---|---|
| Nginx | Event loop, worker model, connection handling, static file, reverse proxy | Cao |
| Caddy | Automatic HTTPS, configuration architecture, secure defaults, DevX | Cao |
| FrankenPHP | Persistent PHP workers, PHP lifecycle, worker mode, Laravel/PHP integration | Rất cao |
| Apache HTTP Server | VirtualHost, module architecture, compatibility và request lifecycle | Trung bình |
| HAProxy | Connection management, upstream pools, load balancing, health checks, reliability | Cao khi làm proxy |
| Traefik | Dynamic configuration, service discovery, Docker/Kubernetes integration | Sau 1.0 |
| Pingora | Rust networking/proxy architecture, connection pooling và performance engineering | Rất cao |
| Hyper | HTTP implementation bằng Rust và nền tảng HTTP core dự kiến của Hamster Wheel | Rất cao |

### Thứ tự nghiên cứu đề xuất

```text
Phase 0 / HTTP Core
├ Hyper
├ Pingora
└ Nginx

PHP Worker Mode
├ FrankenPHP
├ PHP runtime / worker lifecycle
└ Laravel Octane concepts

Configuration / TLS / DevX
└ Caddy

Proxy / Reliability
├ HAProxy
├ Nginx
└ Pingora

Container / Service Discovery
└ Traefik
```

### Nguyên tắc nghiên cứu mã nguồn mở

> **Research existing implementations before inventing, implement independently, then benchmark.**

Quy trình: 

```text
Problem
  ↓
Research existing open-source implementations
  ↓
Understand architecture and trade-offs
  ↓
Design the Hamster Wheel solution
  ↓
Implement independently
  ↓
Benchmark
  ↓
Document the decision
```

Không copy code tùy tiện giữa các project. Trước khi tái sử dụng bất kỳ đoạn code nào phải kiểm tra license và compatibility với license của Hamster Wheel. Ưu tiên học **ý tưởng, pattern, thuật toán và trade-off**, sau đó viết implementation riêng.

### Hai project cần ưu tiên đặc biệt

**Pingora** rất đáng nghiên cứu vì đây là một hệ thống networking/proxy hiệu suất cao viết bằng Rust. Nó có thể cung cấp nhiều bài học về cách tổ chức networking core, connection management và performance engineering trong Rust.

**FrankenPHP** rất đáng nghiên cứu cho hướng PHP Worker Mode vì Hamster Wheel cũng muốn giữ PHP application sống qua nhiều request. Cần tập trung nghiên cứu worker lifecycle, application bootstrap, state isolation/reset, crash recovery và compatibility thay vì sao chép implementation.

### Research Notes

Khi nghiên cứu một project, nên tạo ADR hoặc research note gồm:

```text
Project
Feature studied
Problem it solves
Architecture
Advantages
Trade-offs
License considerations
What Hamster Wheel can learn
What Hamster Wheel should NOT copy
Benchmark hypothesis
Decision
```

Điều này giúp các quyết định kiến trúc sau này có nguồn gốc rõ ràng và tránh lặp lại nghiên cứu.

---
## 41. Kết luận

Hamster Wheel bắt đầu từ một nhu cầu thực tế: vượt khỏi giới hạn của
stack Apache/FastCGI hiện tại và nghiên cứu một cách phục vụ PHP hiện
đại hơn.

Hamster Wheel không bị định nghĩa bởi một application framework hay một PHP
execution mode. Kiến trúc sản phẩm là:

``` text
HTTP / TLS / Transport Context
              ↓
Compiled Routing and Resource Policy
              ↓
Static | FastCGI | Persistent Adapter | Reverse Proxy
```

FastCGI là PHP compatibility layer. Persistent workers là application-server
contract có lifecycle adapter riêng. Laravel Octane chỉ thuộc Laravel adapter.
Reverse proxy là đường tích hợp general-purpose cho các runtime khác.

Hamster Wheel sẽ học những điểm mạnh của các web server lớn nhưng giữ
triết lý riêng:

> **Fast. Simple. Adaptive. Observable. Measured.**

Mục tiêu ngắn hạn là xây một HTTP core đúng, nhỏ và đo được.

Mục tiêu trung hạn là hoàn thiện production spine: HTTP/transport correctness,
static, FastCGI, persistent adapters, reverse proxy, reload, observability và
cross-platform operation.

Mục tiêu dài hạn là biến Hamster Wheel thành một **cross-platform
Web/Application Server** đủ mạnh để cạnh tranh với các stack web hiện đại.
Morph và adaptive execution chỉ được nghiên cứu trên nền production đã ổn định
và dữ liệu workload thật.

Hiệu năng PHP persistent vẫn là một workload research question quan trọng,
không phải định nghĩa sản phẩm:

> **PHP + Hamster Wheel có thể đấu ngang với Node.js trong workload web
> thực tế hay không?**

Câu trả lời sẽ không đến từ giả định.

**Nó sẽ đến từ code và benchmark.**
