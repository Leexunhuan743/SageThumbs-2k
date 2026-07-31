# SageThumbs 2K — 手动测试文档（v1.4.1，Lepton encode 版）

本版构建：`dist\SageThumbs2K-Setup-1.4.1.exe`（**Compact 包**，见 §5 差异说明）。
新增核心功能：**Convert into ▸ LEP**（JPEG 无损重压缩为 Lepton 容器）、Convert 对话框 LEP 目标、.lep 编辑保持 .lep、编码面崩溃加固。

---

## 1. 安装

1. 双击 `dist\SageThumbs2K-Setup-1.4.1.exe`，按向导安装（需管理员）。
2. 安装器会 `regsvr32` 注册 shell 扩展 DLL，并信任自签名证书（Win11 现代菜单用，本 Compact 包跳过该步骤）。
3. 安装完成后**重启 Explorer**（任务管理器 → Windows 资源管理器 → 重新启动），或注销重登，让右键菜单生效。

**验证安装成功**：
- 打开任意文件夹，空白处右键 → 菜单含 "SageThumbs 2K" 项。
- 对任意图片右键 → 出现缩略图/预览相关菜单项（"Convert into ▸"、"Resize"、"Rotate" 等）。

---

## 2. Lepton 编码功能测试（本版重点）

### 2.1 右键菜单 Convert into ▸ LEP（无损路径）

**准备**：一张普通 JPEG（如手机照片 `photo.jpg`，建议带 EXIF）。

**步骤**：
1. 右键 `photo.jpg` → **Convert into ▸ LEP**。
2. 预期：同目录生成 `photo.lep`，无错误弹窗。
3. 验证无损：
   ```
   st2k convert photo.lep photo_back.jpg
   ```
   然后用文件比较工具（或 `fc /b`）对比 `photo.jpg` 与 `photo_back.jpg` —— **必须逐字节一致**（EXIF 也保留）。
4. 预期 `photo.lep` 体积 ≤ `photo.jpg`（通常小 3-10%）。

### 2.2 右键菜单：非 JPEG 源被明确拒绝

**步骤**：
1. 右键一张 PNG → **Convert into ▸ LEP**。
2. 预期：转换失败提示（不会静默生成 .lep，也不崩溃）。
3. CLI 验证（st2k 在安装目录）：
   ```
   st2k convert photo.png out.lep
   ```
   预期 stderr：`convert failed: ...: lepton output requires a JPEG source (jpg/jpeg/jpe/jfif)`，退出码非 0。

### 2.3 Convert 对话框（Convert…）

**步骤**：
1. 右键一张 JPEG → **Convert…**（打开转换对话框）。
2. 格式下拉框 → 选 **LEP — Lepton (lossless JPEG)**（位于列表末尾）。
3. 预期（本版专项，验证 settings_kind 修复）：
   - **"Settings…" 按钮必须无反应或不可用** —— 绝不能弹出 "AVIF / JPEG XL quality" 弹窗（Compact 无 magick 安装下，旧版会误开该弹窗）。
4. 点 OK → 生成 `.lep` 文件。

### 2.4 对话框 LEP 门控（混合选择禁用）

**准备**：一个文件夹里同时放 1 张 JPEG + 1 张 PNG，全选后右键 → **Convert…**。

**步骤**：
1. 全选两文件，打开 Convert 对话框，选 LEP 目标。
2. 预期：**确定按钮被禁用**，并显示警告行（"Lepton requires JPEG sources" 之类）。
3. 勾选 **Resize** 复选框 → 预期：**确定按钮恢复可用**（resize 路径接受任意可解码源，有损重编码）。
4. 取消勾选 Resize → 确定按钮再次禁用。

### 2.5 编辑保持 .lep

**准备**：用 §2.1 生成的 `photo.lep`。

**步骤**：
1. 右键 `photo.lep` → **Resize** → 选一个尺寸（如 50%）。
2. 预期：生成 `photo (resized).lep`（**不是 .png**），且能正常解码查看。
3. 右键 `photo.lep` → **Rotate**（如右转 90°）→ 预期生成 `photo (edited).lep`。

### 2.6 CLI 冒烟（完整契约）

```powershell
# 无损路径
st2k convert photo.jpg out.lep          # 成功；decode 回源逐字节一致
# 有损路径（带 resize）
st2k convert photo.jpg small.lep --resize 200x200
# .lep 源转回（有损重编码）
st2k convert photo.lep back.jpg
# 超预算 JPEG（>128 MiB）→ 明确尺寸错误
st2k convert huge.jpg huge.lep         # stderr: "exceeds the 128 MiB lepton container budget"
# .mpo 拒绝（扩展名门，即使内容是真 JPEG）
st2k convert photo.mpo out.lep         # "requires a JPEG source"
```

---

## 3. 回归测试点（确认既有功能未被破坏）

1. **缩略图/预览**：浏览含 jpg/png/gif/webp/heic 的文件夹，缩略图正常；双击看大图正常。
2. **.lep 解码**：打开一个既有 `.lep` 文件，缩略图/预览正常（本版仍完整支持解码）。
3. **OCR**：右键图片 → Copy text，中文/英文识别正常。
4. **Strip metadata**：右键 jpg/png → Strip metadata，文件体积变小。
5. **PDF/CBZ/壁纸**：PDF 首图缩略图、CBZ 封面、右键 Set as wallpaper 正常。
6. **对话框其他目标**：Convert… 选 JPEG/PNG/WebP 转换正常（质量滑块按目标出现/隐藏正确）。
7. **并发稳定性**：全选一个文件夹（≥10 张图）批量 Resize/Convert，全部完成、无崩溃、无残留 .st2ktmp 文件。

---

## 4. 崩溃安全（本版加固项，抽测）

- 用损坏的 JPEG（用记事本改坏几个字节、或截断文件）右键 Convert into ▸ LEP：**不崩溃**（失败或成功均可，进程不得消失）。
- 磁盘写满/目标目录只读时转换：**干净报错**，不崩 dllhost/explorer。
- 连续快速右键多个大图 Convert into ▸ LEP：并发受信号量约束，全部完成不死锁（内存上限 2 并发）。

---

## 5. 本 Compact 包已知差异（构建于 `-NoImageMagick -NoModernMenu`）

| 项 | 本包 | 完整版（正式发布） |
|---|---|---|
| ImageMagick 转换目标（AVIF/JXL/PSD/DDS/…） | **不可用**（对话框不显示，CLI 拒绝） | 可用（捆绑 100+ 格式引擎） |
| 解码/预览 100+ 格式（HEIC/AVIF/相机 RAW…） | **不可用**（WIC/纯 Rust 之外的回退缺失） | 可用 |
| Win11 现代右键菜单（稀疏包） | **未注册**（仅经典菜单） | 注册 |
| 纯 Rust 格式（jpg/png/gif/webp/tiff/…） | 完整 | 完整 |
| **Lepton 解码 + 编码（无损/有损）** | **完整** | 完整 |

> 需要完整版：安装 ImageMagick 7.1.2-25 Q16-HDRI (64-bit) 后用完整参数重跑发布管线。

---

## 6. 已知限制（设计决定，非缺陷）

- `.lep` 编辑（Resize/Rotate）= **有损重编码**（与 .jpg 编辑同）；仅"纯 JPEG 源 → Convert into ▸ LEP"是无损字节级。
- PNG→LEP 不做隐式 JPEG 化，明确报错（防误操作）。
- `.mpo`（多图 JPEG）拒绝进入 LEP（防静默变单帧）。
- 批量 `--to lep`：非 JPEG 源计 skipped（统计中单列），全部 skipped 时退出码非 0。
