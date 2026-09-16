# Băng thử FrankenPHP — PMS Website

Báo cáo đo hiệu suất worker mode so với classic mode, kèm các đòn bẩy đã thử và bẫy đã gặp
khi triển khai.

- **Vòng 1** — 15/09/2026 — trang `/login`, không chạm database
- **Vòng 2** — 16/09/2026 — năm trang có dữ liệu thật, đăng nhập bằng tài khoản `admin`
- **Bản web:** https://claude.ai/artifact/1eYrM6TdAsnAWZ35fLXeZr

---

## Kết luận

> **Worker mode cắt một khoản cố định 23–59 ms mỗi request, không phải "nhanh hơn 8 lần".**

Vòng đo đầu tiên chỉ có trang `/login` nên ra kết luận ×8. Con số đó **đúng nhưng chỉ đúng
cho trang rỗng**. Khi đo trên trang có dữ liệu thật, tỷ lệ thực tế là **×1,2 đến ×4,5**.

Bốn điều rút ra:

1. **Lợi ích là hằng số cộng, không phải hệ số nhân.** Trang càng nặng, FrankenPHP càng ít
   giúp được. Ước lượng nhanh: `tỷ lệ ≈ T / (T − 50ms)` với T là thời gian classic mode.
2. **Nút thắt thật nằm ở ứng dụng.** Trang báo cáo chi phí bão hoà ở ~15 req/s ở **cả hai
   chế độ**. Đổi server không sửa được.
3. **Nguội so với ấm chênh tới 65 lần.** Request đầu tiên sau deploy chậm hơn nhiều lần mức
   bình thường — do Redis cache của ứng dụng, không phải do FrankenPHP.
4. **Rủi ro rò rỉ state giữa các user vẫn chưa kiểm chứng.** Đây là điều duy nhất đang chặn
   việc dùng worker mode ngoài mục đích đo đạc.

---

## 1. Môi trường đo

| Hạng mục | Giá trị |
|---|---|
| Máy | Windows 11 Pro 26200 · 12 nhân |
| Server | FrankenPHP 1.12.7 · Caddy 2.11.4 |
| PHP | 8.5.10 ZTS · opcache bật · JIT tắt |
| Ứng dụng | `pms-website-2` @ branch `dev` · commit `ee4608daf` · Laravel 12 |
| Luồng | `num_threads 24` · `worker num 12` |
| Backend | MySQL 8.0.44 + Redis, chung LAN tại `192.168.8.148` |
| Công cụ | `bench.php` · curl_multi · HTTP/1.1 · có `Accept-Encoding` như trình duyệt |

Hai chế độ chạy **song song trong cùng một tiến trình FrankenPHP**, cùng 12 luồng PHP, cùng
mã nguồn, cùng backend — nên đây là so sánh cân bằng chứ không phải hai lần đo rời rạc.

- Worker mode → `localhost:9080`
- Classic mode → `localhost:9081`

Mỗi kịch bản có warm-up riêng trước khi tính điểm. Toàn bộ request đều trả HTTP 200.
Vòng 2 tạo khoảng **1900 request có truy vấn database** lên MySQL dùng chung của đội — toàn
bộ là GET, không migration, không ghi dữ liệu.

---

## 2. Năm trang được đo

Chọn theo phổ tải, từ không chạm database đến rất nặng.

| Trang | Loại | Kích cỡ gzip |
|---|---|---:|
| `/login` | Không chạm DB — mốc đối chiếu | 5 KB |
| `/home/content/calendar` | AJAX JSON, truy vấn nhẹ | 11 KB |
| `/management/account` | Trang danh sách, truy vấn trung bình | 18 KB |
| `/projects/plan` | Trang kế hoạch, nhiều view và truy vấn | 23 KB |
| `/management/expense-report` | Báo cáo chi phí — nặng nhất tìm được | 41 KB |

Ba trang trả 302 với tài khoản `admin` nên bị loại vì thiếu quyền: `/management/customers`,
`/management/costs`, `/management/access`.

---

## 3. Kết quả đầy đủ

Đơn vị độ trễ là mili-giây. `req/s` càng cao càng tốt.

### conc = 1 — một người dùng

| Trang | Chế độ | avg | p50 | p95 | p99 | req/s | Tỷ lệ |
|---|---|---:|---:|---:|---:|---:|---:|
| `/login` | Worker | 2,3 | 2,2 | 2,8 | 3,1 | **434** | **×11,0** |
| | Classic | 25,4 | 28,3 | 37,0 | 40,2 | 39 | |
| `/home/content/calendar` | Worker | 19,4 | 20,3 | 22,5 | 23,6 | **51** | **×3,4** |
| | Classic | 66,0 | 66,1 | 86,1 | 98,0 | 15 | |
| `/management/account` | Worker | 28,3 | 28,1 | 31,6 | 34,5 | **35** | **×3,0** |
| | Classic | 85,3 | 91,1 | 108,1 | 109,2 | 12 | |
| `/projects/plan` | Worker | 15,6 | 15,5 | 16,9 | 18,3 | **64** | **×4,5** |
| | Classic | 69,8 | 70,0 | 96,7 | 97,0 | 14 | |
| `/management/expense-report` | Worker | 223,9 | 226,1 | 231,7 | 236,8 | **4,5** | **×1,3** |
| | Classic | 282,7 | 284,5 | 319,5 | 348,4 | 3,5 | |

### conc = 5 — năm người dùng đồng thời

| Trang | Chế độ | avg | p95 | req/s | Tỷ lệ |
|---|---|---:|---:|---:|---:|
| `/login` | Worker | 3,0 | 3,5 | **1247** | **×8,5** |
| | Classic | 25,2 | 39,5 | 146 | |
| `/home/content/calendar` | Worker | 23,1 | 29,0 | **167** | **×2,7** |
| | Classic | 64,1 | 84,0 | 61 | |
| `/management/account` | Worker | 35,9 | 47,7 | **111** | **×2,3** |
| | Classic | 81,8 | 116,6 | 48 | |
| `/projects/plan` | Worker | 18,4 | 23,2 | **201** | **×3,9** |
| | Classic | 77,7 | 103,4 | 51 | |
| `/management/expense-report` | Worker | 290,4 | 345,0 | **15,4** | **×1,3** |
| | Classic | 381,9 | 474,9 | 12,1 | |

### conc = 20 — hai mươi người dùng đồng thời

| Trang | Chế độ | avg | p95 | p99 | req/s | Tỷ lệ |
|---|---|---:|---:|---:|---:|---:|
| `/login` | Worker | 6,3 | 9,3 | 11,6 | **2682** | **×7,0** |
| | Classic | 45,7 | 65,4 | 72,4 | 384 | |
| `/home/content/calendar` | Worker | 42,3 | 75,2 | 92,1 | **425** | **×2,8** |
| | Classic | 117,8 | 157,3 | 175,7 | 150 | |
| `/management/account` | Worker | 120,5 | 181,4 | 185,7 | **147** | **×1,5** |
| | Classic | 171,5 | 230,3 | 233,4 | 98 | |
| `/projects/plan` | Worker | 34,2 | 50,8 | 50,9 | **444** | **×3,6** |
| | Classic | 127,2 | 202,2 | 202,6 | 123 | |
| `/management/expense-report` | Worker | 995,8 | 1508,3 | 1574,3 | **15,7** | **×1,2** |
| | Classic | 1179,9 | 1793,7 | 1840,3 | 13,5 | |

### Trần scale của trang rỗng

Đo riêng ở vòng 1 với 400 request, mức đồng thời cao hơn:

| Trang `/login` | conc 1 | conc 10 | conc 50 |
|---|---:|---:|---:|
| Worker req/s | 369 | 2220 | **2925** |
| Classic req/s | 45 | 299 | 357 |
| Worker avg | 2,7 ms | 3,7 ms | 14,9 ms |
| Classic avg | 22,1 ms | 28,6 ms | 126,2 ms |

Worker mode chạm trần khoảng **2900 req/s** trên máy 12 nhân này.

---

## 4. Phân tích: vì sao tỷ lệ sụp đổ

Nhìn vào **khoản tiết kiệm tuyệt đối** thay vì tỷ lệ thì bức tranh rõ ngay (conc = 1):

| Trang | Worker avg | Classic avg | **Tiết kiệm** | Tỷ lệ |
|---|---:|---:|---:|---:|
| `/login` | 2,3 ms | 25,4 ms | **23,1 ms** | ×11,0 |
| `/home/content/calendar` | 19,4 ms | 66,0 ms | **46,6 ms** | ×3,4 |
| `/management/account` | 28,3 ms | 85,3 ms | **57,0 ms** | ×3,0 |
| `/projects/plan` | 15,6 ms | 69,8 ms | **54,2 ms** | ×4,5 |
| `/management/expense-report` | 223,9 ms | 282,7 ms | **58,8 ms** | ×1,3 |

Khoản tiết kiệm nằm trong khoảng hẹp **23–59 ms** trên cả năm trang, dù thời gian xử lý
chênh nhau gần 100 lần. Đó chính là chi phí bootstrap Laravel mà worker mode xoá bỏ — một
**hằng số cộng**, không phải hệ số nhân.

```
thời gian worker mode  ≈  thời gian classic mode  −  50ms
tỷ lệ cải thiện        ≈  T / (T − 50ms)
```

Nên:

- Trang 60 ms → nhanh hơn ×6
- Trang 150 ms → nhanh hơn ×1,5
- Trang 1000 ms → nhanh hơn ×1,05 — gần như không đáng kể

**Hệ quả:** worker mode đáng giá nhất với endpoint nhỏ và gọi nhiều — API, AJAX, polling.
Với trang báo cáo nặng thì nó gần như không giúp gì; chỗ đó phải tối ưu truy vấn.

---

## 5. Nút thắt thật nằm ở ứng dụng

Trang `/management/expense-report` chỉ phục vụ được **15,7 req/s ở worker mode** và
**13,5 req/s ở classic mode**. Tăng đồng thời từ 5 lên 20 không làm throughput tăng mà chỉ
làm độ trễ phình ra:

| Mức đồng thời | Worker req/s | Worker avg |
|---:|---:|---:|
| 1 | 4,5 | 224 ms |
| 5 | 15,4 | 290 ms |
| 20 | **15,7** | **996 ms** |

Trang này bão hoà ở khoảng 15 req/s bất kể chế độ nào. Để so sánh, `/login` scale tuyến tính
đẹp: 434 → 1247 → 2682 req/s.

---

## 6. Nguội so với ấm — chênh tới 65 lần

Phát hiện ngoài dự kiến. Request đầu tiên tới một trang chậm hơn nhiều so với các request sau.

### Worker mode, ngay sau khi restart server

| Trang | Request #1 | Request #2 | Request #5 | Nguội/ấm |
|---|---:|---:|---:|---:|
| `/login` | 100 ms | 4 ms | 4 ms | **×23,9** |
| `/home/content/calendar` | 52 ms | 24 ms | 23 ms | ×2,3 |
| `/management/account` | 91 ms | 29 ms | 27 ms | ×3,3 |
| `/projects/plan` | 132 ms | 16 ms | 14 ms | **×9,1** |
| `/management/expense-report` | 241 ms | 224 ms | 228 ms | ×1,1 |

### Ba trạng thái nhiệt

Khảo sát đầu tiên — server vừa khởi động, Redis cache của ứng dụng chưa có gì — cho số liệu
cao hơn hẳn:

| Trang | Nguội hoàn toàn | Nguội opcache | Ấm hoàn toàn |
|---|---:|---:|---:|
| `/projects/overview` | **1833 ms** | — | — |
| `/management/expense-report` | **2456 ms** | 241 ms | 228 ms |
| `/management/report` | 1279 ms | — | — |
| `/management/order` | 1012 ms | — | — |
| `/projects/performance` | 909 ms | — | — |
| `/projects/plan` | 906 ms | 132 ms | **14 ms** |
| `/management/overview` | 874 ms | — | — |
| `/management/account` | 389 ms | 91 ms | 27 ms |
| `/projects/docs` | 340 ms | — | — |
| `/` | 230 ms | — | — |
| `/home/content/calendar` | 74 ms | 52 ms | 23 ms |

Trang `/projects/plan` chạy **906 ms lúc nguội hoàn toàn và 14 ms lúc ấm**.

### Nguyên nhân

Mã nguồn gọi `Cache::` **168 lần trong 68 file**, gồm cả `BaocaochiphiController` (báo cáo
chi phí) và `PerformanceController`. Restart FrankenPHP xoá opcache và RAM của worker,
**nhưng không xoá Redis** — nên cột giữa vẫn hưởng lợi từ cache ứng dụng đã ấm. Cột trái là
lần đầu tiên trong ngày, khi Redis cũng trống.

> Chưa kiểm chứng trực tiếp được phần này: muốn tách bạch phải xoá Redis cache, mà Redis đang
> dùng chung với đội nên không làm. Kết luận trên là suy ra từ số lần gọi `Cache::` và từ
> việc restart server không tái lập được số liệu nguội ban đầu.

### Ý nghĩa vận hành

Sau mỗi lần deploy, **người dùng đầu tiên vào mỗi trang phải chịu độ trễ cao gấp nhiều lần**
mức bình thường — với trang báo cáo là 2,4 giây. Nên thêm một bước "làm nóng": gọi tuần tự
các trang chính ngay sau khi restart, trước khi mở cho người dùng.

---

## 7. Các đòn bẩy tối ưu đã thử

Đo trên `/login` tại conc = 50, đơn vị req/s:

| Cấu hình | Worker | Classic |
|---|---:|---:|
| Chỉ bật worker mode | 2663 | 366 |
| + cache Laravel + opcache tuning | 2643 | 577 |
| + `APP_DEBUG=false` | 2668 | 600 |
| + JIT tracing 128 MB | 2683 | 474 |
| Gỡ cache — **cấu hình chốt** | 2925 | 357 |

**Cột worker gần như đứng yên qua mọi cấu hình. Chỉ classic mode phản ứng.**

| Đòn bẩy | Kết luận | Số đo |
|---|---|---|
| Worker mode (Laravel Octane) | ✅ **Áp dụng** | 366 → 2663 req/s, trước cả khi tinh chỉnh gì thêm |
| `config`/`route`/`event` cache + `validate_timestamps=0` | ❌ **Loại bỏ** | Worker 2663 → 2643 (nhiễu). Classic 366 → 577 (+58%) |
| `APP_DEBUG=false` + `APP_ENV=production` | ➖ **Không đổi** | Không khác biệt đo được ở cả hai chế độ |
| opcache JIT tracing 128 MB | ❌ **Loại bỏ** | **Chậm hơn.** Worker conc 1: 2,2 → 4,4 ms. Classic conc 50: 577 → 474 req/s |
| opcache memory 512 MB, interned 32 MB | ✅ **Giữ** | Không đo được khác biệt, nhưng vô hại |

**Vì sao cache không giúp worker mode:** `config:cache` và `route:cache` tiết kiệm công đọc
file lúc bootstrap, mà worker mode chỉ bootstrap đúng một lần. Classic mode trả khoản đó mỗi
request nên mới được +58%. Đã gỡ cache vì chỉ dùng worker mode, và cache buộc phải rebuild
sau mỗi lần sửa mã nguồn.

**Vì sao JIT làm chậm:** mã Laravel chủ yếu là I/O và thao tác mảng/chuỗi, không phải vòng
lặp số học mà JIT tối ưu được. Đây là kết quả bình thường với web app.

---

## 8. Bốn bẫy đã gặp

### 8.1. Bản FrankenPHP Windows không kèm `php.ini`

```
Fatal error: Failed opening required '-v' (include_path='.;C:\php\pear')
```

Mặc định chỉ có khoảng 20 extension built-in. Thiếu `pdo_mysql`, `mbstring`, `openssl`,
`curl`, `gd`, `zip` — Laravel không chạy nổi. `extension_dir` lại trỏ sẵn vào `C:\php\ext`
của một bản PHP khác, **không tương thích ABI**.

**Xử lý:** DLL đã có sẵn trong `~/.frankenphp/ext/`. Tự tạo `~/.frankenphp/php.ini` với
`extension_dir` trỏ đúng vào đó. Cùng thư mục cũng có `php.exe` 8.5 để chạy artisan/composer.

### 8.2. `composer.lock` chặn PHP 8.5

```
lcobucci/clock 2.3.0 requires php ~8.1.0 || ~8.2.0 -> your php version (8.5.10) does not satisfy that requirement
phpoffice/phpspreadsheet 1.30.6 requires php >=7.4.0 <8.5.0
```

Ba package cap dưới 8.5: `lcobucci/clock`, `lcobucci/jwt`, `phpoffice/phpspreadsheet`. Chạy
`composer update` sẽ gỡ được nhưng làm lệch version so với những gì cả đội đang test.

**Xử lý:** cài bằng `--ignore-platform-req=php` để giữ nguyên lock file.

> ⚠️ Đổi lại, phần **export Excel** (`maatwebsite/excel`) và **JWT** đang chạy ngoài dải
> version tác giả hỗ trợ — cần test kỹ hai nhóm đó. Muốn sạch hoàn toàn thì dùng bản
> FrankenPHP build PHP 8.4.

### 8.3. 12 worker tranh nhau tạo `services.php`

```
rename(bootstrap\cache\serF6A4.tmp, bootstrap\cache/services.php): Access is denied (code: 5)
Error: failed to initialize workers: worker frankenphp-worker.php has not reached frankenphp_handle_request()
```

Bẫy riêng của worker mode. Sau `artisan optimize:clear`, file `bootstrap/cache/services.php`
biến mất, rồi cả 12 worker cùng lúc sinh lại nó khi khởi động — tranh chấp `rename()` trên
Windows, worker chết hàng loạt, FrankenPHP abort. Classic mode không gặp vì mỗi request là
một tiến trình tuần tự.

**Xử lý:** chạy `artisan package:discover` bằng **một tiến trình đơn** trước khi start server,
và xoá `bootstrap\cache\*.tmp` còn sót. `start.bat` và `reload.bat` đã làm sẵn bước này.

### 8.4. `artisan view:cache` luôn thất bại

```
Unable to locate a class or view for component [laravel-exceptions-renderer::syntax-highlight]
```

Laravel cố compile blade component của chính trang báo lỗi nội bộ, nhưng component đó không
resolve được lúc build. Vô hại với runtime, nhưng nó làm `artisan optimize` **dừng giữa chừng** —
ba cache đầu chạy xong, view cache hỏng, lệnh trả về như thể đã hoàn tất.

**Xử lý:** nếu cần cache, gọi `config:cache`, `route:cache`, `event:cache` riêng lẻ thay vì
`artisan optimize`.

---

## 9. Giới hạn của phép đo

Những điều báo cáo này **không** chứng minh:

1. **Rủi ro rò rỉ state chưa được kiểm chứng.** Worker mode giữ ứng dụng trong RAM giữa các
   request, nên singleton, biến static và global trong mã nguồn PMS có thể mang dữ liệu từ
   request này sang request khác — giữa những người dùng khác nhau. Cả hai vòng đo đều chỉ
   dùng **một** tài khoản nên không thể phát hiện việc này. Bằng chứng hiện có chỉ giới hạn ở
   trang login: 12/12 CSRF token khác nhau, 6/6 session cookie khác nhau, 0 lỗi sau ~2000
   request. **Cần test bằng ít nhất hai tài khoản khác nhau chạy đồng thời** trước khi dùng
   worker mode ngoài mục đích đo đạc.

2. **Một session duy nhất cho mọi request đồng thời.** Cả 20 luồng dùng chung cookie của
   `admin`. Hai mươi người dùng khác nhau sẽ có profile khác — cache theo người dùng không
   dùng chung được, và ghi session sẽ phân tán hơn. Số thật ở conc cao nhiều khả năng
   **thấp hơn** con số trong báo cáo này.

3. **Chỉ một bộ dữ liệu.** `admin` thấy toàn bộ dự án nên các trang danh sách trả về tập dữ
   liệu lớn nhất. Người dùng thường sẽ nhẹ hơn.

4. **Máy đo trùng máy chạy.** Công cụ benchmark chiếm CPU của chính server. Số thật khi đo từ
   máy khác qua mạng sẽ khác — nhiều khả năng thấp hơn ở mức đồng thời cao.

5. **Không chạy migration, không ghi dữ liệu.** Database trỏ vào server dùng chung của đội
   với dữ liệu thật. Toàn bộ request đều là GET.

---

## 10. Khuyến nghị

| Việc | Lý do |
|---|---|
| **Giữ worker mode** | Vẫn lợi 23–59 ms mỗi request, miễn phí sau khi đã cấu hình xong |
| **Đừng kỳ vọng ×8 trên trang thật** | Con số thực tế là ×1,2 đến ×4,5 |
| **Tối ưu `/management/expense-report`** | Bão hoà ở 15 req/s, p99 tới 1,8 giây ở conc 20. Đây là nút thắt thật |
| **Xem lại `/projects/overview`** | 1833 ms lúc nguội — chậm nhất trong toàn bộ khảo sát |
| **Thêm bước làm nóng sau deploy** | Request đầu chậm gấp 24 lần; trang báo cáo tới 2,4 giây |
| **Test rò rỉ state bằng 2 tài khoản** | Chưa làm được, và là rủi ro chặn việc dùng thật |

---

## 11. Cấu hình đang chạy

| Hạng mục | Giá trị |
|---|---|
| Worker mode | `localhost:9080` |
| Classic mode | `localhost:9081` |
| Mã nguồn | `D:\FrankenPHP\app` |
| Caddyfile | `D:\FrankenPHP\Caddyfile` |
| php.ini | `C:\Users\minhpham\.frankenphp\php.ini` |
| Cache Laravel | đã xoá hết |
| `opcache.validate_timestamps` | `1` — tự phát hiện file đổi |
| `opcache.jit` | `disable` |
| `APP_ENV` / `APP_DEBUG` | `local` / `true` |
| RAM tiến trình | 317 MB · 46 luồng |

### Script

| Script | Dùng khi |
|---|---|
| `start.bat` | Khởi động cả hai port, có bước chuẩn bị bootstrap cache |
| `stop.bat` | Dừng server |
| `reload.bat` | **Restart sau khi sửa mã nguồn** — bắt buộc, vì worker giữ app trong RAM |
| `update.bat` | Pull branch `dev`, cài dependency, restart |
| `bench.bat` | Đo lại, ví dụ `bench.bat /login 500` |

Sửa mã nguồn **không có tác dụng** trên port 9080 cho tới khi chạy `reload.bat` — không phải
do cache Laravel, mà vì worker giữ ứng dụng trong RAM. Port 9081 (classic) thì thấy ngay, nên
tiện để vừa dev vừa kiểm tra lại trên 9080.

### Backup cấu hình

- `Caddyfile.classic.bak` — Caddyfile trước khi bật worker mode
- `~/.frankenphp/php.ini.bak` — php.ini trước khi tinh chỉnh opcache
- `app/.env.debugtrue.bak` — .env trước khi thử `APP_DEBUG=false`

---

## 12. Tái lập phép đo

```bash
# 1. Bật server
D:\FrankenPHP\start.bat

# 2. Đăng nhập, lấy cookie
TOKEN=$(curl -s -c jar.txt http://localhost:9080/login \
  | grep -oE 'name="_token" value="[^"]*"' | head -1 | sed -E 's/.*value="([^"]*)".*/\1/')
curl -s -b jar.txt -c jar.txt -X POST http://localhost:9080/login \
  -d "_token=$TOKEN" -d "txtUsername=admin" -d "txtPassword=..."
COOKIE=$(awk 'NF==7 {printf "%s=%s; ", $6, $7}' jar.txt)

# 3. Đo — bench.php <url> <tổng> <đồng thời> <nhãn> <cookie> <warmup>
php bench.php "http://localhost:9080/projects/plan" 40 5 "WORKER" "$COOKIE" 8
php bench.php "http://localhost:9081/projects/plan" 40 5 "CLASSIC" "$COOKIE" 8
```

Lưu ý khi phân tích cookie jar: dòng của cookie `httponly` bắt đầu bằng `#HttpOnly_`, nên bộ
lọc bỏ dòng `#` sẽ **mất cookie session** và mọi request trả 302.

---

## 13. Thay đổi đã tạo ra trên repo

`composer.json` và `composer.lock` giờ có thêm **`laravel/octane ^2.19`**. Nếu đội chưa chốt
dùng Octane thì revert bằng:

```bash
cd D:\FrankenPHP\app
git checkout composer.json composer.lock
```

`.env` cũng bị sửa một dòng: `APP_URL=http://localhost:9080`.
