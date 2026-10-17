# Douyin Downloader

Ứng dụng desktop cho Windows giúp tải nội dung Douyin được đăng công khai: dán
một liên kết bài viết hoặc liên kết trang cá nhân, xem trước những gì có sẵn,
rồi chọn phần mình muốn lưu.

Ứng dụng được viết bằng **Tauri 2** (lõi Rust + giao diện React/TypeScript).
Toàn bộ dữ liệu — cơ sở dữ liệu, lịch sử, tệp đã tải — nằm trên máy của bạn.
Không có máy chủ trung gian nào nhận hay lưu dữ liệu của bạn.

## Tính năng chính

- **Tải từ một liên kết (Single URL).** Dán một liên kết bài viết Douyin; ứng
  dụng phân tích và hiển thị trước nội dung (video, ảnh, bộ ảnh, hoặc bài chỉ có
  chữ), rồi bạn chọn phiên bản muốn tải.
- **Tải từ trang cá nhân (Profile Downloader).** Dán liên kết trang cá nhân,
  duyệt danh sách bài viết công khai theo từng trang, lọc theo loại nội dung và
  chọn nhiều bài để đưa vào hàng đợi cùng lúc.
- **Hàng đợi tải xuống.** Theo dõi tiến độ, tốc độ và thời gian còn lại của từng
  tác vụ. Có thể hủy, tạm dừng, tiếp tục và thử lại — những nút này chỉ xuất hiện
  khi tác vụ thực sự hỗ trợ thao tác đó.
- **Thư viện.** Tìm kiếm, lọc và duyệt mọi nội dung ứng dụng đã phát hiện hoặc đã
  tải, kèm trạng thái của tệp trên đĩa. Thư viện phân biệt rõ ba trường hợp: tệp
  còn đó, tệp đã mất, và tệp không thể kiểm tra được (ví dụ ổ đĩa chưa gắn).
- **Lịch sử tải xuống.** Ghi lại mọi lượt tải đã kết thúc — kể cả lượt thất bại
  hoặc đã hủy — cùng tệp mà lượt tải đó tạo ra.
- **Cài đặt.** Chọn thư mục lưu, quy tắc đặt tên tệp và thư mục, cách sắp xếp
  theo người sáng tạo hoặc theo loại nội dung, số lượt tải đồng thời, cùng các
  tùy chọn nâng cao.

## Nền tảng được hỗ trợ

- **Windows x64.**
- Đã kiểm thử trên Windows 11 với phiên bản **1.0.1**.

## Cài đặt cho người dùng

**Hiện chưa có bản phát hành công khai.** Kho mã nguồn này chỉ chứa mã nguồn;
chưa có GitHub Release và chưa có tệp cài đặt nào được đính kèm. Nếu sau này có
bản phát hành, tệp cài đặt sẽ nằm ở mục **Releases** của kho này.

Trong lúc chờ, bạn có thể tự tạo bộ cài từ mã nguồn — xem mục *Dành cho nhà phát
triển* bên dưới. Quá trình build tạo ra hai loại bộ cài:

| Bộ cài | Phạm vi | Ghi chú |
| --- | --- | --- |
| **NSIS** (`*-setup.exe`) | Chỉ người dùng hiện tại | **Khuyến nghị.** Cài vào thư mục của riêng bạn, không cần quyền quản trị. |
| **MSI** (`*.msi`) | Toàn máy | Cài cho mọi người dùng trên máy, nên Windows sẽ yêu cầu quyền quản trị. |

Bộ cài chỉ đặt tệp chương trình. Dữ liệu của bạn — cơ sở dữ liệu, lịch sử, cài
đặt và các tệp đã tải — nằm ở thư mục dữ liệu riêng và **không** bị bộ cài xóa
hay ghi đè khi cập nhật.

## Dành cho nhà phát triển

### Yêu cầu

| Thành phần | Phiên bản | Vì sao cần |
| --- | --- | --- |
| Windows x64 | 11, hoặc Windows 10 còn được hỗ trợ | Nền tảng mục tiêu. |
| WebView2 runtime | — | Có sẵn trên Windows 11. Trên Windows 10, bộ cài sẽ tự cài. |
| Node.js | 24.x | Build phần giao diện. |
| Rust | 1.77 trở lên | Lõi ứng dụng, toolchain `x86_64-pc-windows-msvc`. |
| MSVC build tools | Visual Studio 2022 C++ | Trình liên kết cho lõi Rust. |
| Windows SDK | 10/11 | Tauri cần cho WebView2 và Common Controls. |
| FFmpeg | bản gần đây | **Tùy chọn.** Cần khi Douyin phục vụ video và âm thanh thành hai luồng riêng: ứng dụng tải cả hai rồi ghép lại thành một tệp MP4. Việc ghép là sao chép luồng (`-c copy`), không mã hóa lại. Nếu máy chưa có FFmpeg, những video như vậy sẽ báo lỗi nêu rõ tên FFmpeg thay vì lặng lẽ lưu tệp không có tiếng. |
| Chrome hoặc Edge | — | **Khuyến nghị.** Một số trang Douyin chỉ đưa nội dung cho trình duyệt thật; ứng dụng dùng Chrome/Edge để đọc những trang đó. |

### Các lệnh

```bash
cd code
npm install
npm run tauri:dev      # chạy ứng dụng ở chế độ phát triển
npm run tauri:build    # tạo bộ cài NSIS và MSI trong src-tauri/target/release/bundle/
```

### Kiểm tra chất lượng

```bash
cd code
npm run typecheck      # kiểm tra kiểu TypeScript
npm test               # bộ kiểm thử giao diện (Vitest)
npm run build          # build giao diện cho môi trường thật
cd src-tauri
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Một số kiểm thử trong `src-tauri/tests/live_*.rs` cố ý bị đánh dấu `#[ignore]`:
chúng cần mạng thật và một trình duyệt đã cài, nên bộ kiểm thử mặc định không phụ
thuộc vào việc Douyin có truy cập được hay không.

## Xử lý sự cố

**Tải video báo lỗi cần FFmpeg.** Douyin phục vụ hình và tiếng thành hai luồng
riêng cho nhiều video. Ứng dụng cần FFmpeg để ghép chúng thành một tệp có tiếng.
Hãy cài FFmpeg, hoặc trỏ ứng dụng tới tệp thực thi bằng biến môi trường
`DOUYIN_DOWNLOADER_FFMPEG`.

**Không đọc được trang cá nhân.** Một số trang chỉ hiển thị bài viết cho người
đã đăng nhập. Ứng dụng có phần đăng nhập riêng: nó mở một cửa sổ Douyin để bạn tự
đăng nhập, và lưu phiên đó trong hồ sơ trình duyệt **riêng của ứng dụng** — không
đụng tới trình duyệt hằng ngày của bạn. Việc dùng phiên đăng nhập là tùy chọn và
mặc định tắt.

**Ứng dụng báo "cần trình duyệt".** Hãy cài Chrome hoặc Edge rồi mở lại ứng dụng.

**Bấm Refresh không thấy tệp mới.** Nút *Check files* trong màn hình Tải xuống và
Lịch sử sẽ đọc lại ổ đĩa; thao tác này tách biệt với việc đọc hàng đợi.

## Lưu ý về sử dụng có trách nhiệm

Ứng dụng này chỉ dành cho **nội dung công khai**. Hãy chỉ tải những nội dung mà
bạn được phép lưu lại, tôn trọng quyền của người sáng tạo và tuân thủ quy định
của Douyin cũng như pháp luật áp dụng cho bạn.

Ứng dụng không vượt qua kiểm soát truy cập, không truy cập nội dung riêng tư, và
không có tính năng xóa watermark: khi nguồn có sẵn bản gốc sạch, ứng dụng sẽ ưu
tiên bản đó, nhưng không chỉnh sửa nội dung để tạo ra nó.

## Giấy phép

**Dự án chưa chọn giấy phép.** Kho mã nguồn không kèm tệp `LICENSE` và không có
trường giấy phép trong `package.json` hay `Cargo.toml`. Theo mặc định của luật
bản quyền, điều đó có nghĩa là **mọi quyền được bảo lưu** — bạn có thể đọc mã
nguồn, nhưng chưa có giấy phép nào cho phép sử dụng, sao chép, sửa đổi hay phân
phối lại. Nếu bạn muốn dự án có giấy phép nguồn mở, hãy thêm giấy phép một cách
có chủ đích; tệp này không tự ý chọn thay bạn.
