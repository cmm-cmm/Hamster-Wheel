# Mốc đánh giá và mục tiêu hiệu suất Windows — Node.js

Ngày ghi nhận: **2026-11-01**. Trạng thái: **baseline đã đo; mục tiêu cải
thiện đề xuất, chưa triển khai hoặc xác nhận đạt**. Tài liệu riêng này dùng
để chọn và đánh giá công việc tiếp theo; không thay roadmap, quyết định
architecture hay tiêu chí release. Lượt ghi nhận không chạy benchmark mới.

## FACT — Baseline được giữ nguyên

Nguồn: [báo cáo Node đầy đủ](../benchmarks/results/node_windows_20261101.md),
[raw 54 lượt](../benchmarks/results/node_windows_20261101.jsonl),
[metadata](../benchmarks/results/node_windows_20261101.jsonl.meta.json).
Windows 11, i5-1235U, Node **26.10.0**, PHP **8.5.12-1 NTS**, Hamster **0.8.0**
release MSVC, oha **1.16.0**. Mỗi cell có ba process tree mới mỗi bên,
xen kẽ H N N H H N, warm-up 1.000 request và 10 giây gửi tải mỗi lượt.

**6.431.035 request đúng status dự kiến, không lỗi transport/status ngoài
dự kiến; 54 pre/post response checks qua.** Có 3.477.322 response 200 và
2.953.713 response 404 dự kiến, không phải tất cả là request ứng dụng 200.

Binary baseline SHA-256:
`D1E2B14CF48611844762B5C1B3CC6D13EEAD8DA0D87825A6803071FC63F74163`.
Không ghép với bảng FrankenPHP cũ thành phép so ba bên cùng binary: binary
đã khác và các thay đổi đường ghi PHP frame đã tồn tại trước pass Node.

### Mốc throughput ứng dụng

Mỗi ô RPS là **min / median / max**; H/N là tỷ lệ hai median, không phải
scaling efficiency hoặc phần chi phí có thể loại bỏ.

| Workload | Hamster/PHP RPS | Node RPS | H/N |
| --- | ---: | ---: | ---: |
| Hello W1/C1 | 1831 / 2455 / 3210 | 5966 / 7572 / 10182 | 0,32 |
| Hello W4/C32 | 11011 / 12733 / 14750 | 25462 / 30594 / 32487 | 0,42 |
| JSON W4/C32 | 12179 / 13215 / 16049 | 34120 / 36084 / 37367 | 0,37 |
| CPU SHA256×20000 W4/C16 | 373 / 384 / 422 | 85 / 96 / 102 | 4,00 |
| POST 1 MiB digest W4/C16 | 614 / 792 / 822 | 919 / 1061 / 1063 | 0,75 |

### Mốc latency và tài nguyên

Independent median, mỗi ô **Hamster / Node**. CPU gồm user+kernel cả cây
server trên mỗi request; không gồm load generator. RAM cuối run không phải
peak hay bằng chứng memory stability.

| Workload | P99 ms | CPU µs/request | Working set MiB | Private MiB |
| --- | ---: | ---: | ---: | ---: |
| Hello W1/C1 | 1,10 / 0,39 | 514,90 / 83,28 | 39,50 / 100,66 | 12,36 / 51,62 |
| Hello W4/C32 | 4,35 / 3,10 | 534,57 / 131,02 | 111,71 / 259,99 | 41,98 / 138,91 |
| JSON W4/C32 | 3,65 / 2,61 | 532,89 / 120,89 | 111,74 / 259,40 | 41,62 / 137,30 |
| CPU W4/C16 | 59,61 / 276,43 | 10648,63 / 41248,09 | 110,29 / 309,76 | 39,55 / 193,60 |
| POST 1 MiB W4/C16 | 31,65 / 37,23 | 9941,93 / 4222,62 | 136,55 / 450,24 | 61,72 / 329,59 |

Static nhỏ C32 có spread RPS chồng lấp; static 1 MiB C8 Hamster đạt median
1523 RPS so với Node 1149, nhưng CPU/request cao hơn (~4247 / 1823 µs).
Không lấy throughput cao hơn để kết luận CPU efficiency tốt hơn.

## INFERENCE — Mục tiêu dùng để đánh giá cải thiện

Node là **mốc tham chiếu**, không phải specification hoặc điều kiện bắt buộc
để hoàn tất Hamster. Không cam kết đạt mọi con số Node; ưu tiên giảm chi phí
đã xác định mà vẫn giữ chức năng và boundary của Hamster.

| Hạng mục | Mục tiêu | Bằng chứng cần có |
| --- | --- | --- |
| Cold first-request | Giải thích preflight 504 còn mở từ lượt Laravel trước | Timeline spawn/READY/request/exit và diagnostic; nếu không tái hiện, giữ NOT REPRODUCED, không ghi đã sửa |
| Hello C1 và W4/JSON | Thu hẹp khoảng cách với mốc Node, giảm latency và CPU/request | Profile phân biệt dispatch/pipe/PHP/HTTP; A/B cùng workload chứng minh lợi ích vượt drift |
| POST 1 MiB | Tăng goodput, giảm CPU/request, giữ lợi thế tail latency hiện có | Profile body assembly/frame/pipe/IO; báo riêng P50/P95/P99, CPU cả cây và của host |
| Static | Giữ containment/streaming và kiểm soát trade-off throughput–CPU | Control static nhỏ và 1 MiB; không bỏ validation để so với Node fixture ít policy hơn |
| CPU-heavy | Giữ correctness và tránh regression do thay đổi request path | Digest độc lập, throughput/latency/CPU control; không rewrite dispatcher chỉ để đuổi bài hash |

**Mốc tham vọng dài hạn để theo dõi**, không phải yêu cầu implementation đã
phê duyệt: tiến gần H/N = 1 ở Hello/JSON và POST. Khi đo lại, phải so với
Node control chạy cùng đợt; không dùng cố định 7572/30594/36084/1061 RPS
trên máy khác như SLO. Mục tiêu trước mắt là một cải thiện nhỏ có nguyên
nhân và có thể tái lập, thay vì đặt phần trăm tăng tùy ý trên host đang nhiễu.

### Điều kiện công nhận một candidate

1. Profile chỉ ra operation/phase cụ thể trước khi chọn primitive; ghi rõ
   allocation/copy/lock/syscall nào được thay đổi và workload kích hoạt.
2. Build release, cùng runtime/INI/body/queue/admission, ít nhất ba fresh
   process mỗi bên, xen kẽ baseline/candidate, giữ warm-up và measured run
   đủ dài. Không chạy build/test nặng đồng thời với đo tải.
3. Báo min/median/max RPS; median P50/P95/P99, CPU/request và WS/private.
   Giữ mọi run, error/status và body checks. Lợi ích phải phân biệt được
   với drift control; nếu chưa phân biệt được, kết luận INCONCLUSIVE.
4. Nếu throughput tăng nhưng tail latency/CPU/RAM xấu đi, ghi trade-off
   và chưa công nhận đạt mục tiêu tổng thể khi chưa có justification.
5. Chạy regressions phù hợp: body/frame limits, cancellation, at-most-once,
   timeout, shutdown, containment; workload overload phải tách goodput 200
   khỏi 503 và latency từng status trước khi suy ra SLO.

Giữ nguyên transaction ownership, bounded admission/body budgets, protocol
semantics, static containment và lifecycle. Không replay request có thể đã
thực thi, bỏ policy bảo vệ, tăng timeout để che lỗi hoặc dùng 503 trả nhanh
như PHP throughput. Không tự mở thêm Node execution subsystem.

## HYPOTHESIS — Chưa được số liệu này chứng minh

Khoảng cách Hello chưa xác định là dispatch, pipe, PHP runtime hay phần khác.
POST có ~68% CPU cả cây ở host Hamster; đó chưa chứng minh một copy cụ thể
là bottleneck. CPU-heavy chỉ ~6% CPU ở host và dùng API hash khác Node;
không suy thành kết luận Rust/PHP nói chung nhanh hơn JavaScript.

Node fixture là core HTTP/cluster, có primary + N workers kể cả W1; không
Express/Fastify/Laravel. 404/static dùng một Node HTTP worker so với Tokio
đa luồng mặc định; không cùng CPU quota. Node không có đầy đủ admission,
aggregate body budget và static policy tương đương. Chưa kiểm chứng Node
standalone/best tuning, peak/soak/leak, TLS/remote/business/database hoặc
Linux trong phép đo này.

## Trạng thái tiếp theo

**Đã ghi nhận baseline và tiêu chí đánh giá. Chưa triển khai tối ưu hoặc
đánh dấu mục tiêu đạt.** Việc tiếp theo đề xuất là chẩn đoán cold request
và profiling hẹp Hello C1/POST với control 404. Owner có thể dùng tài liệu
này để chọn slice; không coi việc ghi tài liệu là phê duyệt redesign hoặc
production change. Historical results và raw evidence giữ nguyên.
