# Hamster Wheel — Backlog nghiên cứu (tạm hoãn)

Ngày ghi nhận: 2026-09-27. Trạng thái chung: **đề xuất nghiên cứu, chưa triển khai hoặc chứng minh lợi ích**, trừ những năng lực nền tảng đã được báo cáo riêng. Tài liệu này không phải cam kết tính năng hay số liệu hiệu năng. Ưu tiên hiện tại của Owner là hoàn thiện bản Windows.

Tài liệu ý tưởng liên quan: `HAMSTER_MORPH_REQUEST_GRAPH_JIT.md` (bản phác thảo tầm nhìn Morph; mọi capsule, StateFence, Cortex, Turbo và HamsterLink trong đó vẫn là giả thuyết cần kiểm chứng).

## Các hướng đã được nêu

| Hướng | Giả thuyết / giá trị | Probe đầu tiên và điều kiện dừng |
|---|---|---|
| Morph: workload lanes | Phân tách hàng đợi/worker cho request ngắn và dài có thể giảm head-of-line blocking. | Dùng log PMS thật để tạo tải trộn `/up`, `/login`, dashboard, report. Giữ cùng tổng worker và giới hạn RAM; đo P50/P99 theo route, queue wait, CPU, RSS, starvation. Dừng nếu không cải thiện tail latency đủ bù chi phí phân vùng. |
| Morph: route affinity mềm | Worker vừa xử lý route R có thể xử lý R kế tiếp nhanh hơn. | Tiền kiểm không cần scheduler: thử `R→R` và `S→R` ngẫu nhiên trên cùng worker, cùng input và trạng thái DB; tách queue wait khỏi thời gian PHP. Nếu hiệu ứng không lặp lại vượt nhiễu, bỏ affinity. Nếu có, factorial 2×2 lanes × affinity, cùng worker/RAM; chỉ nhận affinity khi lợi ích tăng thêm so với lanes-only và không làm xấu route khác. P99.9 chỉ báo khi đủ mẫu. |
| Morph mở rộng / Request-Graph JIT | Quan sát workload, tạo đường xử lý chuyên biệt có guard, canary và rollback. | Chỉ sau khi probe nhỏ chứng minh ích lợi; mọi dạng capsule, tự động đổi topology, deoptimization, Execution Genome và Cortex AI là tương lai, không phải hiện trạng. Không bỏ qua ngữ nghĩa middleware hay isolation. |
| GC ngoài request | Chuyển thu gom chu kỳ sang pha housekeeping có thể giảm tail latency. | Trên đúng phiên bản PHP đang chạy, lấy delta `gc_status()`/request theo mẫu, đo overhead ghi số liệu; nếu GC không góp phần đáng kể vào request chậm thì dừng. Nếu đáng, so sánh mặc định, trì hoãn GC, và housekeeping có giới hạn bộ nhớ. Ghi roots, buffer_size, RSS, worker-seconds housekeeping và latency request kế tiếp. Không gộp tác động `gc_collect_cycles()` với `gc_mem_caches()`. |
| Warm Clone Windows | Clone worker PHP/Laravel đã boot để rút ngắn startup và chia sẻ bộ nhớ. | Nghiên cứu biệt lập: PHP tối giản → từng extension → Laravel boot → chuỗi request. Trước clone kiểm tra process mẫu chỉ có một thread, không giữ socket/DB connection; sau clone kiểm tra handle, loader, extension, Defender, ổn định và bộ nhớ. Một thread/không socket chỉ là sàng lọc, không bảo đảm an toàn. API clone Windows chưa được hỗ trợ chính thức; dừng hoặc giới hạn hỗ trợ nếu không ổn định hay lợi ích RAM thấp. Linux fork là nhánh khác cần kiểm chứng riêng. |
| Cache theo hợp đồng ứng dụng | Ứng dụng khai báo response công khai và điều kiện invalidation; Hamster kiểm tra hợp đồng trước khi cache. | Thiết kế hợp đồng rõ principal, session, cookie, cache key, phụ thuộc DB và ghi từ ngoài worker; probe endpoint public thực sự. Mặc định loại `/login`, CSRF, session và dữ liệu cá nhân. Differential tests cache bật/tắt trên biến thể tenant/user và dữ liệu thay đổi. Không gọi heuristic là bằng chứng. |
| Principal Echo / StateFence | Adapter có thể báo principal và kiểm tra trạng thái không rò giữa request trên persistent worker. | Hiện chỉ là đề xuất; xác định nguồn danh tính đáng tin cậy, ranh giới request, reset state, tình huống auth chuyển đổi và test rò chéo user/tenant trước khi dựa vào nó cho cache hoặc tối ưu khác. |
| Verified Deploy | Shadow generation, so sánh response trước khi chuyển traffic. | Đặc tả môi trường cách ly và cơ chế chặn side effect (DB write, mail, queue, HTTP outbound, lock, audit); transaction read-only một mình không đủ. Chỉ mirror request đủ điều kiện, chuẩn hóa diff và rollback; không mặc nhiên replay POST. |
| Early Hints tự học | Quan sát asset ổn định rồi phát 103 để tải song song. | Phụ thuộc HTTP/2 và đo thực nghiệm trình duyệt qua mạng thật; cần guard theo host/auth và invalidation theo deploy. Để sau HTTP/2. |
| Static I/O Windows | `TransmitFile`/IoRing có thể giảm chi phí static file. | Chỉ probe khi profiling chỉ ra static I/O là nút thắt; `TransmitFile` không dùng trực tiếp cho nội dung đang mã hóa TLS. |
| HamsterLink / shared memory | Giảm chi phí IPC Rust–PHP. | Chỉ bắt đầu nếu trace cho thấy IPC chiếm phần đủ lớn trong latency; cần protocol, crash recovery và tính đúng đắn xuyên process. |

## Điều chỉnh từ phản biện

- `Principal Echo` chưa tồn tại; không được dùng như điều kiện đã có cho cache.
- `/login` không phải endpoint thử cache an toàn vì session/CSRF.
- Tên “proof-carrying micro-cache” nói quá: header/fingerprint chỉ là heuristic. Nếu tiếp tục, gọi là cache theo hợp đồng ứng dụng và xác minh đúng phạm vi hợp đồng.
- Tách lợi ích lanes và affinity; kết quả tổng Morph không chứng minh affinity hữu ích.
- `gc_status()` có thêm các chỉ số thời gian và `buffer_size` từ PHP 8.3; root buffer không còn giới hạn cố định 10.000 từ PHP 7.3. Vẫn phải đo áp lực bộ nhớ thực khi trì hoãn GC.
- Kiểm tra số thread/handle trước Warm Clone là điều kiện cần, không chứng minh extension hoặc process clone an toàn.
- Không tuyên bố hướng nào độc nhất thế giới hoặc hứa hiệu năng khi chưa có so sánh công bằng.

## Phạm vi thực thi hiện nay: hoàn thiện Windows

Theo handoff do Owner cung cấp, TLS v1 và certificate reload đã qua nhiều gate Windows, gồm test process Ctrl+Break; Linux SIGHUP/native vẫn chưa được kiểm chứng. Không có checkout mã nguồn trong môi trường cuộc trò chuyện này để tái kiểm chứng hoặc xác nhận hiện trạng mới hơn. Trước khi gọi bản Windows hoàn tất, cần làm việc trên checkout thực tế và đối chiếu tài liệu handoff, test, packaging và vận hành Windows. Các nghiên cứu ở trên được giữ ở trạng thái backlog, không chen vào đường hoàn thiện Windows.

Không tự động commit, tag, push, merge hoặc release từ tài liệu này.
