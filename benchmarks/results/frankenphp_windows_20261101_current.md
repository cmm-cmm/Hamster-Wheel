# Windows: kiểm tra lại FrankenPHP với release Hamster hiện tại

Ngày đo: **2026-11-01**. Bộ kết quả riêng, không thay thế hoặc sửa dữ liệu
[FrankenPHP trước](frankenphp_windows_20261101.md) và
[Laravel/heavy trước](frankenphp_windows_20261101_laravel_heavy.md).

## FACT — Artifact và phạm vi

Windows 11 10.0.26300, Intel i5-1235U, 10 core/12 logical CPU, ~16 GB RAM,
máy dùng chung. Loopback HTTP/1.1 keep-alive, không TLS/access log. Không
thay Service, firewall, global env, hệ thống PHP hoặc dữ liệu ứng dụng thật.

- Hamster 0.8.0 MSVC **release**, SHA-256
  `D1E2B14CF48611844762B5C1B3CC6D13EEAD8DA0D87825A6803071FC63F74163`.
  Trùng binary của [Node pass](node_windows_20261101.md), khác binary
  `AF03...` của FrankenPHP baseline cũ. Không sửa Rust production trong pass.
- FrankenPHP **1.13.4 / PHP 8.5.12 ZTS / Caddy 2.11.7**, EXE SHA-256
  `38533C7D4A3E86EF01E8A704244F826F9610664484CC75EB44E1631F4EFB8620`.
  PHP TS DLL SHA-256
  `2CAD79B8ACA7CCB8051D24E76EE394A1BF10F6F1FB0A0698EC570F4E6F4CBD5B`.
  Dùng cache package đã đối chiếu checksum chính thức ở lượt trước; không
  tự cập nhật bản mới hoặc thay binary đang vận hành.
- Hamster dùng PHP **8.5.12-1 NTS** cô lập; oha **1.16.0**. Toàn bộ hashes,
  source/runtime identities và cell selection trong
  [metadata](frankenphp_windows_20261101_current.jsonl.meta.json).
- Metadata ghi HEAD `9519e18`. HEAD đã được cập nhật bên ngoài lượt agent
  trong lúc chuẩn bị; agent không thực hiện commit/push. Binary đã build
  trước cập nhật Git này; source worker/protocol digests vẫn trùng Node pass.
  Nhận dạng phép đo bằng SHA binary và source, không chỉ HEAD/build stamp.

## Phương pháp

**13 cell khai báo trước, ba process mới mỗi bên, H F F H H F.** Warm-up
1.000 request tại min(C, 8), rồi 10 giây gửi với `oha -w` chờ request đang
chạy hoàn tất. Không chạy Cargo build/test hoặc benchmark khác song song.
Không lặp classic FastCGI, W4/C4 hay C64/C128 overload trong matrix này.

| Nhóm | Cell |
| --- | --- |
| HTTP | not_found C1, C32; chỉ yêu cầu đúng status 404, body lỗi khác nhau |
| Static | `/` file thực **23 B** CRLF tại C32; `/1m.bin` 1 MiB tại C8 |
| Persistent PHP | Hello W1/C1, Hello W4/C32, JSON W4/C32 |
| Framework | Laravel `/login` placeholder và Welcome, W4/C1 và W4/C16 |
| Heavy | SHA256×20000 W4/C16; POST 1 MiB digest W4/C16 |

Raw giữ tên nominal cũ `static 22 B`; file thực là 23 B. Static 1 MiB là
cùng file random trong scratch cho cả hai server. Hello cùng 14 B, JSON cùng
object/bytes. Heavy dùng chung pure function PHP: 20.000 SHA-256 hex từ
64 ASCII a; POST đúng 1 MiB ASCII x trả digest, Python reference độc lập.

Driver bổ sung cell JSON W4/C32, `--cell` exact name có thể lặp, từ chối tên
sai trước khi launch, và pre/post body checks cho static/worker/classic.
Không đổi timed load, admission, timeout hoặc computation để cải thiện số.
Baseline 404 kiểm status; static/Hello/JSON/heavy kiểm bytes chính xác;
Laravel `/login` kiểm placeholder, Welcome kiểm signature `Laravel`, không
phải đối chiếu toàn bộ HTML hoặc body từng response trong oha.

### PHP và Laravel

Giữ `workers=1/4`, `queue_capacity=64` cùng body/aggregate budgets của
Hamster. FrankenPHP `worker.num=W`, `num_threads=W+1` như baseline cũ.
Theo [config 1.13.4](https://raw.githubusercontent.com/php/frankenphp/v1.13.4/docs/config.md),
`num_threads` là non-worker threads, worker threads thêm vào. Do đó W4 có
bốn application workers **và năm spare classic threads**, không phải năm
PHP threads tổng cộng; RAM gồm cả spare threads. Không claim best tuning.

Laravel **12.69.2 / Octane 2.20.0**, clone skeleton tin cậy `www/laravel`
vào temp, không copy `.env`, storage/database/cache cũ. Chỉ giữ AppServiceProvider
đã kiểm tra không có side effect; tạo key, routes, config/view caches mới.
Session/cache array, queue sync, SQLite `:memory:`, DB_URL rỗng, không dùng
PMS/MySQL/Redis. Cả hai bật OPcache cho framework; simple/heavy dùng INI
tối thiểu riêng. `php.startup_timeout=30` cho framework, không tăng execution
timeout. Welcome là Blade HTML; `/login` là placeholder 17 B, không auth.

CPU user+kernel cả cây / request thực tế, không tính oha. WS/private lấy cuối
run, independent median mỗi metric; WS có thể đếm shared pages nhiều lần,
private là commit, không phải peak hoặc memory-soak. Whole-tree gồm hidden
console descendants khi có. Không dùng median-RPS run đại diện mọi metric.

## FACT — Kết quả

**78 timed runs hoàn tất, 5.125.180 request đúng status dự kiến:**
1.958.515 HTTP 200 và 3.166.665 HTTP 404; zero transport/unexpected status
errors trong phần đo, không có 503; 78 pre/post response checks qua.
Elapsed từng measured run **10,002–10,067 s**. **Có thêm một preflight 504**
ngoài phần đo, mô tả bên dưới; vì vậy đây là steady-state data, không phải
qualification cold-start availability đã PASS. Không xóa timed run hoặc
đổi kết quả lỗi thành một lượt đo thành công.

### RPS min / median / max

H/F là tỷ lệ median cùng cell; không phải scaling efficiency.

| Workload | Hamster | FrankenPHP | H/F |
| --- | ---: | ---: | ---: |
| 404 C1 | 9471 / 11050 / 11720 | 3681 / 3826 / 4333 | 2,89 |
| 404 C32 | 45194 / 47299 / 48505 | 39512 / 45840 / 45912 | 1,03 |
| Static 23 B C32 | 3995 / 4097 / 4858 | 3958 / 4333 / 4370 | 0,95 |
| Static 1 MiB C8 | 1958 / 2074 / 2088 | 1161 / 1324 / 1345 | 1,57 |
| Hello W1/C1 | 2095 / 2396 / 2714 | 1156 / 1234 / 1599 | 1,94 |
| Hello W4/C32 | 14179 / 14331 / 14369 | 6387 / 7290 / 7798 | 1,97 |
| JSON W4/C32 | 12480 / 14484 / 15776 | 5662 / 5672 / 6050 | 2,55 |
| Laravel /login W4/C1 | 254 / 274 / 299 | 267 / 322 / 423 | 0,85 |
| Laravel /login W4/C16 | 1308 / 1644 / 1697 | 1102 / 1119 / 1200 | 1,47 |
| Laravel Welcome W4/C1 | 313 / 423 / 435 | 345 / 441 / 445 | 0,96 |
| Laravel Welcome W4/C16 | 828 / 884 / 1140 | 774 / 930 / 1067 | 0,95 |
| SHA256×20000 W4/C16 | 450 / 456 / 542 | 395 / 404 / 466 | 1,13 |
| POST 1 MiB digest W4/C16 | 824 / 897 / 930 | 324 / 343 / 355 | 2,62 |

### Independent median latency/tài nguyên — Hamster / FrankenPHP

| Workload | P50 ms | P95 ms | P99 ms | CPU µs/request | WS MiB | Private MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 404 C1 | 0,06 / 0,21 | 0,22 / 0,50 | 0,33 / 0,70 | 53,14 / 299,97 | 15,71 / 53,21 | 3,23 / 68,77 |
| 404 C32 | 0,62 / 0,41 | 1,26 / 2,16 | 1,61 / 4,88 | 84,39 / 119,98 | 17,20 / 65,92 | 5,24 / 79,89 |
| Static 23 B C32 | 7,90 / 7,50 | 9,07 / 11,17 | 9,93 / 16,96 | 914,81 / 1202,88 | 18,70 / 63,72 | 8,07 / 78,54 |
| Static 1 MiB C8 | 4,04 / 5,55 | 5,41 / 10,63 | 6,13 / 12,69 | 3300,44 / 4310,02 | 17,72 / 64,52 | 5,86 / 78,81 |
| Hello W1/C1 | 0,39 / 0,82 | 0,69 / 1,35 | 0,90 / 1,82 | 511,23 / 856,88 | 40,52 / 64,57 | 12,36 / 81,61 |
| Hello W4/C32 | 2,33 / 3,62 | 3,35 / 10,16 | 3,85 / 13,85 | 449,26 / 930,69 | 114,12 / 73,84 | 41,71 / 102,79 |
| JSON W4/C32 | 2,31 / 4,89 | 3,29 / 11,81 | 3,77 / 15,18 | 452,29 / 1184,58 | 113,97 / 73,23 | 41,18 / 102,31 |
| Laravel /login C1 | 3,36 / 2,83 | 5,45 / 6,00 | 6,93 / 7,37 | 3661,04 / 3318,80 | 249,84 / 117,58 | 91,68 / 121,91 |
| Laravel /login C16 | 7,64 / 14,06 | 16,34 / 17,64 | 19,68 / 19,62 | 2686,21 / 4602,89 | 256,54 / 112,58 | 98,04 / 115,57 |
| Laravel Welcome C1 | 2,27 / 2,17 | 2,71 / 2,65 | 4,09 / 3,62 | 2313,31 / 2381,15 | 270,34 / 126,57 | 110,62 / 131,70 |
| Laravel Welcome C16 | 18,63 / 18,65 | 22,72 / 24,35 | 24,51 / 26,87 | 4995,14 / 5268,64 | 274,49 / 136,33 | 112,09 / 143,61 |
| SHA256×20000 | 27,58 / 44,10 | 53,41 / 56,73 | 58,66 / 61,26 | 9029,75 / 10873,25 | 112,70 / 69,24 | 39,67 / 98,98 |
| POST 1 MiB | 18,32 / 32,55 | 22,91 / 70,15 | 26,58 / 133,45 | 8477,23 / 9407,33 | 139,20 / 76,35 | 62,06 / 102,09 |

### CPU tổng tại tải đã đo

Median của `(CPU seconds cả cây) / measured elapsed` từng run; **1,00**
tương đương 100% một logical CPU. Không pin affinity hoặc đặt CPU quota.
CPU/request thấp hơn không nhất thiết có nghĩa tổng CPU thấp hơn khi RPS khác.

| Cell | Hamster CPU-equivalent | FrankenPHP CPU-equivalent |
| --- | ---: | ---: |
| Hello W1/C1 | 1,18 | 1,12 |
| Hello W4/C32 | 6,46 | 6,54 |
| JSON W4/C32 | 6,68 | 6,71 |
| SHA256×20000 | 4,12 | 4,40 |
| POST 1 MiB | 7,60 | 3,34 |
| Laravel Welcome C16 | 4,45 | 4,88 |

POST tận dụng tổng CPU lớn hơn đáng kể ở Hamster; không mô tả throughput
2,62× thành CPU efficiency 2,62× hoặc thấp hơn tổng CPU của FrankenPHP.

## FACT — Cold Laravel 504 và phase probe riêng

Attempt đầu hoàn tất **42 measured rows** (bảy cell), rồi **Hamster preflight
`/login` trả 504**, trước warm-up/timed rows của Laravel. Không có request
replay hoặc tự thay timeout. Log:
`target/tools/benchmark-20261101/franken-current.logs/hamster-wheel.exe-a7iwr0gu.log`.
Bốn worker READY sau ~3092 ms; slot 0 generation 1 (PHP PID **24196**) hết
transaction deadline **5000 ms**, được đánh unavailable và lên lịch generation
2. Driver sau đó dừng đúng subprocess đã launch. Không có timed Laravel row
của attempt lỗi; không được gọi cả lượt qualification là không lỗi.

[Phase-probe evidence](frankenphp_windows_20261101_cold_probe.json) là thí
nghiệm **không dùng làm throughput data**: clone Laravel mới, thêm STDERR
timestamps vào bản scratch adapter và scratch Octane Worker; ba cây Hamster
mới lần lượt dùng **cùng scratch app**, mỗi cây đúng một GET khác query marker.
Giữ `request_timeout=5s`, không retry request thất bại, không sửa source thật.
Probe source được giữ tại
`target/tools/benchmark-20261101/diagnose_laravel_cold.py`; temp đã cleanup.

| Cycle | Parent / PHP xử lý PID | HTTP status | HTTP elapsed | Trong Octane gateway |
| --- | --- | ---: | ---: | ---: |
| 1 | 26596 / 25468 | 200 | 3725,75 ms | 3505,56 ms |
| 2 | 17752 / 26796 | 200 | 133,40 ms | 120,87 ms |
| 3 | 20392 / 23440 | 200 | 73,54 ms | 48,63 ms |

Mỗi process đều tăng từ 443 lên 563 included files qua gateway. Cycle 1
nhận/decode frame và build request trong <1 ms sau marker nhận frame;
flush RESPONSE khoảng 0,04 ms. Phần gần hết HTTP elapsed nằm trong PHP
handle/lifecycle, không phải một stall 3,7 s ở write/read pipe trong probe.
**Probe không tự tái hiện 504**, nhưng cho thấy request đầu sau READY vẫn có
thể tốn nhiều giây ở framework; bootstrap READY không có nghĩa mọi class
phục vụ request đã warm. Chưa tách filesystem/OPcache/CPU/antivirus, chưa
chứng minh nguyên nhân của toàn bộ 5 s trong attempt gốc hoặc đóng finding.
Instrumentation có thể ảnh hưởng timing; không dùng probe để đặt SLO hay
chứng minh upper bound của cold latency.

Các cell còn thiếu chạy như **continuation độc lập** vào
[remaining JSONL](frankenphp_windows_20261101_current_remaining.jsonl) với
[metadata riêng](frankenphp_windows_20261101_current_remaining.jsonl.meta.json).
Clone app mới, process mới; không xóa lỗi gốc hoặc retry ngầm một transaction.
Hai file measured rows phải được báo cùng finding preflight khi diễn giải.

## INFERENCE — Đánh giá và mốc cải thiện

1. **Giữ ưu thế worker đơn giản nhưng không ngoại suy framework.** Các
   khoảng RPS Hello W1/C1, W4/C32, JSON W4/C32 và POST không chồng lấp:
   Hamster cao hơn trong cấu hình này, với CPU/request và tail latency thấp
   hơn. Không từ đó kết luận thắng mọi ứng dụng hoặc phiên bản FrankenPHP.
2. **Static nhỏ và 404 C32 chưa có winner ổn định về RPS.** Spread chồng
   lấp; chênh median ~3–5% chưa đủ để mở một dự án tối ưu. Static 1 MiB
   Hamster cao hơn cả ba run, với CPU/request thấp hơn trong cặp này; kết
   quả không thay thế static correctness/containment qualification.
3. **Laravel cần đánh giá riêng từng endpoint và concurrency.** `/login`
   C16 nghiêng rõ về Hamster, nhưng C1 có median thấp hơn và spread còn
   chồng lấp. Welcome C1/C16 cũng chồng lấp; không có cơ sở tuyên bố framework
   superiority. Median P99 `/login` C16 gần ngang (19,68/19,62 ms) dù RPS
   khác; throughput cao hơn không đồng nghĩa mọi percentile đều tốt hơn.
4. **CPU-heavy không phải lý do ưu tiên dispatcher rewrite.** RPS có
   overlap; host Hamster ~596 µs/request, ~6,6% CPU cả cây. Hai runtime là
   NTS và ZTS cùng base PHP, không phải cùng process/thread architecture.
   POST có host ~5672 µs/request, ~67% CPU: vẫn đáng profile body/frame/pipe,
   nhưng chưa chứng minh syscall/copy cụ thể. Giữ aggregate body budget.
5. **RAM là trade-off.** W4/framework Hamster có summed WS cao hơn, private
   bytes thấp hơn FrankenPHP trong các cell này; không gọi Hamster ít RAM
   hơn nói chung. Spare classic threads và shared-page accounting phải giữ
   trong diễn giải; chưa có peak/soak hay minimum-memory tuning.

**Mục tiêu tiếp theo đề xuất:** ưu tiên giải thích cold request sau READY
trước tối ưu RPS. Thêm phase evidence sâu hơn trong gateway của scratch app
để phân biệt autoload/file IO, middleware, crypto/runtime và scheduling.
Không tăng timeout để biến failing preflight thành PASS, không bỏ cleanup
Octane hoặc prewarm route ngầm. Startup timeout 30 s và request timeout 5 s
là hai hợp đồng khác nhau: attempt lỗi đã READY, nên tăng startup budget
không giải thích được request deadline. Chưa đề xuất production fix từ một
probe đã instrument; sau khi có nguyên nhân mới chọn correction nhỏ nhất.

Nếu chọn tối ưu tiếp, dùng Hello C1 và POST 1 MiB làm workload, giữ control
404, baseline/candidate cùng đợt ít nhất ba fresh process, cùng runtime/INI/
admission; báo CPU/RAM/tail trade-off và không replay transaction có thể đã
thực thi. Không lấy bảng FrankenPHP cũ khác binary làm baseline cải thiện.

## HYPOTHESIS / giới hạn

Không dùng chênh lệch giữa pass này và bảng `AF03...` cũ để gán lợi ích
riêng cho thay đổi PHP frame: không có baseline/candidate A-B cùng đợt.
Mặc dù cùng SHA Hamster với Node, hai matrix khác thời điểm, fixture static
1 MiB khác bytes và topology/policy khác; không coi là phép đo ba bên xen kẽ.

Máy dùng chung, ba run không chứng minh production SLO. Không kiểm chứng
TLS/remote/business/database, overload fixed-arrival, cancellation/recovery
fault injection, memory peak/leak hoặc Linux trong pass này. Finding cold
Laravel preflight 504 từ lượt cũ đã tái hiện trong pass này, vẫn chưa đóng;
steady-state success không thay thế cold-request investigation.
Không mở tối ưu dispatcher, protocol, lifecycle hay tự quản lý php-cgi.

## Tái lập và verification

Các arguments đầy đủ trong metadata; output phải là tên mới. Ví dụ chọn
hai cell (vẫn ba process mỗi bên), build release trước, port 8080 rảnh:

```powershell
python -B benchmarks/frankenphp_windows.py `
  --frankenphp target/tools/benchmark-20261101/frankenphp/frankenphp.exe `
  --php-dir target/tools/benchmark-20261101/php-nts `
  --oha target/tools/benchmark-20261101/oha.exe `
  --duration 10 --output target/franken-new.jsonl `
  --cell "worker hello W1 C1" --cell "worker json W4 C32" worker
```

Raw: [JSONL](frankenphp_windows_20261101_current.jsonl). Logs giữ trong
`target/tools/benchmark-20261101/franken-current.logs/`; orchestration log
`franken-current-console.log` cùng tool-cache root. Không có automatic retry
sau một request có thể đã thực thi. Cleanup chỉ dùng subprocess handles
được giữ từ launch; không process-name kill hoặc PID snapshot termination.

| Kiểm chứng | Kết quả |
| --- | --- |
| 13 cell steady-state, ba run/arm | PASS: 78 measured rows; 5.125.180 response đúng status dự kiến |
| Cold Laravel preflight | **FAIL: một 504**, không có timed row của attempt đó |
| Scratch phase probe | PARTIAL: định vị gateway chậm trong probe; ba 200, không tái hiện 504 ở chính probe, root cause chưa đóng |
| Tooling | PASS: release build, fmt check, 13 Python driver regressions, PHP fixture lint, diff check |
| Rust regressions | Reuse Node-pass 317 passed / 145 ignored và strict MSVC all-target/all-feature Clippy trên production content không đổi; không gọi là rerun trong pass này |
| Ignored PHP/lifecycle, Cargo bench-link, Linux | Không chạy lại trong pass này |
| Cleanup | PASS theo audit bên dưới |

[Cleanup audit](frankenphp_windows_20261101_current.cleanup.json): **384**
distinct PID/creation identities từ timed matrix và probe không còn live
match; bốn PHP PID ghi trong failed log đều không còn tồn tại; zero process
dùng cached PHP executable, zero owned temp roots, zero listener ở các port
benchmark. Parent PID của attempt lỗi không được lưu trước exception; đây
là giới hạn evidence, không dùng PID từ log để terminate. `kill/wait` của
subprocess teardown đã hoàn tất; không có fallback kill theo tên/PID snapshot.

Tất cả binary/source digests ghi trong hai metadata còn khớp sau kiểm thử,
bao gồm original Laravel inputs. Giữ raw, probe evidence, log và scratch
probe source trong ignored cache để tái lập; thư mục app/config/INI/body tạm
đã xóa. Không thay Service/firewall/data/global env. Không commit/tag/push/
merge/release bằng agent; HEAD ngoài lượt agent đã được ghi rõ ở trên.

Version probe `frankenphp php-cli -v` trả lỗi unsupported với PHP 8.5 ở
artifact này; dùng `frankenphp version` và bundled `php.exe -n -v` để ghi
identity. Đây không phải lỗi serving hoặc một failed measured run. Release
build còn warning `compile_validated` unused đã có trước, không sửa/ẩn nó.
