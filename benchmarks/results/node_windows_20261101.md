# Windows: Hamster/PHP và Node.js core HTTP — 2026-11-01

## FACT — Phạm vi và phương pháp

Bổ sung Node.js theo yêu cầu Owner. Đây là phép đo hai **stack thực thi**:
Hamster với PHP persistent workers và Node core HTTP/cluster. Không phải phép
so Rust với JavaScript thuần, Laravel với Express, hay hai sản phẩm có cùng
chính sách bảo vệ/admission. Không thay production code cho lượt này.

Host Windows 11 10.0.26300, Intel i5-1235U, 10 core/12 logical CPU, ~16 GB RAM, máy dùng
chung. Release MSVC Hamster 0.8.0 tại HEAD `db5c472` với working tree chưa
commit. Khi bắt đầu lượt này, `src/php_protocol.rs` và `src/php_worker.rs` đã
có thay đổi đường ghi REQUEST frame; giữ nguyên và build lại trước khi đo.
Binary SHA-256:
`D1E2B14CF48611844762B5C1B3CC6D13EEAD8DA0D87825A6803071FC63F74163`.
**Khác binary của bảng FrankenPHP trước đó:** không ghép bảng cũ thành
so sánh ba bên hoặc dùng chúng để tính phần trăm cải thiện của sửa frame.

- Node có sẵn tại `C:/Program Files/nodejs/node.exe`, version **26.10.0**,
  V8 14.6.202.34-node.34, OpenSSL 3.5.8. Không cài/npm update runtime.
- PHP **8.5.12-1 NTS** từ cache cô lập đã kiểm SHA chính thức ở lượt trước;
  INI tối thiểu giống heavy/baseline PHP, không bật OPcache CLI. Giữ nguyên
  PHP operator/system/repository; `php/8.5` vẫn chưa được nâng runtime.
- oha **1.16.0**, HTTP/1.1 loopback, keep-alive, client cùng máy. Mỗi cell:
  **H N N H H N**, ba cây process mới mỗi bên; warm-up 1.000 request tại
  min(C, 8), rồi **10 giây** gửi tải với `-w` chờ request đang chạy kết thúc.
- Không chạy build/test Rust đồng thời với measured matrix. Smoke 0,2 giây
  chỉ kiểm orchestration; không đưa vào số liệu hiệu suất.
- Body/status được kiểm tra trước/sau mỗi measured run; CPU/POST digest có
  Python reference độc lập. oha kiểm status/error của từng request nhưng
  không so body từng response. GET/POST thành công phải là 200; 404 cell
  yêu cầu 404. Hamster có body lỗi 9 B, Node 404 rỗng: cell này không cùng bytes.

Driver: [node_windows.py](../node_windows.py), fixture
[node_http.cjs](../fixtures/node_http.cjs). Dùng lại sampler/measurement từ
`frankenphp_windows.py`; chỉ thêm callback launch/finish để Node được cleanup
đúng lifecycle riêng. Không sửa luồng đo FrankenPHP trước đó.

### Topology và workload

Node dùng **cluster primary + N worker process**, kể cả W1; explicit
`SCHED_RR` phân phối **connection**, keep-alive giữ request trên worker đã
nhận connection. Đây không phải standalone Node một process hay phép đo
Windows default scheduling/minimum RAM/best tuning. Các worker chỉ cần core
modules, không Express/Fastify. CPU work synchronous trong mỗi worker.
Theo [Node cluster docs](https://nodejs.org/api/cluster.html), RR không phải
default trên Windows; fixture chọn rõ để tránh lệch phân phối connection.
Hamster dùng runtime Tokio mặc định; không pin core/CPU quota. Node baseline
404/static chỉ có một HTTP worker; vì vậy không xem RPS của các cell đó là
so sánh transport tại cùng số CPU execution threads. W4 chỉ khớp số process
thực thi ứng dụng, không khớp tổng số thread hay toàn bộ chính sách server.
Node xử lý HTTP ngay trong application process; Hamster gửi transaction qua
pipe đến PHP. Chênh lệch ranh giới process là một phần của stack được đo.

| Cell | Work thực tế |
| --- | --- |
| 404 C1/C32 | Hamster compiled not_found vs Node status-only; không PHP |
| Static 23 B C32 | Cùng `public/index.html` CRLF, request `/` |
| Static 1 MiB C8 | Cùng file 1 MiB ASCII x, request `/1m.bin` |
| Hello W1/C1, W4/C32 | Cùng 14 B `Hello from PHP`, dù Node viết bằng JS |
| JSON W4/C32 | Serialize cùng object, cùng bytes `{"message":"Hello from PHP"}` |
| CPU W4/C16 | 20.000 SHA-256 hex liên tiếp, bắt đầu từ 64 ASCII a |
| POST W4/C16 | Upload đúng 1 MiB ASCII x, trả SHA-256 hex 64 B |

Static Node mở file, stat **chính handle đó**, stream 128 KiB/lần có
backpressure, chỉ allowlist hai fixture. Không có canonical containment,
virtual-host/config graph, liveness/admin/lifecycle như Hamster; fixture này
**không được dùng làm production file server**. Đây là reference IO cost.
Hamster static giữ toàn bộ boundary hiện tại. Node và PHP có cùng thuật
toán CPU nhưng khác API/binding/allocator; chênh lệch không phải thuần HTTP.

Hamster worker `queue_capacity=64`, giữ body/aggregate budgets và timeout.
Node có body limit 1 MiB trong adapter nhưng không implement tương đương
aggregate budget/bounded job admission. Vì vậy không suy ra cùng khả năng
chống overload/memory exhaustion từ throughput ở C16/C32.

### Cách đọc CPU/RAM

Mọi chỉ tiêu là independent median ba run, RPS có min/median/max. CPU là
user+kernel **cả cây server** / số request thực tế, không bao gồm oha; Node
bao gồm primary và worker, Hamster gồm host/worker descendants. Working set
cộng các process có thể đếm shared pages nhiều lần; private bytes là commit.
RAM được lấy cuối run, không phải peak hoặc bằng chứng không leak. Không
benchmark startup/graceful shutdown/kill recovery trong throughput matrix.
Sampler ghi nhận 2/4/10 process ở Hamster tùy cell và 3/6 ở Node: tính cả
descendants phụ của Windows, không chỉ application worker. PID/creation
identity được giữ trong raw; không dùng số process như số execution slot.

## FACT — Kết quả

**54 measured runs, 6.431.035 request: 3.477.322 HTTP 200 và 2.953.713 HTTP
404 đúng dự kiến. Không có lỗi transport, status khác hay 503.** Tất cả 54
preflight/post-load response checks qua. Measured elapsed 10,005–10,214 s;
không bỏ outlier hoặc thay run. Không dùng smoke vào thống kê.

### RPS min / median / max

RPS của 404 là số response 404 đúng dự kiến; những cell khác chỉ tính 200.

| Workload | Hamster/PHP | Node core HTTP |
| --- | ---: | ---: |
| not_found C1 | 9287 / 10467 / 11620 | 7688 / 8253 / 11717 |
| not_found C32 | 48049 / 52534 / 55765 | 21497 / 26894 / 31228 |
| static 23 B C32 | 3828 / 4368 / 4748 | 3926 / 4081 / 4787 |
| static 1 MiB C8 | 1315 / 1523 / 1660 | 677 / 1149 / 1198 |
| Hello W1/C1 | 1831 / 2455 / 3210 | 5966 / 7572 / 10182 |
| Hello W4/C32 | 11011 / 12733 / 14750 | 25462 / 30594 / 32487 |
| JSON W4/C32 | 12179 / 13215 / 16049 | 34120 / 36084 / 37367 |
| CPU SHA256×20000 W4/C16 | 373 / 384 / 422 | 85 / 96 / 102 |
| POST 1 MiB digest W4/C16 | 614 / 792 / 822 | 919 / 1061 / 1063 |

### Independent median latency và tài nguyên — Hamster / Node

| Workload | P50 ms | P95 ms | P99 ms | CPU µs/request | Working set MiB | Private MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| not_found C1 | 0,08 / 0,10 | 0,23 / 0,22 | 0,37 / 0,38 | 59,67 / 79,19 | 15,21 / 100,31 | 3,24 / 51,84 |
| not_found C32 | 0,54 / 1,26 | 1,17 / 1,87 | 1,54 / 2,77 | 75,82 / 44,35 | 16,41 / 102,14 | 4,82 / 54,24 |
| static 23 B C32 | 7,92 / 7,15 | 9,60 / 13,97 | 10,61 / 21,61 | 907,18 / 826,05 | 17,77 / 189,48 | 7,29 / 176,71 |
| static 1 MiB C8 | 4,67 / 6,47 | 8,40 / 13,00 | 10,09 / 23,46 | 4246,93 / 1822,98 | 17,38 / 175,73 | 6,41 / 162,50 |
| Hello W1/C1 | 0,35 / 0,11 | 0,76 / 0,24 | 1,10 / 0,39 | 514,90 / 83,28 | 39,50 / 100,66 | 12,36 / 51,62 |
| Hello W4/C32 | 2,58 / 0,83 | 3,74 / 2,20 | 4,35 / 3,10 | 534,57 / 131,02 | 111,71 / 259,99 | 41,98 / 138,91 |
| JSON W4/C32 | 2,36 / 0,66 | 3,16 / 1,96 | 3,65 / 2,61 | 532,89 / 120,89 | 111,74 / 259,40 | 41,62 / 137,30 |
| CPU W4/C16 | 44,83 / 160,79 | 55,83 / 221,63 | 59,61 / 276,43 | 10648,63 / 41248,09 | 110,29 / 309,76 | 39,55 / 193,60 |
| POST 1 MiB W4/C16 | 19,91 / 13,31 | 24,76 / 28,35 | 31,65 / 37,23 | 9941,93 / 4222,62 | 136,55 / 450,24 | 61,72 / 329,59 |

## Tái lập và artifacts

```powershell
cargo build --release --target x86_64-pc-windows-msvc
python -B benchmarks/node_windows.py `
  --php-dir target/tools/benchmark-20261101/php-nts `
  --oha target/tools/benchmark-20261101/oha.exe `
  --output benchmarks/results/node_windows_20261101.jsonl
```

Output phải là tên mới; driver không overwrite. Port 8080 phải rảnh.
Raw [JSONL](node_windows_20261101.jsonl) và
[metadata](node_windows_20261101.jsonl.meta.json) lưu version/hash binary,
source và full oha summary/status/error/latency, PID+creation identity.
Log từng process ở `node_windows_20261101.jsonl.logs/`; fixture/config/body
scratch thuộc `TemporaryDirectory`, không đọc database/ứng dụng thật.

## INFERENCE — Nhận định và đề xuất

1. **Không có người thắng chung.** Trong phạm vi adapter này, Node nhanh
   hơn Hello W1/C1 (~3,08× median), Hello W4/C32 (~2,40×) và JSON (~2,73×);
   các khoảng RPS không chồng lấp. Đây là evidence đáng profile đường ứng
   dụng Hamster/PHP, chưa phân biệt dispatch, pipe, PHP protocol hoặc runtime.
   Không kết luận HTTP transport vốn chậm từ chênh lệch application đó.
2. 404 C1 và static nhỏ có khoảng throughput chồng lấp. 404 C32 và static
   1 MiB Hamster cao hơn trong cả ba run, nhưng Node chỉ một HTTP worker,
   Hamster dùng Tokio đa luồng. **Không claim thắng tại cùng CPU budget.**
   Static 1 MiB Hamster còn dùng CPU/request cao hơn (~4247 vs 1823 µs):
   throughput và CPU efficiency là hai trade-off khác nhau.
3. CPU chain PHP nhanh hơn ~4× median, nhưng đo cả chi phí gọi `hash()`
   và Node `createHash().update().digest()` 20.000 lần. Không suy thành Rust
   hay PHP nói chung nhanh hơn JavaScript; cũng không xem adapter là Node
   crypto được tune tốt nhất. Hamster process ~629 µs/request, chỉ ~6% CPU
   cả cây; không ưu tiên dispatcher rewrite để cải thiện bài CPU này.
4. POST 1 MiB Node cao hơn throughput (~1061 vs 792), CPU/request thấp hơn
   (~4223 vs 9942 µs), nhưng median P95/P99 lại cao hơn Hamster. Không nói
   Node nhanh hơn mọi percentile. Hamster host ~6798 µs/request, ~68% CPU
   cả cây: tiếp tục profile body assembly/frame/pipe và HTTP IO trước khi
   chọn copy/allocator/channel primitive. Đây không chứng minh một copy
   cụ thể là bottleneck; thay đổi frame đã có sẵn trước pass được giữ nguyên.
5. End-of-run WS/private của Hamster thấp hơn ở các cell này. Node đang
   giữ cluster primary và worker, GC/runtime/Buffer riêng; không claim đây
   là minimum-memory Node hoặc RAM ổn định qua soak. Giữ body/queue budgets
   của Hamster; không bỏ chúng để đạt cấu hình benchmark có ít bảo vệ hơn.

**Đề xuất:** giữ cold-first-request diagnosis ở mục ưu tiên từ assessment
trước; nếu Owner chọn tối ưu tiếp, lấy Hello C1 và POST 1 MiB làm hai workload
profile có control 404. Đo phase cụ thể trong cùng binary/runtime/host,
baseline/candidate xen kẽ ít nhất ba fresh process, rồi mới chọn thay đổi nhỏ.
Chưa có căn cứ thêm Node execution subsystem, redesign worker pool, bỏ IPC
ownership, retry/replay, hoặc thêm dependency vào production.

## HYPOTHESIS / chưa kiểm chứng

Không đo Node standalone, scheduling NONE, Express/Fastify, business/database,
Laravel tương đương, TLS, máy remote, fixed-arrival overload, peak/soak/leak,
hoặc hiệu năng Linux trong pass này. Không ngoại suy các trường hợp đó từ
core-HTTP loopback. Một sample Node không phải tối ưu toàn bộ ecosystem.
Cold Hamster preflight 504 từ lượt Laravel trước vẫn là finding chưa có
nguyên nhân; matrix này không tự chứng minh lỗi đó đã được sửa.

## Verification và cleanup

- PASS: release MSVC build; `cargo fmt --all -- --check`; strict Clippy
  MSVC all-targets/all-features; Windows default suite **317 passed,
  0 failed, 145 ignored**. Protocol subset 8/8 cũng được chạy riêng trước
  matrix. Không gọi 145 ignored là PASS; không chạy lại toàn bộ PHP/lifecycle
  ignored hoặc Cargo bench link gate cho thay đổi driver Python này.
- PASS: 10 Python driver regressions (7 existing + 3 Node/reference/cleanup);
  `node --check` fixture; PHP 8.5.12-1 lint ba heavy adapters/functions;
  `git diff --check`. Release còn warning `compile_validated` unused đã có
  sẵn ở default build; strict all-features Clippy qua, không giấu warning.
- [Cleanup audit](node_windows_20261101.cleanup.json): **294** distinct
  PID/creation identities từ matrix và smoke, không còn live match; không
  còn temp root `hamster-vs-node-*`, port 8080 không listen. Binary và source
  hashes khớp metadata tới cuối lượt. Node worker cleanup dùng retained
  process handles; không terminate từ tên process hoặc snapshot PID.
- Giữ logs/raw/tool cache để tái lập; toàn bộ config/body fixture tạm đã
  cleanup. Không đổi global env, system runtime, Service/firewall, dữ liệu
  `www/` hoặc dịch vụ khác. Không commit/tag/push/merge/release.

Working tree vẫn dirty, gồm thay đổi icon/frame/docs/benchmark có sẵn. Lượt
Node thêm `benchmarks/node_windows.py`, `benchmarks/test_node_windows.py`,
`benchmarks/fixtures/node_http.cjs`, report/raw/metadata/log/audit này; sửa
nhỏ callback measurement helper và cập nhật BENCHMARKS, assessment, handoff.
Không sửa AGENTS, Cargo, Rust production source hay protocol bytes.
