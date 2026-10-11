# Bài học từ WezTerm, Mullvad, WinSW và các dự án liên quan

Ngày khảo sát: 2026-10-11. Mục đích: chọn phần đáng đọc để phát triển Hamster Wheel trên Windows và hiểu mô hình runtime kiểu Herdr. Đây là khảo sát tài liệu/source upstream, **không** phải kiểm chứng các dự án đó bằng build hoặc triển khai Service.

## Điểm xuất phát thực tế của Hamster

Hamster đã có một server process sở hữu PHP worker pool, Windows Job Object cho cây worker do nó sinh ra, và control pipe tùy chọn với `ping`, `status`, `stop`, `reload-certificates` ([source](../../src/control.rs), [runbook](../WINDOWS_OPERATIONS.md)). Pipe đặt DACL owner/System khi tạo; `stop` đi vào graceful drain, không phải `TerminateProcess`. Test với process detached đã chạy, nhưng hành vi dưới một Windows Service account thật, cross-account authorization và supervisor stop/restart vẫn **NOT VERIFIED** ([handoff](../AI_HANDOFF.md)). FastCGI `php-cgi` là endpoint do operator quản lý, nằm ngoài Job Object của Hamster. Vì thế câu hỏi gần nhất không còn là “có cần IPC không?”, mà là “SCM/supervisor gọi control endpoint, báo readiness, thu hồi đúng process tree và ghi log như thế nào?”.

Herdr là đối tượng học khác: [README](https://github.com/herdrdev/herdr) mô tả background server giữ terminal khi client detach, nhưng sau **server/machine restart** chỉ layout được phục hồi và agent session hỗ trợ có thể resume; process cũ không sống qua restart. Không đồng nhất “reattach vào daemon còn sống” với “khôi phục process sau crash”.

## Ma trận đối chiếu

| Dự án | Evidence upstream / vị trí đọc | Giá trị cho Hamster hoặc Herdr | Không sao chép máy móc |
|---|---|---|---|
| **WezTerm** | [`wezterm-mux-server`](https://github.com/wezterm/wezterm/tree/main/wezterm-mux-server), [`wezterm-client`](https://github.com/wezterm/wezterm/tree/main/wezterm-client), [`mux`](https://github.com/wezterm/wezterm/tree/main/mux), [`pty`](https://github.com/wezterm/wezterm/tree/main/pty); [multiplexing domains](https://github.com/wezterm/wezterm/blob/main/docs/multiplexing.md) | Ranh giới client ↔ daemon ↔ PTY, identity của pane/domain, attach/reconnect; ưu tiên cao cho Herdr. | Tự khởi động mux khi client connect phù hợp desktop UX, không tự động phù hợp server do SCM/supervisor sở hữu. PTY không thuộc HTTP/PHP hot path của Hamster. |
| **Mullvad VPN** | [architecture](https://github.com/mullvad/mullvadvpn-app/blob/main/docs/architecture.md), [`mullvad-daemon`](https://github.com/mullvad/mullvadvpn-app/tree/main/mullvad-daemon), [`mullvad-management-interface`](https://github.com/mullvad/mullvadvpn-app/tree/main/mullvad-management-interface), [`mullvad-cli`](https://github.com/mullvad/mullvadvpn-app/tree/main/mullvad-cli) | Daemon là owner dài hạn; CLI/GUI là frontend; tách SCM lifecycle khỏi request xử lý. Windows dùng named pipe, Unix dùng UDS theo [security document](https://github.com/mullvad/mullvadvpn-app/blob/main/docs/security.md). | Mullvad công khai coi mọi local process/user được phép dùng management interface là **ngoài** threat model của họ. Hamster cố ý giới hạn owner/System; không copy ACL hoặc bề rộng gRPC/event API. Mullvad cần SYSTEM cho VPN/firewall; Hamster không mặc nhiên cần privilege đó. |
| **WinSW** | [XML contract](https://github.com/winsw/winsw/blob/v3/docs/xml-config-file.md), [logging](https://github.com/winsw/winsw/blob/v3/docs/logging-and-error-reporting.md), [CLI](https://github.com/winsw/winsw/blob/v3/docs/cli-commands.md) | Ứng viên external Service wrapper: `<stoparguments>`/`<stopexecutable>` có thể gọi `hamster-wheel --control stop`; `<stoptimeout>`, `<onfailure>`, log rolling và account policy tạo mẫu thử có thể đo. | Mặc định WinSW thử Ctrl+C rồi có thể force-terminate sau timeout. Chưa chứng minh wrapper quản lý chính xác cây `php-cgi`, Job Object lồng nhau, hoặc stop dưới account thật. Không coi XML là evidence vận hành. |
| **interprocess** | [library/README](https://github.com/kotauskas/interprocess) | Tham chiếu API Rust cho local socket, Windows named pipe và Tokio; hữu ích khi audit primitive, cancellation và portability. | Hamster đã có pipe Windows với ACL và framing được test. Thay dependency/transport chỉ khi một thiếu hụt cụ thể được chứng minh; thư viện không tự quyết định authorization hay shutdown semantics. |
| **windows-service-rs** | [README](https://github.com/mullvad/windows-service-rs), [`ping_service.rs`](https://github.com/mullvad/windows-service-rs/blob/main/examples/ping_service.rs) | Mẫu native SCM: service dispatcher, control handler, `StartPending`/`Running`/`StopPending`/`Stopped`, status handle và stop signal; đối chứng với WinSW. | Ví dụ ping không chứng minh graceful HTTP/PHP drain. Native Service tăng code Windows-specific, packaging và test burden; chưa có evidence bắt buộc Hamster phải tích hợp trực tiếp. |
| **Zellij** | [architecture](https://github.com/zellij-org/zellij/blob/main/docs/ARCHITECTURE.md), [0.44.0 changelog](https://github.com/zellij-org/zellij/blob/main/CHANGELOG.md) | Client/server, session attach/detach, CLI automation; 0.44.0 (2026-03-23) ghi nhận Windows native. Hữu ích cho Herdr session semantics. | “Session resurrection” không có nghĩa process cũ được hồi sinh. Không kéo UI/plugin/session machinery vào Hamster. |
| **Distant** | [repository](https://github.com/chipsenkbeil/distant), [`manager listen`](https://distant.dev/reference/cli/commands/manager/listen/) | Manager daemon, Windows detached `CreateProcess`, protocol điều khiển process/filesystem và quyền truy cập local IPC. | Repo tự gắn nhãn **alpha**. Remote filesystem/process API lớn hơn nhiều so với bốn lệnh control của Hamster; không dùng làm security hoặc stability guarantee. |

## Bài học kỹ thuật cần giữ lại

1. **Tách owner và client.** WezTerm/Herdr giữ PTY dưới server khi UI biến mất; Mullvad giữ state dưới daemon khi CLI/GUI thoát. Hamster tương tự: PHP transaction/worker không được sống hay chết theo HTTP client hoặc control client. Tuy nhiên Hamster process phải do supervisor quản lý; không để CLI âm thầm auto-spawn server.
2. **Authorization là quyết định sản phẩm, không phải thuộc tính của IPC.** Mullvad chọn local-user access rộng trong threat model của họ; Hamster chọn owner/System. Khi đổi account Service, cần test thật `status`, `stop`, `reload-certificates` với tài khoản được phép và bị từ chối. Việc tạo endpoint rồi mới siết quyền sẽ có cửa sổ truy cập; một [advisory upstream của Mullvad](https://github.com/mullvad/mullvadvpn-app/security/advisories/GHSA-p9rr-wc9m-qmwg) minh họa nguyên tắc đó trên Unix, không phải bằng chứng lỗi Windows của Hamster.
3. **SCM status khác application readiness.** `Running` của wrapper/service không tự chứng minh PHP READY hoặc FastCGI endpoint khỏe. Cần kiểm tra log/readiness, `--control status` và một HTTP route an toàn, rồi mới tuyên bố deployment ready.
4. **Stop có deadline và thứ tự owner.** WinSW có stop timeout và forced fallback; Hamster phải nhận stop qua control pipe, ngừng admission, drain, ACK/reap worker. External `php-cgi` cần supervisor và stop policy riêng; không phát lại request có thể đã chạy khi nó chết.
5. **Giữ control plane nhỏ.** Bốn lệnh, một request/connection của Hamster đang đủ cho vận hành hiện tại. gRPC subscription của Mullvad hay protocol remote process/file của Distant chỉ đáng dùng khi có consumer và invariants cụ thể.

WinSW [README](https://github.com/winsw/winsw) hiện phân biệt bản 2.x ổn định và 3.x pre-release; các link XML ở trên là nhánh v3, **không** phải lời khuyên lấy bản mới nhất để triển khai. Pin binary và schema cùng một version trong phép thử. Một [issue upstream](https://github.com/winsw/winsw/issues/1136) về account bị hạn chế là gợi ý thêm cho test quyền, không phải chứng minh Hamster sẽ gặp cùng lỗi.

## Quyết định WinSW hay Service native cần evidence gì?

| Ranh giới | WinSW bên ngoài | Adapter SCM viết bằng Rust |
|---|---|---|
| Stop | `<stoparguments>` chạy chính Hamster CLI với `--control stop`; wrapper chờ rồi có timeout fallback. Cần chứng minh stop command chạy đúng account và không force-kill trước khi drain xong. | `ServiceControl::Stop` chuyển vào shutdown trigger hiện có; `StopPending` và wait hint phải phản ánh drain thật. [Ví dụ ping](https://github.com/mullvad/windows-service-rs/blob/main/examples/ping_service.rs) chỉ là khung, không có HTTP/PHP workload. |
| Ready / crash | Wrapper/SCM nhìn process state; cần probe HTTP và `status` để biết ứng dụng sẵn sàng. `<onfailure>` cần test exit code và backoff. | Có thể báo `Running` sau readiness của Hamster, nhưng phải sở hữu install/registration, status transitions và recovery policy trong code/package. |
| Log / child tree | WinSW có rolling stdout/stderr; đo log khi pipe đóng, crash và stop. Exact PID test cho Hamster, PHP và `php-cgi`; không suy diễn từ tên process. | Hamster cần log sink cho Service không console; Job Object hiện tại vẫn chỉ bảo vệ PHP worker do Hamster sinh, không tự bao phủ FastCGI endpoint ngoài. |

**Suy luận:** WinSW là đường thử nhỏ hơn vì không đổi server runtime; native adapter chỉ có lý do mạnh nếu WinSW không đáp ứng các gate trên hoặc cần readiness/SCM semantics mà wrapper không cung cấp. Đây chưa phải quyết định chọn sản phẩm hay version.

## Thứ tự học và phép thử đề xuất

Vì control pipe **đã tồn tại**, điều chỉnh thứ tự ban đầu theo nhu cầu còn mở:

1. **WinSW + windows-service-rs — quyết định vận hành Windows.** Trong VM cô lập, dựng hai phương án cùng config và cùng workload: WinSW gọi `--control stop` qua `<stoparguments>`; native SCM adapter chỉ làm bridge `Stop →` graceful shutdown hiện có. Đo `StartPending`→ready, stop khi có request đang chạy, exit code, log rotation, crash/restart, exact Hamster/PHP/`php-cgi` PIDs và orphan. Giữ account ít quyền; không sửa service host đang dùng. Nếu WinSW đáp ứng, chưa có lý do tích hợp Service trực tiếp.
2. **Mullvad + interprocess — audit control boundary, không rewrite.** Đọc service/CLI split, quyền pipe, framing, cancellation, admission, deadline và log. Tạo test với Service account thật và account khác. Chỉ xem xét `interprocess` nếu primitive hiện tại có failure được tái hiện hoặc portability cần triển khai thực sự.
3. **WezTerm — nghiên cứu Herdr process/session ownership.** Lần theo đường client connect → mux server → domain/pane → PTY → disconnect/reconnect; lập bảng trạng thái qua client exit, server crash, machine restart. Tách rõ state persistence với process persistence.
4. **Zellij + Distant — nghiên cứu bổ sung.** Dùng Zellij để đối chiếu session/CLI trên Windows; Distant để đọc protocol và daemon detach, nhưng đánh giá alpha và scope lớn trước khi mượn ý tưởng.

### Gate quyết định

Chưa chọn WinSW hay native Service làm architecture. Quyết định chỉ sau VM rehearsal chứng minh: stop/drain không biến thành force kill; restart không để orphan và không làm mất ownership FastCGI; logs có thể chẩn đoán; pipe ACL đúng dưới Service account; certificate reload hoạt động không console. Nếu một gate fail, ghi exact process identity, log và thời điểm thất bại rồi sửa đúng boundary. TCP SYN-blackhole và Linux native vẫn **NOT VERIFIED**; nghiên cứu này không thay đổi chúng.

## Giới hạn của khảo sát

Chỉ đối chiếu source/docs upstream được liên kết và checkout Hamster hiện tại. Không clone/build/chạy WezTerm, Mullvad, WinSW, Zellij, Distant, interprocess hay windows-service-rs; không kiểm chứng runtime hoặc security claims của họ trên host này. Không thay production code, Service, firewall, PHP endpoint hay dữ liệu ứng dụng.
