# Băng thử FrankenPHP — PMS Website

Báo cáo đo hiệu suất worker mode so với classic mode, kèm các bẫy gặp phải khi triển khai.

- **Ngày đo:** 15/09/2026
- **Bản web:** https://claude.ai/artifact/1eYrM6TdAsnAWZ35fLXeZr (có biểu đồ tương tác)

---

## 1. Môi trường đo

| Hạng mục | Giá trị |
|---|---|
| Máy | Windows 11 Pro 26200 · 12 nhân |
| Server | FrankenPHP 1.12.7 · Caddy 2.11.4 |
| PHP | 8.5.10 ZTS · opcache bật · JIT tắt |
| Ứng dụng | `pms-website-2` @ branch `dev` · commit `ee4608daf` · Laravel 12 |
| Luồng | `num_threads 24` · `worker num 12` |
| Trang đo | `GET /login` — **không chạm database** |
| Mẫu | 400 request mỗi kịch bản, có warm-up |
| Công cụ | `bench.php` · curl_multi · HTTP/1.1 |
| Backend | MySQL 8.0.44 + Redis, chung LAN tại `192.168.8.148` |

Hai chế độ chạy **song song trong cùng một tiến trình FrankenPHP**, cùng 12 luồng PHP,
cùng mã nguồn, cùng backend — nên đây là so sánh cân bằng chứ không phải hai lần đo rời rạc.

- Worker mode → `localhost:9080`
- Classic mode → `localhost:9081`

---

## 2. Kết quả chính

Worker mode nhanh hơn classic mode khoảng **8 lần**, ổn định qua cả ba mức tải.

| Kịch bản | avg | p50 | p95 | p99 | max | req/s |
|---|---:|---:|---:|---:|---:|---:|
| Worker · conc 1 | 2,7 | 2,5 | 3,5 | 4,0 | 22,1 | **369** |
| Classic · conc 1 | 22,1 | 15,9 | 37,9 | 41,9 | 46,8 | **45** |
| Worker · conc 10 | 3,7 | 3,3 | 6,0 | 8,4 | 11,6 | **2220** |
| Classic · conc 10 | 28,6 | 30,2 | 43,7 | 45,9 | 47,5 | **299** |
| Worker · conc 50 | 14,9 | 14,6 | 17,2 | 23,9 | 26,8 | **2925** |
| Classic · conc 50 | 126,2 | 128,7 | 153,5 | 164,6 | 177,6 | **357** |

*Đơn vị độ trễ: mili-giây. Tỷ lệ throughput: ×8,1 / ×7,4 / ×8,2.*

Classic mode bootstrap lại toàn bộ Laravel cho mỗi request. Worker mode khởi động một lần
rồi giữ ứng dụng trong RAM. Tỷ lệ giữ ổn định quanh mức tám lần ở cả ba mức tải, cho thấy
chi phí bootstrap là **hằng số cộng thêm vào mỗi request**, không phải hiệu ứng của tắc nghẽn.

---

## 3. Các đòn bẩy tối ưu đã thử

Throughput tại conc = 50, đơn vị req/s:

| Cấu hình | Worker | Classic |
|---|---:|---:|
| Chỉ bật worker mode | 2663 | 366 |
| + cache Laravel + opcache tuning | 2643 | 577 |
| + `APP_DEBUG=false` | 2668 | 600 |
| + JIT tracing 128 MB | 2683 | 474 |
| Gỡ cache — **cấu hình chốt** | 2925 | 357 |

**Cột worker gần như đứng yên qua mọi cấu hình. Chỉ classic mode phản ứng.**

### Phán quyết từng đòn bẩy

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

## 4. Bốn bẫy đã gặp

### 4.1. Bản FrankenPHP Windows không kèm `php.ini`

```
Fatal error: Failed opening required '-v' (include_path='.;C:\php\pear')
```

Mặc định chỉ có khoảng 20 extension built-in. Thiếu `pdo_mysql`, `mbstring`, `openssl`,
`curl`, `gd`, `zip` — Laravel không chạy nổi. `extension_dir` lại trỏ sẵn vào `C:\php\ext`
của một bản PHP khác, **không tương thích ABI**.

**Xử lý:** DLL đã có sẵn trong `~/.frankenphp/ext/`. Tự tạo `~/.frankenphp/php.ini` với
`extension_dir` trỏ đúng vào đó. Cùng thư mục cũng có `php.exe` 8.5 để chạy artisan/composer.

### 4.2. `composer.lock` chặn PHP 8.5

```
lcobucci/clock 2.3.0 requires php ~8.1.0 || ~8.2.0 -> your php version (8.5.10) does not satisfy that requirement
phpoffice/phpspreadsheet 1.30.6 requires php >=7.4.0 <8.5.0
```

Ba package cap dưới 8.5: `lcobucci/clock`, `lcobucci/jwt`, `phpoffice/phpspreadsheet`.
Chạy `composer update` sẽ gỡ được nhưng làm lệch version so với những gì cả đội đang test.

**Xử lý:** cài bằng `--ignore-platform-req=php` để giữ nguyên lock file.

> ⚠️ Đổi lại, phần **export Excel** (`maatwebsite/excel`) và **JWT** đang chạy ngoài dải
> version tác giả hỗ trợ — cần test kỹ hai nhóm đó. Muốn sạch hoàn toàn thì dùng bản
> FrankenPHP build PHP 8.4.

### 4.3. 12 worker tranh nhau tạo `services.php`

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

### 4.4. `artisan view:cache` luôn thất bại

```
Unable to locate a class or view for component [laravel-exceptions-renderer::syntax-highlight]
```

Laravel cố compile blade component của chính trang báo lỗi nội bộ, nhưng component đó không
resolve được lúc build. Vô hại với runtime, nhưng nó làm `artisan optimize` **dừng giữa chừng** —
ba cache đầu chạy xong, view cache hỏng, lệnh trả về như thể đã hoàn tất.

**Xử lý:** nếu cần cache, gọi `config:cache`, `route:cache`, `event:cache` riêng lẻ thay vì
`artisan optimize`.

---

## 5. Giới hạn của phép đo

Bốn điều phép đo này **không** chứng minh:

1. **Chỉ phủ chi phí framework.** `/login` gần như không chạm database. Toàn bộ chênh lệch
   tám lần là chi phí bootstrap Laravel, không phải tốc độ ứng dụng thật. Các trang có query
   MySQL/Redis sẽ thấp hơn đáng kể và khoảng cách giữa hai chế độ sẽ hẹp lại, vì lúc đó
   database mới là nút thắt. **Cần tài khoản test để đo các trang phải đăng nhập.**

2. **Rủi ro rò rỉ state chưa được kiểm chứng.** Worker mode giữ ứng dụng trong RAM giữa các
   request, nên singleton, biến static và global trong mã nguồn PMS có thể mang dữ liệu từ
   request này sang request khác — giữa những người dùng khác nhau. Kiểm tra hiện tại chỉ phủ
   trang login: 12/12 CSRF token khác nhau, 6/6 session cookie khác nhau, 0 lỗi sau ~2000
   request. Đó là bằng chứng cho trang login, **không phải cho cả ứng dụng**.

3. **Không chạy migration.** Database trỏ vào server dùng chung của đội tại `192.168.8.148`
   với dữ liệu thật. Không migration nào được chạy trong quá trình triển khai này.

4. **Máy đo trùng máy chạy.** Công cụ benchmark chạy trên chính máy đang phục vụ, nên phần
   CPU nó chiếm cũng lấy đi từ server. Số thật khi đo từ máy khác qua mạng sẽ khác — nhiều
   khả năng thấp hơn ở mức đồng thời cao.

---

## 6. Cấu hình đang chạy

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

### Lưu ý vận hành

Sửa mã nguồn **không có tác dụng** trên port 9080 cho tới khi chạy `reload.bat` — không phải
do cache Laravel nữa, mà vì worker giữ ứng dụng trong RAM. Port 9081 (classic) thì thấy ngay,
nên tiện để vừa dev vừa kiểm tra lại trên 9080.

### Backup cấu hình

- `Caddyfile.classic.bak` — Caddyfile trước khi bật worker mode
- `~/.frankenphp/php.ini.bak` — php.ini trước khi tinh chỉnh opcache
- `app/.env.debugtrue.bak` — .env trước khi thử `APP_DEBUG=false`

---

## 7. Thay đổi đã tạo ra trên repo

`composer.json` và `composer.lock` giờ có thêm **`laravel/octane ^2.19`**.
Nếu đội chưa chốt dùng Octane thì revert bằng:

```bash
cd D:\FrankenPHP\app
git checkout composer.json composer.lock
```

`.env` cũng bị sửa một dòng: `APP_URL=http://localhost:9080`.
