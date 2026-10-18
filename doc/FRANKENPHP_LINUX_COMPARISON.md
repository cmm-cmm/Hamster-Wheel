# Hamster và FrankenPHP trên Linux — đo cùng máy

Đây là phép đo trực tiếp, cùng máy, cùng PHP, cùng kịch bản. Hai server chạy xen kẽ nhau.
Khác với `doc/FRANKENPHP_BENCHMARK.md` ở hai điểm:

- Báo cáo đó đo FrankenPHP worker mode so với classic mode trên PMS thật, trên Windows.
- Phần so sánh với Hamster trong `doc/research/FRANKENPHP_PMS_EVIDENCE.md` ghép các lần đo khác
  máy và khác cấu hình, nên tự nó ghi là "không phải so sánh sản phẩm hợp lệ".

- **Ngày đo:** 18/10/2026
- **Hamster:** branch `dev-linux` tại commit `7611f32`, bản release
- **Bản ghi số liệu:** `doc/BENCHMARKS.md`, mục *Hamster vs FrankenPHP — Linux, same host*
- **Script tái lập:** `benchmarks/frankenphp_linux.py`

---

## Kết luận

> **Trên ứng dụng Laravel thật, hai server ngang nhau về thông lượng. Hamster tốn ít CPU hơn, FrankenPHP
> tốn ít RAM hơn.** Khoảng chênh lớn chỉ xuất hiện ở hai đầu: kịch bản PHP gần như rỗng
> (Hamster nhanh gấp ~2 lần) và file static (FrankenPHP nhanh gấp 2–3,4 lần).

1. **Chi phí riêng của server cho PHP worker: Hamster thấp hơn.**
   - Với script trả "Hello from PHP", Hamster đạt 8.157 RPS ở 1 kết nối. FrankenPHP đạt 4.110 RPS.
   - CPU mỗi request: 114 µs so với 297 µs.
   - Ở 4 worker và 32 kết nối, Hamster đạt 32.465 RPS, FrankenPHP 16.904 RPS.
   - p99 lần lượt là 2,2 ms và 5,9 ms.
2. **Laravel làm khoảng chênh đó biến mất.**
   - Bản thân ứng dụng tốn 1,4–2,4 ms CPU mỗi request, gấp hơn 10 lần phần chênh của server.
   - Qua hai lượt đo độc lập, `/login` và trang welcome đều cho kết quả trong khoảng ±10%, lúc bên này
     hơn, lúc bên kia hơn.
   - Ổn định ở cả hai lượt là CPU: Hamster dùng ít hơn 9–18% mỗi request.
3. **File static là điểm yếu rõ nhất của Hamster trên Linux.**
   - Caddy, tầng HTTP của FrankenPHP, đạt 7.622 RPS ở 1 kết nối và 43.089 RPS ở 32 kết nối.
   - Hamster đạt 3.728 và 12.585 RPS.
   - Nguyên nhân đã ghi ở `doc/LINUX_VERIFICATION.md`: `resolve_file` đi qua thread pool bốn lần mỗi request.
4. **Chế độ classic (mỗi request một lần chạy PHP mới): FrankenPHP nhanh hơn 32–34%.**
   - FrankenPHP chạy PHP ngay trong tiến trình của nó.
   - Hamster phải gửi request qua FastCGI sang `php-cgi` do operator khởi động.
5. **RAM: FrankenPHP chạy 4 worker Laravel trong một tiến trình, tổng 140–170 MiB.**
   - Hamster có 4 tiến trình PHP riêng cộng tiến trình Rust, tổng 240–260 MiB.
   - Đây là cái giá của mô hình cách ly tiến trình. Một worker Hamster crash chỉ làm request đó trả 502,
     rồi slot được tạo lại; các worker khác không bị ảnh hưởng.

---

## 1. Môi trường

| Hạng mục | Giá trị |
|---|---|
| Máy | Cloud container, 4 vCPU Intel Xeon @ 2,10 GHz, 16 GiB RAM, không swap |
| OS | Ubuntu 24.04.5 LTS, Linux 6.18 |
| Hamster | 0.3.1, release, `dev-linux` @ `7611f32` (có `TCP_NODELAY`) |
| PHP của Hamster | 8.5.11 NTS, CLI + CGI, build từ tag `php-8.5.11` |
| FrankenPHP | 1.13.1 (Caddy 2.11.7), PHP 8.5.11 ZTS |
| Công cụ đo | `oha` 1.16.0, HTTP/1.1, keep-alive, chạy chung máy |

Có hai bản FrankenPHP:

| Bản | Nguồn | Dùng cho |
|---|---|---|
| **glibc** | Build từ tag `v1.13.1` bằng Go 1.27.0, liên kết với `libphp.so` 8.5.11 ZTS (`--enable-embed --enable-zts --disable-zend-signals --enable-zend-max-execution-timers`, cùng bộ extension với PHP của Hamster); tag `nobrotli,nowatcher,nobadger,nomysql,nopgx` | **Số liệu chính** |
| Static | Binary chính thức `frankenphp-linux-x86_64` của release v1.13.1, static-pie, SHA-256 `0b5c93c6…12c4086` | Đối chứng |

Bản static từng bị nghi chậm hơn vì liên kết tĩnh. Thực tế nó cho kết quả gần như bản glibc ở mọi
kịch bản (mục 4), nên kết luận không phụ thuộc vào cách build.

---

## 2. Cách đo

- **Thứ tự chạy:** mỗi kịch bản chạy 6 lần, theo thứ tự Hamster, FrankenPHP, FrankenPHP, Hamster,
  Hamster, FrankenPHP. Mỗi server có 3 lần chạy; mỗi lần khởi động server mới.
- **Warm-up:** 1.000 request trước khi đo, 200 với classic.
- **Số liệu báo cáo:** RPS trung vị. Độ trễ và CPU/RAM lấy từ lần chạy trung vị.
- **CPU:** tổng user + system của cả cây tiến trình, chia cho số request. Với Hamster gồm tiến trình Rust
  và các tiến trình PHP; với FrankenPHP là một tiến trình. Kịch bản classic của Hamster cộng thêm
  `php-cgi`.
- **RAM:** RSS cộng của cả cây tiến trình ở cuối lần đo. RSS của nhiều tiến trình có thể đếm trùng
  trang nhớ dùng chung.
- **Kết quả:** mọi request đều trả đúng mã trạng thái mong đợi, không có lỗi kết nối.

Cấu hình từng kịch bản:

| Kịch bản | Hamster | FrankenPHP |
|---|---|---|
| Static | handler `static`, root `public/` (file 22 B) | `file_server`, cùng root |
| PHP worker | `fixtures/php-worker/worker.php`, 1 hoặc 4 worker | script worker tương đương: cùng body, mã trạng thái và header; `num_threads` = worker + 1 |
| Classic | FastCGI tới một `php-cgi -b` với `PHP_FCGI_CHILDREN=4`, `concurrency = 4` | `php_server`, `num_threads 4`, cùng `fixtures/php-cgi/bench.php` |
| Laravel | `fixtures/php-worker/laravel_worker.php`, 4 worker | worker Octane chuẩn (`frankenphp-worker.php`), `num 4`, `num_threads 5` |

Với Laravel, hai server dùng chung ứng dụng `www/pms-scratch` (Laravel 12 + Octane 2, dựng bằng
`prepare_scratch_app.sh`) và cùng biến môi trường:

- `APP_ENV=production`, `APP_DEBUG=false`
- `SESSION_DRIVER=array`, `CACHE_STORE=array`
- cùng `php.ini`, bật OPcache cho cả hai

Session để trong bộ nhớ để đo server và framework, không đo việc ghi SQLite. Trước khi đo, ba URL
`/login`, `/` và `/up` đã được kiểm tra trên cả hai server: cùng mã 200, cùng kích thước, và trang
welcome 81.268 byte giống hệt từng byte.

FrankenPHP để mặc định: không nén, không access log, HTTP/2 và HTTP/3 tắt vì không có TLS.

---

## 3. Kết quả — FrankenPHP bản glibc

RPS ghi theo dạng thấp nhất / trung vị / cao nhất của 3 lần chạy. Độ trễ tính bằng mili-giây.

### Laravel 12 + Octane, 4 worker

| Kịch bản | Server | RPS | p50 / p95 / p99 | CPU µs/req | RSS MiB |
|---|---|---|---|---:|---:|
| `/login` C1 | Hamster | 510 / **512** / 533 | 1,82 / 2,71 / 3,95 | 1.700 | 241 |
| | FrankenPHP | 484 / **540** / 555 | 1,76 / 2,49 / 3,25 | 1.957 | 151 |
| `/login` C16 | Hamster | 2.187 / **2.326** / 2.506 | 6,70 / 9,00 / 10,48 | 1.448 | 246 |
| | FrankenPHP | 2.160 / **2.169** / 2.198 | 7,32 / 10,86 / 13,02 | 1.697 | 140 |
| welcome 81 KB C1 | Hamster | 429 / **446** / 450 | 2,11 / 3,11 / 4,19 | 2.030 | 259 |
| | FrankenPHP | 443 / **453** / 469 | 2,11 / 2,96 / 3,98 | 2.400 | 166 |
| welcome 81 KB C16 | Hamster | 1.585 / **1.645** / 1.715 | 9,51 / 12,74 / 14,63 | 1.898 | 262 |
| | FrankenPHP | 1.638 / **1.649** / 1.697 | 9,53 / 13,50 / 15,89 | 2.200 | 170 |

`/login` đi qua toàn bộ middleware `web`: session, cookie mã hoá, CSRF. Trang welcome render
một view Blade 81 KB.

### PHP worker tối giản

| Kịch bản | Server | RPS | p50 / p95 / p99 | CPU µs/req | RSS MiB |
|---|---|---|---|---:|---:|
| hello W1 C1 | Hamster | 7.632 / **8.157** / 8.246 | 0,110 / 0,174 / 0,259 | 114 | 34 |
| | FrankenPHP | 3.824 / **4.110** / 4.313 | 0,215 / 0,392 / 0,577 | 297 | 103 |
| json W1 C1 | Hamster | 6.506 / **7.212** / 7.456 | 0,124 / 0,196 / 0,309 | 126 | 34 |
| | FrankenPHP | 3.591 / **3.729** / 4.096 | 0,234 / 0,454 / 0,659 | 324 | 103 |
| echo 12 B W1 C1 | Hamster | 6.286 / **6.300** / 6.617 | 0,146 / 0,218 / 0,319 | 170 | 34 |
| | FrankenPHP | 3.497 / **3.573** / 3.615 | 0,247 / 0,474 / 0,641 | 321 | 103 |
| hello W4 C4 | Hamster | 19.258 / **20.490** / 21.880 | 0,176 / 0,308 / 0,471 | 98 | 117 |
| | FrankenPHP | 13.615 / **14.130** / 14.474 | 0,243 / 0,501 / 0,913 | 166 | 107 |
| hello W4 C32 | Hamster | 30.958 / **32.465** / 32.595 | 0,927 / 1,398 / 2,223 | 72 | 118 |
| | FrankenPHP | 16.826 / **16.904** / 18.507 | 1,697 / 4,066 / 5,893 | 142 | 110 |

### Classic — mỗi request một lần chạy PHP mới

| Kịch bản | Server | RPS | p50 / p95 / p99 | CPU µs/req | RSS MiB |
|---|---|---|---|---:|---:|
| hello C1 | Hamster + php-cgi | 2.194 / **2.287** / 2.352 | 0,398 / 0,688 / 1,011 | 380 | 81 |
| | FrankenPHP | 2.923 / **3.061** / 3.354 | 0,294 / 0,534 / 0,735 | 388 | 103 |
| json C16 | Hamster + php-cgi | 10.386 / **11.264** / 12.204 | 1,324 / 2,192 / 3,323 | 237 | 84 |
| | FrankenPHP | 14.721 / **14.832** / 17.171 | 0,942 / 2,207 / 3,332 | 167 | 106 |

### File static 22 B

| Kịch bản | Server | RPS | p50 / p95 / p99 | CPU µs/req | RSS MiB |
|---|---|---|---|---:|---:|
| C1 | Hamster | 3.512 / **3.728** / 3.945 | 0,238 / 0,390 / 0,637 | 250 | 6 |
| | FrankenPHP | 7.369 / **7.622** / 8.208 | 0,106 / 0,235 / 0,362 | 154 | 88 |
| C32 | Hamster | 10.848 / **12.585** / 12.677 | 2,429 / 4,280 / 5,694 | 200 | 10 |
| | FrankenPHP | 41.090 / **43.089** / 44.737 | 0,246 / 3,336 / 6,915 | 62 | 92 |

---

## 4. Đối chứng với bản FrankenPHP static chính thức

Đây là lượt chạy riêng, cùng cách đo, trước khi có bản glibc. Bảng ghi RPS trung vị, Hamster so với
FrankenPHP static.

| Kịch bản | Hamster | FrankenPHP static | So với bản glibc |
|---|---:|---:|---|
| hello W1 C1 | 7.484 | 3.900 | glibc 4.110 |
| hello W4 C32 | 33.063 | 18.069 | glibc 16.904 |
| classic json C16 | 11.637 | 12.380 | glibc 14.832 |
| static C32 | 12.482 | 42.363 | glibc 43.089 |

Bản static và bản glibc cùng phía ở mọi kịch bản, với biên độ gần như nhau. Chỉ ở classic, bản
glibc nhanh hơn bản static rõ hơn (+20%).

---

## 4b. Lượt lặp lại bằng script tái lập

Đây là lượt chạy thứ hai, bằng chính `benchmarks/frankenphp_linux.py` với bản glibc. Mọi kịch bản
được chạy lại từ đầu. Bảng ghi RPS trung vị, Hamster / FrankenPHP; tỷ lệ được tính theo hướng
Hamster chia FrankenPHP.

| Kịch bản | Lượt 1 (mục 3) | Lượt 2 | Tỷ lệ lượt 2 |
|---|---|---|---:|
| Laravel `/login` C1 | 512 / 540 | 582 / 566 | 1,03 |
| Laravel `/login` C16 | 2.326 / 2.169 | 2.487 / 2.255 | 1,10 |
| Laravel welcome C1 | 446 / 453 | 469 / 511 | 0,92 |
| Laravel welcome C16 | 1.645 / 1.649 | 1.691 / 1.658 | 1,02 |
| worker hello W1 C1 | 8.157 / 4.110 | 7.251 / 3.865 | 1,88 |
| worker hello W4 C32 | 32.465 / 16.904 | 30.171 / 18.733 | 1,61 |
| classic json C16 | 11.264 / 14.832 | 11.355 / 14.867 | 0,76 |
| static C1 | 3.728 / 7.622 | 3.467 / 7.249 | 0,48 |
| static C32 | 12.585 / 43.089 | 12.524 / 40.611 | 0,31 |

- **Laravel CPU mỗi request, Hamster / FrankenPHP:** `/login` 1.530 / 1.850 và 1.352 / 1.635 µs;
  welcome 1.960 / 2.150 và 1.826 / 2.214 µs.
- **Không ổn định giữa hai lượt:** RSS của FrankenPHP ở kịch bản worker tối giản (103 MiB rồi 85 MiB) và
  ở Laravel (140–170 MiB rồi 125–152 MiB). Các con số RAM trong tài liệu này chỉ nên đọc như khoảng
  giá trị.

---

## 5. Phân tích

Các giải thích về nguyên nhân dưới đây là suy luận từ kiến trúc của hai server, chưa được profile.
Riêng về file static, cách Hamster mở file đọc trực tiếp từ mã nguồn.

**Vì sao Hamster nhanh hơn ở PHP worker tối giản?**

- Trên đường đi của mỗi request, Hamster làm việc trong Rust: parse HTTP bằng hyper, đóng một frame
  nhị phân, ghi qua pipe sang worker đã đợi sẵn. Phía PHP chỉ giải mã frame.
- FrankenPHP phải đi qua ranh giới Go↔C (cgo) và dựng lại môi trường SAPI cho mỗi request: các
  superglobal, header, output buffer.
- Phần chênh khoảng 180 µs CPU mỗi request là chi phí của tầng server. Nó không đổi theo độ nặng của
  ứng dụng.

**Vì sao chênh lệch biến mất với Laravel?**

- Một request Laravel ở đây tốn 1,4–2,4 ms CPU, gấp hơn 10 lần phần chênh của tầng server.
- Đây đúng là quy luật "lợi ích là hằng số cộng, không phải hệ số nhân" mà
  `doc/FRANKENPHP_BENCHMARK.md` đã rút ra.
- Ở C16, Hamster hơn 7% và 10% trên `/login` qua hai lượt. Ở welcome C1, FrankenPHP hơn 8% ở
  lượt 2. Các lần chạy của hai server chồng nhau một phần, nên về thông lượng không bên nào hơn chắc
  chắn.
- Điều lặp lại được qua cả hai lượt là CPU: Hamster dùng ít hơn 9–18% mỗi request.

**Vì sao FrankenPHP nhanh hơn ở static và classic?**

- **Static:** Caddy phục vụ file trực tiếp và đã được tối ưu lâu năm. Hamster mở file qua bốn lần
  gọi `tokio::fs`, mỗi lần đi qua thread pool riêng của tokio. Ở C32, FrankenPHP dùng 62 µs CPU mỗi
  request, Hamster dùng 200 µs.
- **Classic:** FrankenPHP chạy PHP ngay trong tiến trình. Hamster phải gửi request qua FastCGI sang
  một tiến trình `php-cgi` khác. Với Hamster, FastCGI là đường để chạy ứng dụng PHP kiểu cũ, không
  phải đường hiệu năng chính.

**RAM**

- FrankenPHP dùng chung một tiến trình, nên OPcache, mã máy của PHP và Caddy chỉ có một bản.
- Hamster có mỗi worker là một tiến trình riêng, khoảng 27 MiB với script tối giản và 55–60 MiB với
  Laravel.
- Đổi lại, một worker Hamster crash không kéo theo các worker khác, và slot được tạo lại tự động
  (`doc/WORKER_LIFECYCLE_0_3.md`).

---

## 6. Giới hạn của phép đo

1. **Chỉ một máy 4 vCPU, công cụ đo chạy chung máy.** Hai server và `oha` tranh CPU với nhau. Số tuyệt
   đối chỉ mô tả máy này.
2. **Chỉ HTTP/1.1, không TLS, không nén.** HTTP/2, HTTP/3, TLS tự động và nén là thế mạnh của
   FrankenPHP/Caddy nhưng chưa được đo.
3. **Laravel là `pms-scratch`, không phải PMS thật.** Session và cache để trong bộ nhớ, không có MySQL
   hay Redis. Với ứng dụng nặng database, cả hai server sẽ còn gần nhau hơn.
4. **Không có lượt đo nào kéo dài, crash, recycle hay đổi worker.** Hành vi khi lỗi của hai server
   khác nhau về bản chất và không được so sánh ở đây.
5. **Hamster dùng PHP NTS, FrankenPHP bắt buộc dùng PHP ZTS.** Đây là khác biệt vốn có của hai kiến
   trúc, không phải lựa chọn cấu hình.
6. **Cấu hình FrankenPHP để mặc định**, ngoài `num_threads` và số worker. Không thử tinh chỉnh
   `GOMAXPROCS`, `max_threads` hay mimalloc.

---

## 7. Tái lập

```sh
# PHP 8.5 NTS cho Hamster: doc/LOCAL_TEST_APPLICATIONS.md
# PHP 8.5 ZTS embed cho FrankenPHP, prefix riêng:
git clone --depth 1 --branch php-8.5.11 https://github.com/php/php-src.git
cd php-src && ./buildconf --force
./configure --prefix=/opt/php-8.5-zts --enable-embed --enable-zts --disable-zend-signals \
    --enable-zend-max-execution-timers --disable-cgi --disable-phpdbg --enable-mbstring \
    --with-openssl --with-curl --enable-intl --with-zip --with-sodium --with-sqlite3 \
    --with-pdo-sqlite --with-zlib
make -j"$(nproc)" && make install

# FrankenPHP bản glibc:
git clone --depth 1 --branch v1.13.1 https://github.com/php/frankenphp.git
cd frankenphp/caddy/frankenphp
P=/opt/php-8.5-zts/bin/php-config
CGO_ENABLED=1 GOTOOLCHAIN=auto CGO_CFLAGS="$($P --includes)" \
CGO_LDFLAGS="$($P --ldflags) $($P --libs) -L/opt/php-8.5-zts/lib -lphp -Wl,-rpath,/opt/php-8.5-zts/lib" \
    go build -ldflags='-w -s' \
    -tags=nobadger,nomysql,nopgx,nobrotli,nowatcher,deprecated_topic,deprecated_claim \
    -o frankenphp-glibc .

# Từ thư mục gốc repo:
cargo build --release && cargo install oha
fixtures/php-worker/pms/prepare_scratch_app.sh
benchmarks/frankenphp_linux.py --frankenphp path/to/frankenphp-glibc \
    --laravel "$PWD/www/pms-scratch"
```

Script tự sinh Caddyfile, script worker, `php.ini` và cấu hình Hamster vào một thư mục tạm. Trong
lượt chạy, nó chép stub `frankenphp-worker.php` của Octane vào `public/` của ứng dụng rồi xoá khi
kết thúc. Kết quả in ra từng dòng JSON. Có thể chạy riêng một nhóm: `static`, `worker`, `classic`
hoặc `laravel`.
