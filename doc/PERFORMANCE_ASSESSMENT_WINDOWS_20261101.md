# Đánh giá hiệu suất Hamster Wheel trên Windows

Ngày đánh giá: 2026-11-01. Đây là đánh giá kỹ thuật và đề xuất công việc tiếp
theo, **không phải chứng nhận production hoặc quyết định release**.

## 1. Kết luận điều hành

Hamster Wheel 0.8.0 có kết quả tốt ở HTTP tối thiểu và persistent PHP worker
trong các fixture đã đo. Static nhỏ C32 gần ngang FrankenPHP. Chưa có bằng
chứng đủ vững để nói FastCGI nhanh hơn một tỷ lệ cố định: độ dao động lớn,
thậm chí phạm vi RPS của FrankenPHP vượt cả hai phía phạm vi của Hamster.

**Việc nên làm trước là tăng độ tin cậy và khả năng giải thích của phép đo,
không phải redesign dispatcher, thay runtime hay thêm cache theo suy đoán.**
Số đo bổ sung chỉ ra candidate thực tế hơn: CPU của Hamster khi nhận POST
1 MiB, goodput dưới reject storm và latency framework C1. Static có thể được
kiểm tra riêng bằng một phép đo index-selection rất nhỏ. Không suy hiệu suất
Laravel/CPU nặng từ benchmark Hello.

## 2. Nguồn và phạm vi bằng chứng

- [Báo cáo số đo gốc](../benchmarks/results/frankenphp_windows_20261101.md):
  điều kiện, hash binary/runtime, toàn bộ min/median/max, latency, CPU và RAM.
- [78 lượt đo](../benchmarks/results/frankenphp_windows_20261101.jsonl) và
  [lượt lặp W4/C4](../benchmarks/results/frankenphp_windows_20261101_w4c4_repeat.jsonl).
- Source đã đối chiếu: `src/static_files.rs`, `src/fastcgi.rs`,
  `src/fastcgi_profile.rs`, driver `benchmarks/frankenphp_windows.py`.

Windows 11 Home build 26300; i5-1235U, 12 logical CPU; release MSVC; PHP
8.5.12-1 NTS cho Hamster, FrankenPHP 1.13.4/PHP 8.5.12 TS có DLL khớp bộ
Windows 8.5.12-1; oha 1.16.0. Loopback HTTP/1.1 keep-alive, không TLS/access
logging. Mỗi ô có ba process mới/bên, H F F H H F, warm-up rồi phát request
10 giây và chờ request cuối hoàn tất. Host không dành riêng cho benchmark.

Các kết quả không đo tác động riêng của icon hoặc bản vá PHP. Không tính
regression từ bảng PHP 8.5.10 cũ; không ngoại suy sang Linux, mạng ngoài,
HTTPS hoặc một ứng dụng thực có database.

## 3. FACT — Hiệu suất hiện tại đã quan sát

Ma trận chính: **3.834.582 request đo được**, không lỗi transport hoặc status
ngoài dự kiến. Số dưới đây là median; phải đọc cùng độ phân tán ở báo cáo gốc.

| Trường hợp | RPS Hamster / FrankenPHP | Đánh giá trong phạm vi đã đo |
| --- | ---: | --- |
| HTTP 404 C1 | 6.521 / 4.892 | Hamster cao hơn ở cả ba lượt của ô này |
| Static nhỏ C32 | 3.092 / 3.052 | Gần ngang; các phạm vi giao nhau |
| Static 64 KiB C32 | 2.882 / 2.602 | Median Hamster cao hơn; chưa xác định syscall gây chi phí |
| Static 1 MiB C8 | 1.367 / 819 | Hamster cao hơn trong cả ba lượt; đây là loopback, không phải năng lực đường truyền ngoài |
| Worker Hello W1/C1 | 1.519 / 867 | Tín hiệu thuận lợi cho persistent worker trên fixture nhỏ |
| Worker JSON W1/C1 | 2.043 / 1.000 | Không đại diện cho framework hoặc business logic |
| Worker Hello W4/C32 | 6.979 / 5.148 | Độ phân tán lớn, chưa chứng minh scaling efficiency |
| FastCGI JSON, bốn slot, C16 | 3.602 / 1.716 | Không kết luận speedup 2,10x ổn định |

File static nhỏ thực tế dài **23 byte** do CRLF; nhãn `22 B` trong raw driver
là tên lịch sử. Không đổi payload giữa hai server.

### Latency, CPU và bộ nhớ

- Worker Hello W1/C1: median P99 **1,18 / 2,05 ms**; chưa thấy median latency
  15,625 ms trên ô này. Không dùng điều này để chứng minh mọi workload không
  có stall.
- Worker W4/C32: P99 **8,74 / 21,66 ms**; CPU toàn cây server **853 / 1.218
  µs/request**. Đây là CPU server, không gồm CPU client oha.
- W4/C32: private bytes **42,20 / 102,53 MiB**, nhưng tổng working set
  **111,81 / 72,38 MiB**. Không thể kết luận đơn giản “Hamster dùng ít RAM
  hơn”: working set cộng lặp các trang chia sẻ, private bytes là commit,
  còn số đo cuối lượt không phải peak hoặc kiểm chứng leak.
- FrankenPHP 1.13.4 hiểu `num_threads` là thread ngoài worker. Cấu hình cũ
  của driver đặt W+1 ngoài W worker; có thêm thread dự phòng trong số RAM.
  NTS/process và TS/thread cũng là hai mô hình khác nhau. Đây không phải so
  sánh hai cấu hình đã tối ưu bộ nhớ đến mức tốt nhất.

### Độ bất định phải giữ lại

FastCGI JSON của FrankenPHP dao động **1.119–5.039 RPS**, bao phủ toàn bộ
khoảng **3.313–3.622** của Hamster. Vì vậy tỷ lệ hai median không phải bảo đảm
hay confidence interval. Ba mẫu không đủ xây dựng kết luận thống kê mạnh.

Một lượt FrankenPHP W4/C4 mất **39,645 giây** dù chỉ phát request 10 giây,
không có lỗi được báo. Lượt lặp riêng không tái hiện khoảng dài đó nhưng
throughput vẫn dao động ở cả hai bên. Dữ liệu đầu không giữ đủ timing để
quy lỗi cho server, client hay host. Không loại outlier; không thay bằng lượt
lặp thuận lợi hơn. P99 thông thường cũng không mô tả được một sự kiện cực
hiếm nằm ngoài 1% chậm nhất.

## 4. Laravel và PHP tải lớn — bổ sung

[Báo cáo bổ sung](../benchmarks/results/frankenphp_windows_20261101_laravel_heavy.md)
ghi workload, dữ liệu thô và cả các lần thử thất bại. PHP nặng có 24 lượt,
1.162.908 attempt: 573.648 response 200, 589.260 response 503 có kiểm soát,
không lỗi transport hoặc status khác. Kiểm tra nội dung/digest trước và sau
tải qua tất cả các lượt, không phải kiểm chứng nội dung từng response.

| Workload | Goodput median Hamster / FrankenPHP | Nhận xét |
| --- | ---: | --- |
| CPU: 20.000 vòng SHA-256, W4/C16 | 235 / 223 | Phần việc PHP nặng làm khoảng cách còn ~1,05x |
| POST đúng 1 MiB, trả digest, W4/C16 | 514 / 146 | Là nhận body lớn, không phải stream response 1 MiB |
| Hello W4/C64 | 6.916 / 3.885 | Không có 503 |
| Hello W4/C128 | 3.810 / 3.580 | Hamster từ chối ~84% attempt; phạm vi goodput hai bên giao nhau |

CPU nặng: tiến trình Hamster dùng khoảng 897/17.386 µs CPU mỗi request của
toàn cây (~5%). POST lớn: khoảng 9.496/13.730 µs (~69%). Đây là hai profile
khác nhau; một tối ưu dispatch chung không thể mặc nhiên giúp cả hai.

C128 của Hamster hoàn tất 111.616 request và trả 589.260 lần 503 trong ba
lượt. Không gọi tổng rate ~23k response/s là ~23k PHP executions/s. Goodput
giảm so với C64 dù vẫn có thể trả response bình thường sau tải. Client
closed-loop phát thêm attempt khi 503 trả nhanh, nên chưa cô lập được ảnh
hưởng của admission CPU. P99 đang trộn 200/503, không phải P99 thành công.

Laravel dùng bản sao tạm Laravel 12.69.2/Octane 2.20.0, config/view cache,
OPcache, session/cache array, SQLite trong RAM; không dùng dữ liệu/PMS thật.
Ma trận sửa cấu hình có **110.490 request, tất cả 200**, không lỗi transport;
kiểm tra nội dung trước/sau tải qua cả 24 lượt. `/login` chỉ trả 17 byte;
Welcome render 81.277 byte Blade HTML. Không có browser asset loading hoặc
nghiệp vụ đăng nhập/database.

| Laravel W4 | RPS Hamster min/median/max | RPS FrankenPHP min/median/max |
| --- | ---: | ---: |
| `/login` C1 | 119 / 119 / 166 | 111 / 156 / 358 |
| `/login` C16 | 537 / 617 / 1.065 | 448 / 451 / 1.033 |
| Welcome C1 | 190 / 229 / 233 | 251 / 255 / 260 |
| Welcome C16 | 748 / 753 / 761 | 709 / 722 / 732 |

**Điểm thua cần ghi rõ:** Welcome C1 có median Hamster thấp hơn khoảng 10%,
phạm vi ba lượt không giao nhau. Đây là candidate profile latency framework
C1, không phải lý do bỏ/reset lifecycle. Welcome C16 hơi nghiêng về Hamster;
`/login` quá nhiễu để khẳng định tỷ lệ ổn định. Ngay cả Welcome C1, P99 median
7,33 / 7,56 ms không cùng thứ tự với throughput: không nói mọi chỉ số latency
của Hamster đều kém hơn.

Lần preflight đầu của Hamster nhận 504 chưa rõ nguyên nhân, không được tính
như benchmark thành công hoặc xóa khỏi lịch sử. Lỗi Caddyfile do phần mở rộng
driver bỏ giá trị env rỗng đã tái hiện bằng parser và sửa quoting; không phải
bằng chứng FrankenPHP native crash.

## 5. INFERENCE — Điểm nên điều tra để cải thiện

### 5.1. Sửa chất lượng measurement trước

Giữ full client summary, thời gian monotonic bên ngoài, cửa sổ phát tải và
thời gian drain riêng; ghi lỗi startup/exit code/stdout-stderr. Không bỏ `-w`
để che việc request cuối kéo dài. Nếu thêm watchdog, timeout phải là kết quả
thất bại có log, không âm thầm cắt bỏ mẫu. Driver đã bắt đầu lưu các trường
timing/log/status cần thiết; chưa giải thích được outlier cũ.

Đo trong điều kiện host ổn định hơn; ghi nguồn điện, power policy/tần số CPU,
tải nền và tình trạng quét file, nhưng không tự tắt bảo vệ hay dịch vụ máy.
Chèn một control ngắn trước/sau nhóm và cân bằng thứ tự hai bên giữa các
block. Nếu cả control cũng trôi mạnh, báo inconclusive thay vì tối ưu server.
Tăng thời gian từng lượt chỉ giảm một phần nhiễu, không tự chữa host drift.

### 5.2. Profile static theo operation, không bỏ containment

`src/static_files.rs::resolve_file/open_contained` hiện đã gộp open, kiểm tra
final path của handle, metadata và đọc file <=64 KiB trong một blocking task.
File lớn vẫn stream. Không đề xuất lại các tối ưu đã có hoặc cache kết luận
containment theo pathname.

Counterexample measurement đáng thử: ô file nhỏ đang request `/`, nên còn
chi phí chọn `index.html`. Source thử open directory, nhận PermissionDenied,
query metadata directory rồi open index; `/index.html` đi đường trực tiếp.
Đo cặp `/` và `/index.html` trên cùng file 23 B, C1/C32, một thay đổi mỗi lần.
Chênh lệch ổn định mới chứng minh chi phí index selection đáng xử lý.

Đối chiếu `windows_filesystem -- operations/handoff` và
[hướng dẫn ETW](WINDOWS_STATIC_PROFILING.md): tách thời gian chờ blocking pool,
open/final-path/metadata, read và scheduler. Không hủy session profiler của
người khác; ETL chẩn đoán không đưa vào Git. Instrumented timing không thay
cho benchmark release không instrumentation.

Candidate code chỉ xét sau đó: giảm handoff/copy thực sự được chứng minh dư
thừa. Bảo vệ tuyệt đối **handle validated = handle served**, GET/HEAD cùng
boundary, access policy/PHP-source denial, giới hạn RAM và response liveness.
Không nâng ngưỡng buffer hoặc đọc file lớn toàn bộ vào RAM theo trực giác.

### 5.3. POST lớn và overload: profile đúng phần có thể cải thiện

Đây là candidate dựa trên CPU đo được, không chỉ vì RPS thấp/cao. Đối chiếu
`src/php_worker.rs::read_http_body`, `src/php_protocol.rs::encode_request` và
`write_frame_tracked`: có bước gom body, copy vào frame và ghi pipe. Sự tồn tại
của copy không chứng minh nó chiếm 9,5 ms; cần tách allocation/copy, chờ
handoff/pipe và kernel CPU trước khi chọn sửa.

Phép đo nhỏ: body 256 KiB/1 MiB, C1/C16, cùng digest và cùng binary; ghi CPU
Hamster/PHP riêng và thời gian từng phase trong profiling build. Chỉ tối ưu
reserve/copy hoặc batching I/O nếu profile/A-B chỉ rõ lợi ích. Không preallocate
theo Content-Length chưa được bảo đảm, không nới aggregate budget/body limit,
không đổi protocol v2 hay bỏ tracking bytes-written để được số đẹp.

Với C128, dùng arrival rate cố định tại vài mức quanh capacity đã đo; báo
goodput, 503, CPU/200 và latency 200/503 riêng. Đo một batch bình thường sau
tải. Chỉ sau khi kiểm chứng reject handling gây chi phí đáng kể mới xem xét
giảm allocation/response work trên nhánh từ chối. Không tăng queue vô hạn,
thêm retry hoặc biến queue size thành cách che quá tải. Bài hiện tại không
chứng minh busy traffic làm hại route/pool khác; cần test riêng nếu xét vấn đề đó.

### 5.4. Worker: đo queue và cùng concurrency trước khi thay dispatcher

W1/C1 và W4/C32 không phải phép đo scaling cùng concurrency. Matrix nhỏ tiếp
theo nếu cần: W1/W4 tại C1/C4/C16/C32, cùng workload và cấu hình lifecycle.
Tách thời gian xếp hàng khỏi transaction; dùng snapshot/counter hiện có,
chỉ thêm profiling bounded nếu thiếu, không synchronous log từng request.

Chỉ xem xét giảm allocation/copy IPC hoặc reconstruction khi profile chỉ ra
operation cụ thể và A/B lặp lại có lợi vượt nhiễu control. Không global mutex,
không hủy transaction khi HTTP disconnect, không retry request có thể đã chạy.

### 5.5. FastCGI: tận dụng measurement đã có

Source đã có kept connections, kiểm tra trước reuse, script-resolution cache
bounded và đường connect Windows được giới hạn tài nguyên. Không mặc định
“thêm connection reuse” là công việc mới, cũng không kéo dài cache tùy tiện.

Nếu nhu cầu thực tế hoặc phép đo ổn định xác nhận bottleneck, dùng
`src/fastcgi_profile.rs::FastCgiStageTiming` để tách slot wait, TCP connect,
preamble write và backend head. Backend-head bao gồm backlog, decode, PHP và
scheduling, không đồng nghĩa thời gian PHP thuần. Đối chiếu tỷ lệ connection/
resolution reuse và CPU Hamster/PHP riêng, rồi mới chọn sửa đúng phase.

Giữ endpoint do operator quản lý; không replay transaction đã có thể bắt đầu.
Đánh giá overload bằng **goodput 200/s**, 503 và latency tách status; không
coi hàng loạt 503 trả nhanh là năng lực thực thi PHP.

### 5.6. Framework C1: chỉ profile trước, không đoán reset/metadata

Nếu latency C1 thấp là mục tiêu của workload thực, đo riêng request hydration,
Octane handle, cleanup và flush/IPC; dùng cùng route/view cache/INI trên hai
bên. Số PHP nặng hoặc Hello không trả lời câu hỏi này. Không tự thay application
reset, metadata/authority hay bỏ lifecycle listener để thắng benchmark.
Một profile có overhead chỉ dùng định vị; candidate vẫn phải qua release A/B
và regression isolation/cancellation/no-retry trước khi cân nhắc nhận.

Tách riêng cold startup/first request khỏi steady state. Lần đầu 504 là một
giới hạn chưa đóng, không được giải thích bằng warm-up sau đó. Lưu READY,
first-request elapsed, worker exit/replacement và log cùng một timeline;
không tăng request_timeout hoặc retry để làm lỗi biến mất trong báo cáo.

## 6. HYPOTHESIS — Chưa đủ chứng cứ để sửa production

- Blocking-pool scheduling, filesystem/minifilter hoặc index selection có
  thể giải thích phần chi phí static; chưa có trace lần này để phân bổ.
- Một số khoảng chậm có thể do host/client; chưa chứng minh nguyên nhân.
- Reconstruction/lifecycle Laravel có thể tốn hơn HTTP/IPC tối thiểu; cần
  số đo Laravel và profile cụ thể, không suy từ Hello.
- Bộ nhớ tăng/giảm theo worker hoặc thời gian cần peak sampling/soak riêng;
  không suy memory stability từ ảnh chụp cuối lượt.

## 7. Quy tắc nhận một cải thiện

Một candidate cần baseline/candidate cùng host, runtime, workload, INI và
chính sách admission; ít nhất ba process mới/bên, có control và độ phân tán.
Chấp nhận khi lợi ích lặp lại vượt drift control, không làm xấu đáng kể ô
đối chứng/latency/resource, và regression correctness tương ứng vẫn qua.
Nếu lợi ích nằm trong nhiễu hoặc cần nới safety invariant thì không nhận.

Không mở scope TLS/proxy, Linux, generic plugin, adaptive pool hoặc tối ưu
dài hạn từ các con số này. Không thay roadmap hay tự phê duyệt next slice.

## 8. Đề xuất bước kế tiếp và trạng thái bàn giao

Đề xuất Owner xem xét một **profiling pass nhỏ cho POST 1 MiB và framework
C1**, kèm bài arrival-rate cố định cho overload. Mục tiêu là tìm phase có chi
phí hoặc stall, chưa cam kết sửa primitive nào. Đây là nơi dữ liệu mới chỉ ra
khả năng giảm CPU/latency thực dụng; không cần redesign worker pool.

Nếu chỉ cần một experiment rẻ trước, chạy cặp static `/`–`/index.html` cùng
payload. CPU-heavy thuần hiện không phải lý do ưu tiên tối ưu dispatcher:
đó chủ yếu là công việc PHP đã cố định trong bài đo.

Đã hoàn thành tài liệu/số đo Windows trong phạm vi này; chưa thể tuyên bố
production performance readiness. Còn cold preflight 504 chưa rõ nguyên nhân,
host drift/outlier timing, latency overload chưa tách status, thiếu peak/soak,
remote/TLS/business/database workload. 145 test ignored ở gate trước chưa
chạy lại. Trước khi dùng runtime PHP mới cho triển khai, operator cần chạy
functional/lifecycle gates trên đúng distribution đó; bộ PHP trong `php/8.5`
không được âm thầm thay bằng runtime benchmark.

Lượt này chỉ sửa/bổ sung driver và fixture đo, không sửa Rust production hay
protocol; bảy regression driver và lint ba fixture PHP qua. Gate Rust 314
test/fmt/strict Clippy và binary release từ lượt ngay trước vẫn áp dụng vì
code/binary không đổi. Dữ liệu cũ giữ nguyên; raw bổ sung được lưu riêng.
Không commit/tag/push/merge/release. Cleanup tiến trình/thư mục tạm được ghi
trong handoff; tool cache/log phục vụ tái lập giữ ở `target/` đã git-ignore.

## 9. Nhận định tổng hợp và khuyến nghị của trợ lý

### Nhận định

Theo bằng chứng hiện có, Hamster có nền tảng hiệu suất đáng tiếp tục phát
triển, nhưng chưa thể kết luận vượt FrankenPHP nói chung. Ưu thế Hello không
chuyển nguyên vẹn sang Laravel hoặc CPU-heavy; static nhỏ C32 gần ngang,
Welcome C1 còn thua về throughput, và overload cần đọc bằng goodput thay vì
tổng response/s. Chọn một con số cao nhất để mô tả sản phẩm sẽ gây hiểu nhầm.

Tôi **không đề xuất một đợt tối ưu diện rộng**. Giá trị lớn hơn lúc này là
đóng khoảng trống chẩn đoán, rồi giảm đúng chi phí có ảnh hưởng tới workload
thực. Lỗi first-request 504 cần được ưu tiên hơn việc tăng thêm vài phần trăm
RPS: nếu tái diễn, đó là vấn đề availability chứ không chỉ là benchmark.
Đây là nhận định kỹ thuật, không phải kết luận nguyên nhân của lần 504 đó.

### Thứ tự công việc đề xuất

| Thứ tự | Công việc | Bằng chứng thúc đẩy | Đầu ra cần có trước khi sửa production |
| --- | --- | --- | --- |
| 1 | Điều tra cold first-request và độ ổn định phép đo | Preflight 504 chưa giải thích; outlier và host drift vẫn còn | Timeline chung từ spawn/READY tới request đầu, log/exit/replacement và timing client; phân loại được nguyên nhân hoặc ghi rõ chưa tái hiện |
| 2 | Profile POST lớn và Laravel C1 trong một pass hẹp | POST 1 MiB có ~69% CPU toàn cây ở Hamster; Welcome C1 thấp hơn ~10% median | Chi phí từng phase, xác định operation cụ thể; một candidate nhỏ với baseline/A-B, không bắt đầu bằng lựa chọn primitive |
| 3 | Kiểm chứng admission dưới arrival rate cố định | C128 từ chối ~84% attempt, goodput thấp hơn C64 | Goodput, latency 200/503 riêng và CPU/200 ở cùng offered rate; xác định chi phí reject hay queue trước khi đề xuất thay đổi |
| Phụ, chi phí thấp | So `/` với `/index.html` trên cùng file | Source có thêm bước chọn index ở `/` | Chênh lệch lặp lại đủ lớn mới mở tối ưu static; nếu không thì dừng |

Đây là thứ tự đề xuất theo rủi ro và khả năng học từ bằng chứng, không phải
roadmap đã được Owner phê duyệt. Không lấy việc “thắng benchmark” làm điều
kiện để chọn hoặc loại một workload/framework.

### Điều kiện nhận candidate

Chỉ triển khai khi profile chỉ ra chi phí cụ thể và phép đo có thể phân biệt
baseline/candidate khỏi drift control. Giữ cùng workload/runtime/INI/admission,
ít nhất ba process mới mỗi bên; báo cả độ phân tán, P95/P99, CPU và bộ nhớ.
Nếu có trade-off giữa các chỉ tiêu thì trình bày rõ, không chỉ chọn RPS.

Không đổi protocol, nới body/queue budget, bỏ lifecycle/containment, hủy
transaction vì client disconnect hoặc replay request có thể đã thực thi để
đạt kết quả. Không tăng timeout để che 504, không tính 503 trả nhanh là PHP
throughput. Những ràng buộc này là tiêu chí loại candidate, không phải khoản
chi phí có thể đánh đổi ngầm.

**Khuyến nghị chốt:** Owner xem xét cho phép profiling pass ở bước 1–2; chưa
có căn cứ phê duyệt redesign worker pool, adaptive sizing, cache mới hoặc
thay đổi default production. Nếu không tái lập được vấn đề hoặc lợi ích nằm
trong nhiễu thì giữ implementation hiện tại và ghi kết quả inconclusive.

## 10. Bổ sung Node.js — 2026-11-01

Chi tiết và raw:
[Windows Node comparison](../benchmarks/results/node_windows_20261101.md).
Node 26.10.0 core HTTP/cluster, PHP 8.5.12-1 NTS, oha 1.16.0, ba fresh
process tree mỗi bên, 10 giây/run, chín cell, xen kẽ H N N H H N.
**54 run, 6.431.035 request, không lỗi transport/status ngoài dự kiến;
54 pre/post response checks qua.** Laravel không được quy đổi thành Node.

**FACT:** release SHA-256 lần này là
`D1E2B14CF48611844762B5C1B3CC6D13EEAD8DA0D87825A6803071FC63F74163`,
khác baseline FrankenPHP phía trên. Checkout đã có sửa đường ghi frame PHP
trước khi bắt đầu pass Node; giữ nguyên và build lại. Vì vậy không ghép số
cũ thành bảng ba bên, không tính regression/improvement cho thay đổi frame
từ hai matrix khác thời điểm. Không sửa production code trong pass Node.

| Workload | Hamster median RPS | Node median RPS |
| --- | ---: | ---: |
| Hello W1/C1 | 2455 | 7572 |
| Hello W4/C32 | 12733 | 30594 |
| JSON W4/C32 | 13215 | 36084 |
| CPU SHA256×20000 W4/C16 | 384 | 96 |
| POST 1 MiB digest W4/C16 | 792 | 1061 |

Toàn bộ spread, static/404, P50/P95/P99, CPU và RAM trong báo cáo chi tiết.
Node thắng Hello/JSON ở cả ba run; PHP thắng bài hash chain nhưng hai API
hash có chi phí binding khác nhau. POST Node throughput/CPU tốt hơn, còn
P95/P99 median cao hơn Hamster. Không nói một bên nhanh hơn mọi workload
hay percentile. Static nhỏ có spread chồng lấp; static 1 MiB Hamster RPS
cao hơn nhưng dùng CPU/request cao hơn.

**INFERENCE:** Node xử lý HTTP trong app worker, Hamster qua PHP IPC;
khoảng cách Hello không tự chứng minh HTTP engine chậm. Nên profile Hello
C1 với control 404 và POST lớn trước khi sửa primitive. Hamster host chiếm
~68% CPU ở POST, chỉ ~6% ở CPU-heavy; tối ưu dispatcher không giải quyết
được phần lớn chi phí bài hash. Giữ cold preflight 504 chưa giải thích ở
ưu tiên chẩn đoán, không tuyên bố matrix Node đã đóng finding đó.

**Giới hạn:** Node cluster có primary + N worker kể cả W1; 404/static dùng
một Node HTTP worker, trong khi Hamster dùng Tokio đa luồng mặc định.
Không so tại cùng CPU quota, không claim cấu hình Node tốt nhất/minimum
RAM. Node fixture thiếu tương đương aggregate body budget, job admission,
static containment và các policy production của Hamster. End RAM thấp hơn
ở Hamster không phải bằng chứng memory stability. Node standalone,
framework/database/TLS/remote/soak và Linux không được đo trong lượt này.

## 11. Kiểm tra lại FrankenPHP trên binary hiện tại

[Báo cáo riêng, raw và phase evidence](../benchmarks/results/frankenphp_windows_20261101_current.md).
FrankenPHP 1.13.4 / PHP 8.5.12, Hamster dùng NTS 8.5.12-1. SHA Hamster
`D1E2...` trùng Node pass; không dùng `AF03...` của bảng cũ làm A/B.
13 cell, ba fresh process/arm, 10 giây/run, H F F H H F. Hoàn tất **78 timed
rows, 5.125.180 response đúng status dự kiến, zero transport/unexpected
status errors trong phần đo**. **Có thêm một cold Laravel preflight 504**;
không được diễn giải timed data thành toàn bộ qualification không lỗi.

Median RPS H/F: Hello W1/C1 **2396/1234**, W4/C32 **14331/7290**; JSON
**14484/5672**; POST 1 MiB **897/343**. CPU-heavy **456/404** nhưng spread
overlap. Static nhỏ cũng overlap. Laravel `/login` C16 **1644/1119**;
Welcome C1 **423/441**, C16 **884/930**, cả hai overlap. Không ngoại suy
ưu thế Hello thành ưu thế framework. W4/framework có summed WS cao hơn
nhưng private bytes thấp hơn FrankenPHP, không phải ít RAM hơn nói chung.

**FACT mới:** attempt gốc dừng sau 42 rows khi H `/login` preflight trả 504,
slot 0 generation 1 hết request deadline 5 s sau khi pool đã READY. Không
retry hoặc tăng timeout. Sáu cell còn thiếu hoàn tất trong continuation
riêng với clone/process mới; original failure/log/raw giữ nguyên.

**Probe riêng, không làm performance data:** ba cây H4 mới trên cùng scratch
app, ba request khác marker, đều 200: **3725,75 / 133,40 / 73,54 ms**.
Octane gateway lần lượt **3505,56 / 120,87 / 48,63 ms**, mỗi process tăng
120 included files qua gateway. Decode/build/flush nhỏ trong chính probe.
Không sửa production; instrumentation chỉ ở scratch adapter/vendor copy.

**INFERENCE:** cold first-use framework là hướng điều tra có evidence mạnh
hơn speculative dispatcher optimization. **Chưa chứng minh** filesystem,
OPcache, antivirus hay CPU là nguyên nhân; probe không tự tái hiện 504 và
không xác định toàn bộ 5 s ở failed run. Mục tiêu cold-request chuyển thành
**REPRODUCED / PARTIALLY LOCALIZED / NOT FIXED**, không đóng bởi steady-state
success. Startup budget 30 s và request deadline 5 s vẫn độc lập; không tăng
timeout hoặc prewarm route ngầm để làm kết quả đẹp. Không đo Linux lượt này.

## 12. Phân tích riêng Laravel và proposal

[File đề xuất](LARAVEL_WINDOWS_PERFORMANCE_ANALYSIS_PROPOSAL_20261101.md)
đối chiếu source/raw và bổ sung hai probe semantic/lifecycle, không đo lại
throughput hoặc sửa production. Laravel không thua mọi cell: `/login` C1
−15%, Welcome C1/C16 ~−4/−5% có spread overlap; `/login` C16 +47%.

**FACT mới:** Symfony Response::send trên FrankenPHP đã gửi đủ response cho
client khi callback tail còn bị giữ ở marker gate. Hamster commit sau mọi
cleanup. Octane script cap mặc định 1000 độc lập engine max_requests=0;
4.200 request khi không đặt MAX_REQUESTS tạo 8 bootstrap / 4 end events ở
đúng 1000, zero errors. Driver cũ chưa ghim/ghi effective cap. Không gán
ngược số restart cho từng historical row hoặc xóa số đo trước.

**INFERENCE:** khác điểm HTTP completion và lifetime có thể ảnh hưởng cách
đọc C1/tail/CPU, chưa lượng hóa được phần gap do từng yếu tố. Cần pin control
và reuse Gate 2 phase profiling trước khi chọn primitive. Không dùng việc
bỏ cleanup/commit gate để thu hẹp RPS. Cold 504 vẫn riêng một nhánh chưa đóng.
