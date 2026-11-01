# Laravel trên Windows: phân tích khoảng cách và đề xuất cải thiện

Ngày: **2026-11-01**. Phạm vi: phân tích số đo/source hiện tại và hai probe
cục bộ để kiểm tra giả định. **Không triển khai tối ưu production**, không
đổi protocol, lifecycle hoặc timeout. Đề xuất chưa phải architecture decision.

## 1. Kết luận

Hamster có khoảng cách cần xử lý ở **Laravel C1**, nhưng dữ liệu chưa chứng
minh thua mọi workload Laravel. Quan trọng hơn, hai bên **khác điểm hoàn tất
HTTP và chính sách kết thúc worker script**. Vì vậy chưa thể quy khoảng cách
RPS trực tiếp thành lỗi dispatcher, IPC hoặc PHP adapter.

Hai probe mới xác nhận: Symfony `Response::send()` trên FrankenPHP có thể
hoàn tất response tới client trước phần xử lý còn lại; Octane script mặc định
kết thúc sau 1.000 request dù engine log `max_requests=0`. Đây là hai điều
kiện phải kiểm soát khi phân tích tiếp. **Chưa lượng hóa được mỗi nguyên nhân
đóng góp bao nhiêu vào khoảng cách hiện tại.** Cold Laravel 504 vẫn chưa sửa.

## 2. FACT — Hamster đang thua ở đâu?

Nguồn: [báo cáo current-binary](../benchmarks/results/frankenphp_windows_20261101_current.md),
[raw Laravel](../benchmarks/results/frankenphp_windows_20261101_current_remaining.jsonl).
Windows i5-1235U, Hamster release `D1E2...`, PHP 8.5.12-1 NTS; FrankenPHP
1.13.4/PHP 8.5.12 ZTS, Laravel 12.69.2/Octane 2.20.0. Ba fresh process/arm,
warm-up 1.000 request, measured 10 s, H F F H H F. Máy dùng chung.

| Cell | Hamster min/median/max RPS | FrankenPHP min/median/max RPS | Chênh median H/F |
| --- | ---: | ---: | ---: |
| `/login` W4/C1 | 254 / 274 / 299 | 267 / 322 / 423 | −15,0% |
| `/login` W4/C16 | 1308 / 1644 / 1697 | 1102 / 1119 / 1200 | +47,0% |
| Welcome W4/C1 | 313 / 423 / 435 | 345 / 441 / 445 | −4,1% |
| Welcome W4/C16 | 828 / 884 / 1140 | 774 / 930 / 1067 | −4,9% |

Ba cell có median thấp hơn đều có **spread chồng lấp**. `/login` C1 là tín
hiệu đáng điều tra hơn chênh ~4–5% của Welcome; không gọi các tỷ lệ này là
regression đã xác lập hoặc SLO. `/login` là placeholder 17 B, Welcome 81.277 B
Blade HTML; không authentication/database/business qualification.

Ở C1, `1000 / median RPS` xấp xỉ chu kỳ serialized request: `/login`
**3,65 / 3,10 ms**, khoảng cách ~0,55 ms; Welcome **2,36 / 2,27 ms**, ~0,10 ms.
Đó không phải số đo riêng PHP hoặc IPC. Không dùng công thức này làm service
time ở C16, nơi nhiều request/queue chạy đồng thời. Không trừ median Hello
khỏi Laravel để suy ra chi phí framework: adapter, INI, workload và thời điểm
đo khác nhau.

### CPU, latency và bộ nhớ không cùng kể một câu chuyện

| Cell | CPU toàn cây H/F µs/request | CPU host Hamster µs/request | P99 H/F ms |
| --- | ---: | ---: | ---: |
| `/login` C1 | 3661 / 3319 | 762 | 6,93 / 7,37 |
| `/login` C16 | 2686 / 4603 | 458 | 19,68 / 19,62 |
| Welcome C1 | 2313 / 2381 | 373 | 4,09 / 3,62 |
| Welcome C16 | 4995 / 5269 | 904 | 24,51 / 26,87 |

Independent medians, không phải mọi metric của median-RPS run. CPU host là
toàn process Rust, **không phải IPC timing**. Không so host riêng với cả
FrankenPHP process để kết luận efficiency. Welcome có RPS thấp hơn nhưng
CPU/request thấp hơn trong mẫu này; chưa đủ bằng chứng cho câu chuyện
"thiếu CPU". H summed WS framework ~250–274 MiB cao hơn F ~113–136 MiB,
nhưng private bytes thấp hơn; chưa đo unique physical RAM hoặc leak.

## 3. FACT — Đường xử lý khác nhau ở đâu?

### Hamster

```text
HTTP/body admission -> bounded queue -> dispatcher -> worker-owned transaction
-> REQUEST pipe -> decode + dựng Illuminate Request -> Octane Worker::handle
-> stage response frame -> Octane terminate/sandbox cleanup
-> Hamster external-resource cleanup -> flush RESPONSE
-> Rust read/decode/validate response -> HTTP client
```

Source:
[Laravel worker](../fixtures/php-worker/laravel_worker.php),
[client adapter](../fixtures/php-worker/laravel_octane_client.php),
[external cleanup](../fixtures/php-worker/laravel_lifecycle.php),
[HttpFoundation mapping](../fixtures/php-worker/httpfoundation.php),
[Rust transaction](../src/php_worker.rs).

`HamsterOctaneClient::respond()` encode/stage; `markCleanupCompleted()` và
`flushResponse()` chỉ chạy sau khi `Worker::handle()` cùng cleanup trả về.
Một failure trước commit có thể làm generation unavailable thay vì công bố
response thành công rồi mới phát hiện cleanup lỗi. Đây là hợp đồng correctness.
Laravel đã boot một lần/process; không có full application bootstrap mỗi request.

Chi tiết cần giữ khi profile: `flushResponse()` nằm trong `try`; `finally`
unwind output buffers, discard uploads lần nữa và bỏ references sau đó.
Boundary nói ở đây là Octane termination/sandbox cùng `completeRequest()` và
upload cleanup trước gate, không phải khẳng định PHP không còn bất kỳ work
nào sau write hoặc đã quay lại chờ input. Đo thêm guard/finally tail nếu cần.

### FrankenPHP + Octane

```text
HTTP -> native PHP SAPI -> Request::capture() -> cùng Octane Worker::handle
-> FrankenPhpClient::respond -> Symfony Response::send -> HTTP response
-> Octane request callbacks/terminate/sandbox cleanup -> callback kết thúc
```

Source đã đọc trong installed vendor:
`vendor/laravel/octane/src/FrankenPhp/FrankenPhpClient.php`,
`vendor/laravel/octane/src/Worker.php`,
`vendor/symfony/http-foundation/Response.php` trong `www/laravel`.
Đối chiếu [Octane v2.20.0 client upstream](https://raw.githubusercontent.com/laravel/octane/v2.20.0/src/FrankenPhp/FrankenPhpClient.php).
`respond()` gọi `send()` trước gateway termination; runtime có
`fastcgi_finish_request()`, nhánh Symfony sử dụng để hoàn tất response.

### Probe A — Response tới client trước callback tail: CONFIRMED

[Raw probes](../benchmarks/results/laravel_windows_analysis_20261101_probes.json),
[archived source](../benchmarks/results/laravel_windows_analysis_20261101_probes.py).
Một FrankenPHP worker, Symfony Response từ đúng vendor, không thêm thủ công
Content-Length. Sau `send()`, PHP ghi marker rồi giữ callback tại release gate.
Client nhận **đủ HTTP 200/body `semantic probe` khi gate chưa mở**; callback
tail chỉ hoàn tất sau khi parent tạo release marker. PID **27148**, SAPI
`frankenphp`, cả hai hàm finish-request hiện diện. Caddy tự trả Content-Length 14.

**INFERENCE:** ở C1, client có thể gửi request tiếp theo trong khi worker
FrankenPHP trước còn làm tail; các worker khác có thể xử lý request đó.
Hamster C1 đợi các cleanup trước gate kể trên trước khi client thấy response
hoàn tất. Khác biệt này
có khả năng ảnh hưởng latency/closed-loop RPS mà không đồng nghĩa framework
PHP thực thi chậm hơn. Worker đã gửi response không tự nhiên trở thành rảnh
trong lúc callback vẫn chạy. Probe chứng minh boundary, **chưa đo natural
Octane tail hoặc tỷ lệ 0,55 ms có thể giải thích**; không chứng minh mọi
request Laravel đều bị chi phối bởi khác biệt này.

## 4. FACT mới — Recycle policy chưa được ghim trong benchmark

[Octane v2.20.0 worker script](https://raw.githubusercontent.com/laravel/octane/v2.20.0/bin/frankenphp-worker.php)
đọc `MAX_REQUESTS` từ environment, fallback **1000**, và lặp khi
`$requestCount < $maxRequests`. Driver `LARAVEL_ENV` chưa đặt biến này;
Hamster config benchmark không bật `max_requests`/`max_worker_age`.

### Probe B — Script thực sự kết thúc ở 1.000 request: CONFIRMED

Dùng clone Laravel mới, cùng driver config W4, `MAX_REQUESTS` không có trong
child environment, 4.200 GET `/login` tại C4: **4.200 HTTP 200, zero transport
errors, 8 start events, 4 end events**, mỗi end ghi handled=1000/limit=1000.
FrankenPHP PID **25772** không đổi; token bootstrap riêng phân biệt các lần
script chạy lại. Đây là correctness probe, không dùng RPS/latency của nó làm
performance result.

Engine log `max_requests=0` và cap của **Octane PHP script** là hai control
khác nhau. Probe này chứng minh engine không recycle theo cap đó vẫn có thể
khởi động lại application script. Dữ liệu benchmark cũ không lưu effective
MAX_REQUESTS hoặc bootstrap-token counts; **không gán ngược số lần recycle
chính xác** cho từng historical row. Các số cũ vẫn là observed results của
cấu hình đã chạy, chưa phải so adapter cost ở cùng generation lifetime.

**INFERENCE:** recycle có thể thêm bootstrap/GC/tail variance phía F; bản
thân finding này không chứng minh nguyên nhân H thấp RPS hơn, và còn có thể
làm F chậm hơn. Cần controlled measurement trước khi phân bổ nguyên nhân.

**Correction đo lường đề xuất:** baseline H giữ recycle OFF; F benchmark
script đặt cap dương đủ lớn hơn tổng warm-up/load, ví dụ
`MAX_REQUESTS=1000000000`, ghi effective cap và bootstrap tokens trước/sau.
Đây là cap thực nghiệm, không phải production lifecycle recommendation.
**Không đặt MAX_REQUESTS=0** cho Octane script này: điều kiện loop sẽ sai ngay;
không đồng nghĩa recycle OFF như engine `max_requests=0`.
Chưa sửa driver để áp dụng thay đổi đó trong pass phân tích.

## 5. Chi phí tích hợp có thật, mức đóng góp hiện tại chưa được đo

| Vùng | Source evidence | Phép đo phân biệt |
| --- | --- | --- |
| Transport/process boundary | H qua pipe + actor/channel; F native SAPI | Queue wait, write/read, decode, HTTP completion tách riêng |
| Request reconstruction | `hamster_request_parts` + `hamster_httpfoundation_request`; F capture từ SAPI | Timestamp bounded samples trước/sau mapping; cùng headers/body |
| Octane/Laravel | Cả hai dùng Worker clone/sandbox; gateway/middleware | Prepare, kernel, response stage/send, terminate, sandbox tail riêng |
| Extra cleanup H | DB rollback khi có transaction, scoped/static/cwd/timezone restoration | Warm cleanup distribution; không tắt cleanup để lấy số |
| Response payload | PHP pack/concat frame; Rust `read_frame` cấp buffer, `decode_response` `.to_vec()` body | Payload ladder cùng middleware; 17 B/4 KiB/64 KiB/81.277 B |

`http_response` dùng `Bytes::from(worker.body)` chuyển ownership; không gán
thêm một body copy ở bước này. Không mặc định toàn bộ ~81 KiB Welcome
chậm do copy: `/login` 17 B có khoảng cách C1 lớn hơn trong dữ liệu hiện tại,
và các cell không chạy đồng thời để suy causal cost từ chênh lệch đó.

Đã có [Gate 2 profiler/results](research/PERSISTENT_WORKER_COST_GATE_2_RESULTS.md)
và [sampled helper](../benchmarks/support/laravel_gate_2_profile.php). Gate 2
lịch sử ghi direct PHP adapter ~4–5% PHP time, phần lớn ở Laravel/Octane;
không ủng hộ mặc định quy residual cho adapter. Workload/runtime/binary khác
hiện tại, không dùng các microsecond cũ để trừ vào bảng mới. Tái sử dụng
phương pháp, không tạo profiling subsystem mới.

## 6. Cold 504 là nhánh nguyên nhân riêng, cần ưu tiên

Preflight thật đã trả **504 ở request deadline 5 s sau READY**. Phase probe
cũ trên scratch app ghi request đầu 3725,75 ms, gateway 3505,56 ms và thêm
120 included files; process mới dùng lại app còn 133,40/73,54 ms. Probe đó
không tự tái hiện 504. Chưa phân biệt autoload, filesystem, OPcache, antivirus,
middleware/crypto initialization hoặc scheduling.

Startup 30 s đã thành công; không giải thích request deadline bằng tăng
startup budget. Cold load và warm throughput phải báo riêng. Không tăng
request timeout hoặc prewarm route ngầm để đóng finding. Chưa đề xuất
production readiness/prewarm policy khi chưa biết nguyên nhân và side effect.

## 7. Đề xuất next slice nhỏ nhất

### A. Ghim hợp đồng đo và profile hai bên

1. Giữ current release/PHP/INI/app/config/view caches, body/queue budgets;
   ghi effective OPcache/JIT, worker/spare threads, cap và bootstrap tokens.
   Tách default-recycle control khỏi large-cap control; không xóa historical data.
2. Profile `/login`, Welcome ở **W1/C1, W4/C1, W4/C16**. W1/C1 giúp phân biệt
   hiệu ứng tail overlap qua nhiều worker với direct request-path overhead.
   Không gọi callback-return là worker READY chính xác nếu còn native reset.
3. Tái sử dụng Gate 2: frame/decode/build, Octane prepare/kernel, response
   stage/send, terminate/tail, external cleanup, flush; bổ sung Rust queue/
   pipe/decode/HTTP boundaries trong measurement-only build khi cần.
4. Timestamp sampling vào bộ nhớ, dump **sau** resource window; kiểm tra
   observer effect với uninstrumented control. PHP durations và CPU budgets
   không được cộng độc lập vào client latency như một causal decomposition.
5. Mỗi throughput point ít nhất ba fresh process, xen kẽ; warm-up rõ ràng,
   run đủ dài, RPS min/median/max, independent P50/P95/P99/CPU/RAM; báo worker
   replacement/recycle, errors, body checks và thời điểm response/cleanup.

### B. Chỉ chọn một candidate sau profile

| Nếu bằng chứng chỉ ra | Candidate nhỏ để thử | Điều kiện không được đổi |
| --- | --- | --- |
| Rust response decode allocation/copy đáng kể | Reuse ownership buffer/body slice ở decoder; protocol wire giữ nguyên | Bounds/IDs/header validation; tính cả retention của full frame với header-heavy response/slow client, không claim zero-cost |
| PHP response packing chiếm phần đáng kể | Giảm intermediate concat hoặc chia các phần ghi cùng wire frame | Toàn frame chỉ commit sau cleanup; partial write/failure vẫn deterministic |
| Request mapping/cleanup chiếm phần đáng kể | Giảm allocation/work lặp mà vẫn cùng kết quả và reset contract | Không bỏ hydration/cleanup hoặc đổi authority/metadata semantics |
| Laravel/Octane kernel/middleware hoặc cold first-use chiếm phần lớn | Narrow app/runtime/config investigation trên scratch, đối chiếu hai engine | Không bỏ web middleware production hoặc coi framework-specific tuning là server fix |
| Chênh chỉ do boundary/recycle/drift | Ghi kết luận measurement và giữ code hiện tại | Không redesign pool hoặc đổi điểm commit để thắng bảng |

Chưa có evidence chọn candidate production nào. Natural cleanup tail có
thể giải thích một phần C1 nhưng không thể giả định việc bỏ nó sẽ an toàn;
response-gating semantics được giữ nguyên.

## 8. Acceptance, non-goals và bàn giao

Mục tiêu là **giảm chi phí đã đo và cold failure có nguyên nhân**, không đặt
SLO theo một median trên máy dùng chung hoặc yêu cầu thắng mọi framework.
Chỉ nhận candidate khi lợi ích vượt drift/observer effect, không che lỗi,
không regression tail/CPU/RAM thiếu justification, và meaningful correctness
regressions qua. Body/request/header limits, cancellation-safe transaction,
at-most-once, isolation, recovery/shutdown và staged response phải được giữ.

Không đổi protocol, không streaming/multiplex redesign, native embedded PHP,
global execution lock, adaptive pool hoặc bỏ cleanup/Octane listeners. Không
mở lại authority/metadata, Service, TLS, reverse proxy hoặc tối ưu Linux.
Không giả định OPcache/cache chưa được bật: cả hai framework arm đã cấu hình
OPcache và config/view caches; cần ghi effective runtime trước khi đề xuất.

Pass này bổ sung file phân tích và evidence, không sửa production hoặc
vendor gốc. Hai probe local trên port 18080 đều kết thúc; bốn PID/creation
identities đã gone, temp app/markers/config xóa, port rảnh; input hashes
không đổi. [Cleanup audit](../benchmarks/results/laravel_windows_analysis_20261101_cleanup.json).
Archival source/raw giữ để review; logs trong ignored `target/` cache.
Không có throughput matrix mới, không chạy lại full Rust gates chỉ cho
thay đổi tài liệu; không commit/tag/push/merge/release, không đo Linux.
