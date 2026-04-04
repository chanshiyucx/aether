代码整体质量很高，结构清晰，错误处理规范，增量逻辑完整。下面按严重程度分层说。

## 🔴 需要修复的问题

<!-- **1. manifest 的 reuse key 用错了字段（第 459 行）**

```rust
// 现在的代码
.map(|photo| (photo.original.url.clone(), photo))

// try_reuse 里查找用的是
previous_manifest.photos_by_key.get(&item.original_key)?
```

`original.url` 存的是完整 URL（`https://img.example.com/...`），但 `original_key` 是相对路径（`originals/20240209-Paris/IMG_1234.HIF`），两者永远不会匹配，导致**所有图片每次都会被重新处理，增量逻辑完全失效**。

应该用 `sourceKey` 作为 map 的 key，或者在 `PhotoEntry` 里单独存一个 `source_key` 字段用于索引。 -->

**2. BlurHash 在原图上计算（第 295、426 行）**

```rust
// process_one 里
let blurhash = if config.enable_blurhash {
    Some(compute_blurhash(&image)?)  // image 是原始解码后的大图
};

// compute_blurhash 里虽然也缩了一次
let reduced = image.thumbnail(32, 32).to_rgba8();
```

问题是 `image.thumbnail()` 内部会对原图做一次完整遍历，原图是 6240×4160 的话内存和计算开销都很大。应该复用已经生成的 `thumbnail_image`，直接对缩略图再缩到 32×32，而不是对原图操作：

```rust
let blurhash = if config.enable_blurhash {
    Some(compute_blurhash(&thumbnail_image)?)  // 复用缩略图
};
```

**3. `decode_heif_image` 忽略了 EXIF 旋转（第 365 行）**

`libheif_rs` 解码出来的像素数据是原始方向，没有应用 EXIF orientation。富士 X-T5 竖拍的照片会出现方向错误的缩略图。需要在解码后检查 `handle.image_handle()` 的 `transform_image_metadata()` 或者手动读取 EXIF 旋转信息再做变换。

---

## 🟡 值得改进的地方

**4. `resize_image` 使用 `image` crate 而不是 `fast_image_resize`（第 340 行）**

你的依赖里有 `fast_image_resize` 但实际缩放用的是 `image::resize_exact`。`fast_image_resize` 的 Lanczos3 在大图上性能差异显著，这里没有用到是个遗漏。

**5. `now_rfc3339()` 在每张图片处理时调用（第 313 行）**

```rust
state_entry: StateEntry {
    processed_at: now_rfc3339()?,  // 并行处理时每张图各调一次
```

这个无关紧要，但语义上 `processedAt` 代表的是整批构建的时间还是单张图的时间？如果是前者，应该在 `run()` 开始时生成一次时间戳，作为参数传入。

**6. `Location.alt` 不是 Option（第 655 行）**

```rust
struct Location {
    alt: f64,  // GPS 高度经常缺失
```

很多照片没有 GPS 高度数据，这里应该是 `Option<f64>`，否则缺失时会写入 `0.0`，语义上是错误的。同理 `Camera` 里的 `lens`、`focal_length_mm` 也可能缺失。

**7. `write_json` 直接覆盖原文件，没有原子写入（第 502 行）**

```rust
let file = File::create(path)?;  // 直接创建/截断
serde_json::to_writer_pretty(writer, value)?;
```

如果写入过程中进程崩溃（比如磁盘满），会留下一个损坏的 manifest.json 或 state.json，下次运行时解析失败就会退化为全量重建。建议先写到临时文件再 `rename`：

```rust
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let file = File::create(&tmp)?;
    serde_json::to_writer_pretty(BufWriter::new(file), value)?;
    fs::rename(&tmp, path)?;  // 原子替换
    Ok(())
}
```

**8. `EXIF / taken_at / location / camera` 全部写死为 `None`（第 332-335 行）**

```rust
taken_at: None,
location: None,
camera: None,
image: None,
```

这是明显的未完成部分，但从代码结构上看你已经预留了完整的数据结构，只是还没接入 EXIF 解析库。建议加一个 `// TODO: parse EXIF` 注释，或者在 README 里标注，避免遗忘。

---

## 🟢 做得好的地方

- `cleanup_removed_outputs` 先于并行处理执行，删除逻辑正确
- `try_reuse` 同时验证 state 指纹和缩略图文件是否实际存在，防御性很好
- `decode_heif_image` 里手动处理 stride padding 是正确的，不少实现会在这里出 bug
- `anyhow` + `with_context` 的错误链很完整，出问题时定位方便
- `BufWriter` 包装文件写入，避免频繁 syscall

---

## 优先级建议

最需要立刻修复的是 **第 1 条（增量逻辑失效）** 和 **第 3 条（HEIF 旋转）**，这两个会直接影响功能正确性。第 2 条（BlurHash 性能）和第 7 条（原子写入）次之。
