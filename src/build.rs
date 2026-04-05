use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    fs::File,
    io::{BufWriter, Cursor, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use blurhash::encode;
use fast_image_resize as fr;
use image::{
    ColorType, DynamicImage, GenericImageView, ImageBuffer, ImageFormat, ImageReader, Rgb, Rgba,
    codecs::jpeg::JpegEncoder,
};
use indicatif::{ProgressBar, ProgressStyle};
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
use plist::Value as PlistValue;
use ravif::{BitDepth as AvifBitDepth, ColorModel as AvifColorModel, Encoder as RavifEncoder, Img};
use rayon::{ThreadPoolBuilder, prelude::*};
use rgb::FromSlice;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::warn;
use walkdir::WalkDir;

use crate::config::{Config, ThumbnailFormat};

const SUPPORTED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "heif", "heic", "hif"];
const FINDER_TAGS_XATTR: &str = "com.apple.metadata:_kMDItemUserTags";
const AVIF_EXTENSION: &str = "avif";
const AVIF_MIME: &str = "image/avif";
const AVIF_SPEED: u8 = 8;

pub enum BuildExit {
    Success,
    PartialFailure,
}

pub fn run() -> Result<BuildExit> {
    let config = Config::load()?;

    if !config.source_tags().is_empty() && !cfg!(target_os = "macos") {
        bail!("sourceTags filtering currently only supports macOS");
    }

    let source_dir = config.source_path();
    let originals_dir = config.originals_path();
    let thumbnails_dir = config.thumbnails_path();
    let root_dir = config.root_dir();
    let started_at = Instant::now();

    if !source_dir.exists() {
        bail!("source directory does not exist: {}", source_dir.display());
    }

    fs::create_dir_all(&originals_dir).with_context(|| {
        format!(
            "failed to create originals directory {}",
            originals_dir.display()
        )
    })?;
    fs::create_dir_all(&thumbnails_dir).with_context(|| {
        format!(
            "failed to create thumbnails directory {}",
            thumbnails_dir.display()
        )
    })?;

    let previous_manifest = load_previous_manifest(&config.manifest_path())?;
    let previous_state = load_previous_state(&config.state_path())?;
    let mut sources = collect_selected_sources(&source_dir, config.source_tags())?;
    sources.sort_by(|left, right| left.source_key.cmp(&right.source_key));

    let total = sources.len();
    let parallelism = recommended_parallelism();
    let avif_threads = recommended_avif_threads(parallelism, total);

    println!("starting build");
    println!("root: {}", root_dir.display());
    println!("source: {}", source_dir.display());
    if !config.source_tags().is_empty() {
        println!("tags: {}", config.source_tags().join(", "));
    }
    println!("originals: {}", originals_dir.display());
    println!("thumbnails: {}", thumbnails_dir.display());
    if !config.source_tags().is_empty() {
        println!("found {} tagged images", total);
    } else {
        println!("found {} supported images", total);
    }
    println!("workers: {}", parallelism);

    let progress = ProgressBar::new(total as u64);
    progress.set_style(progress_style()?);
    progress.set_message("building");

    let current_keys: BTreeSet<_> = sources.iter().map(|item| item.source_key.clone()).collect();
    cleanup_removed_outputs(&root_dir, &current_keys, &previous_state)?;

    let mut photos = Vec::new();
    let mut files = BTreeMap::new();
    let mut pending = Vec::new();
    let mut reused_count = 0usize;

    for item in sources {
        if let Some((photo, state_entry)) =
            try_reuse(&root_dir, &item, &previous_manifest, &previous_state)
        {
            progress.inc(1);
            photos.push(photo);
            files.insert(item.source_key, state_entry);
            reused_count += 1;
        } else {
            pending.push(item);
        }
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(parallelism)
        .build()
        .map_err(|error| anyhow!("failed to build thread pool: {error}"))?;

    let results: Vec<_> = pool.install(|| {
        pending
            .par_iter()
            .map(|item| {
                let result = process_one(
                    &config,
                    &root_dir,
                    &originals_dir,
                    &thumbnails_dir,
                    config.avif_quality,
                    avif_threads,
                    item,
                );
                progress.inc(1);

                if let Err(error) = &result {
                    warn!("skipped {}: {error:#}", item.path.display());
                    progress.println(format!("failed {}: {error:#}", item.source_key));
                }

                result
            })
            .collect()
    });

    let mut failures = Vec::new();
    let mut built_count = 0usize;

    for result in results {
        match result {
            Ok(processed) => {
                files.insert(processed.state_key, processed.state_entry);
                photos.push(processed.photo_entry);
                built_count += 1;
            }
            Err(error) => failures.push(error.to_string()),
        }
    }

    photos.sort_by(|left, right| left.original.url.cmp(&right.original.url));

    let failed_count = failures.len();
    let now = now_rfc3339()?;
    write_json(
        &config.manifest_path(),
        &ManifestFile {
            version: 1,
            updated_at: now.clone(),
            photos,
        },
    )?;
    write_json(
        &config.state_path(),
        &StateFile {
            version: 1,
            updated_at: now,
            files,
        },
    )?;

    progress.finish_and_clear();

    println!(
        "build completed: {} built, {} reused, {} failed, elapsed {:.2?}",
        built_count,
        reused_count,
        failed_count,
        started_at.elapsed()
    );

    if failures.is_empty() {
        Ok(BuildExit::Success)
    } else {
        Ok(BuildExit::PartialFailure)
    }
}

fn collect_selected_sources(source_dir: &Path, source_tags: &[String]) -> Result<Vec<SourceItem>> {
    let mut candidates = Vec::new();

    for entry in WalkDir::new(source_dir) {
        let entry = entry?;
        let path = entry.path();

        if entry.file_type().is_dir() {
            continue;
        }

        if is_supported(path) {
            candidates.push(path.to_path_buf());
        }
    }

    let results: Vec<_> = candidates
        .par_iter()
        .map(|path| build_source_item(source_dir, path, source_tags))
        .collect();

    let mut selected = Vec::new();
    for result in results {
        if let Some(item) = result? {
            selected.push(item);
        }
    }

    Ok(selected)
}

fn build_source_item(
    source_dir: &Path,
    path: &Path,
    source_tags: &[String],
) -> Result<Option<SourceItem>> {
    if !source_tags.is_empty() && !has_any_finder_tag(path, source_tags)? {
        return Ok(None);
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    let relative_path = path
        .strip_prefix(source_dir)
        .with_context(|| format!("failed to strip source prefix from {}", path.display()))?
        .to_path_buf();

    Ok(Some(SourceItem {
        path: path.to_path_buf(),
        relative_path,
        source_key: normalize_relative_path(source_dir, path)?,
        size: metadata.len(),
        mtime_ms: metadata_mtime_ms(&metadata)?,
    }))
}

fn has_any_finder_tag(path: &Path, expected_tags: &[String]) -> Result<bool> {
    let tags = read_finder_tags(path)?;
    Ok(expected_tags
        .iter()
        .any(|expected| tags.iter().any(|tag| tag == expected)))
}

fn read_finder_tags(path: &Path) -> Result<Vec<String>> {
    let Some(raw) = xattr::get(path, FINDER_TAGS_XATTR)
        .with_context(|| format!("failed to read Finder tags for {}", path.display()))?
    else {
        return Ok(Vec::new());
    };

    let value = PlistValue::from_reader(Cursor::new(raw))
        .with_context(|| format!("failed to parse Finder tags for {}", path.display()))?;

    let tags = match value {
        PlistValue::Array(values) => values
            .into_iter()
            .filter_map(|value| match value {
                PlistValue::String(tag) => Some(normalize_finder_tag(tag)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    Ok(tags)
}

fn normalize_finder_tag(tag: String) -> String {
    tag.split_once('\n')
        .map(|(name, _)| name.to_string())
        .unwrap_or(tag)
}

fn try_reuse(
    root_dir: &Path,
    item: &SourceItem,
    previous_manifest: &LoadedManifest,
    previous_state: &StateFile,
) -> Option<(PhotoEntry, StateEntry)> {
    let state_entry = previous_state.files.get(&item.source_key)?;
    if state_entry.original.is_empty() {
        return None;
    }

    let photo_entry = previous_manifest.photos_by_key.get(&state_entry.original)?;

    if state_entry.size != item.size || state_entry.mtime_ms != item.mtime_ms {
        return None;
    }

    let original_path = root_dir.join(&state_entry.original);
    if !original_path.exists() {
        return None;
    }

    let thumbnail_path = root_dir.join(&state_entry.thumbnail);
    if !thumbnail_path.exists() {
        return None;
    }

    Some((photo_entry.clone(), state_entry.clone()))
}

fn cleanup_removed_outputs(
    root_dir: &Path,
    current_keys: &BTreeSet<String>,
    previous_state: &StateFile,
) -> Result<()> {
    for (source_key, state_entry) in &previous_state.files {
        if current_keys.contains(source_key) {
            continue;
        }

        remove_output_if_exists(root_dir, &state_entry.original)?;
        remove_output_if_exists(root_dir, &state_entry.thumbnail)?;
    }

    Ok(())
}

fn remove_output_if_exists(root_dir: &Path, relative_path: &str) -> Result<()> {
    if relative_path.is_empty() {
        return Ok(());
    }

    let path = root_dir.join(relative_path);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale output {}", path.display()))?;
    }

    Ok(())
}

fn process_one(
    config: &Config,
    root_dir: &Path,
    originals_dir: &Path,
    thumbnails_dir: &Path,
    avif_quality: u8,
    avif_threads: usize,
    item: &SourceItem,
) -> Result<ProcessedPhoto> {
    let loaded = load_image(&item.path)
        .with_context(|| format!("failed to decode {}", item.path.display()))?;
    let image = &loaded.image;

    let thumbnail_path = build_thumbnail_path(thumbnails_dir, config, &item.relative_path);
    if let Some(parent) = thumbnail_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let thumbnail_image = resize_image(&image, config.thumbnail_width)?;
    save_thumbnail(
        &thumbnail_image,
        &thumbnail_path,
        config.thumbnail_format,
        config.thumbnail_quality,
    )
    .with_context(|| format!("failed to write {}", thumbnail_path.display()))?;

    let original_path = build_original_path(originals_dir, &item.relative_path);
    if let Some(parent) = original_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    save_original_avif(&loaded, &original_path, avif_quality, avif_threads)
        .with_context(|| format!("failed to write {}", original_path.display()))?;

    let original_metadata = fs::metadata(&original_path)
        .with_context(|| format!("failed to read metadata for {}", original_path.display()))?;
    let thumbnail_metadata = fs::metadata(&thumbnail_path)
        .with_context(|| format!("failed to read metadata for {}", thumbnail_path.display()))?;
    let (original_width, original_height) = image.dimensions();
    let (thumbnail_width, thumbnail_height) = thumbnail_image.dimensions();

    let blurhash = if config.enable_blurhash {
        Some(compute_blurhash(&thumbnail_image)?)
    } else {
        None
    };

    let original_key = normalize_relative_path(root_dir, &original_path)?;
    let thumbnail_key = normalize_relative_path(root_dir, &thumbnail_path)?;
    let title = item
        .path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("missing file stem for {}", item.path.display()))?;

    Ok(ProcessedPhoto {
        state_key: item.source_key.clone(),
        state_entry: StateEntry {
            size: item.size,
            mtime_ms: item.mtime_ms,
            original: original_key.clone(),
            thumbnail: thumbnail_key.clone(),
            processed_at: now_rfc3339()?,
        },
        photo_entry: PhotoEntry {
            original: Asset {
                url: original_key,
                width: Some(original_width),
                height: Some(original_height),
                bytes: original_metadata.len(),
                mime: AVIF_MIME.to_string(),
            },
            thumbnail: Asset {
                url: thumbnail_key,
                width: Some(thumbnail_width),
                height: Some(thumbnail_height),
                bytes: thumbnail_metadata.len(),
                mime: mime_from_format(config.thumbnail_format).to_string(),
            },
            blurhash,
            title,
            taken_at: None,
            location: None,
            camera: None,
            image: None,
        },
    })
}

fn resize_image(image: &DynamicImage, target_width: u32) -> Result<DynamicImage> {
    let width = image.width();
    if width <= target_width {
        return Ok(image.clone());
    }

    let ratio = target_width as f64 / width as f64;
    let target_height = (image.height() as f64 * ratio).round() as u32;
    let target_height = target_height.max(1);
    let src = image.to_rgba8();
    let src_width = src.width();
    let src_height = src.height();
    let src_image =
        fr::images::Image::from_vec_u8(src_width, src_height, src.into_raw(), fr::PixelType::U8x4)
            .map_err(|error| anyhow!("failed to create resize source buffer: {error}"))?;
    let mut dst_image = fr::images::Image::new(target_width, target_height, fr::PixelType::U8x4);
    let options =
        fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    let mut resizer = fr::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, Some(&options))
        .map_err(|error| anyhow!("failed to resize image: {error}"))?;

    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(
        target_width,
        target_height,
        dst_image.into_vec(),
    )
    .ok_or_else(|| anyhow!("failed to build resized RGBA image buffer"))?;

    Ok(DynamicImage::ImageRgba8(buffer))
}

fn load_image(path: &Path) -> Result<LoadedImage> {
    if is_heif_family(path) {
        return decode_heif_image(path);
    }

    let image = ImageReader::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("failed to guess image format for {}", path.display()))?
        .decode()
        .with_context(|| format!("failed to decode {}", path.display()))?;

    Ok(LoadedImage {
        bit_depth: inferred_bit_depth(&image),
        image,
    })
}

fn decode_heif_image(path: &Path) -> Result<LoadedImage> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))?;
    let context = HeifContext::read_from_file(path_str)
        .with_context(|| format!("failed to open HEIF container {}", path.display()))?;
    let handle = context
        .primary_image_handle()
        .with_context(|| format!("failed to read primary image handle {}", path.display()))?;
    let bit_depth = handle
        .luma_bits_per_pixel()
        .max(handle.chroma_bits_per_pixel());
    let has_alpha = handle.has_alpha_channel();
    let hdr = bit_depth > 8;
    let little_endian = cfg!(target_endian = "little");
    let color_space = match (hdr, has_alpha, little_endian) {
        (false, false, _) => ColorSpace::Rgb(RgbChroma::Rgb),
        (false, true, _) => ColorSpace::Rgb(RgbChroma::Rgba),
        (true, false, true) => ColorSpace::Rgb(RgbChroma::HdrRgbLe),
        (true, false, false) => ColorSpace::Rgb(RgbChroma::HdrRgbBe),
        (true, true, true) => ColorSpace::Rgb(RgbChroma::HdrRgbaLe),
        (true, true, false) => ColorSpace::Rgb(RgbChroma::HdrRgbaBe),
    };
    let image = LibHeif::new()
        .decode(&handle, color_space, None)
        .with_context(|| format!("failed to decode HEIF image {}", path.display()))?;
    let planes = image.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| anyhow!("HEIF image is not interleaved: {}", path.display()))?;

    if hdr {
        let channels = if has_alpha { 4usize } else { 3usize };
        let row_size = plane.width as usize * channels * 2;
        let mut pixels =
            Vec::with_capacity(plane.width as usize * plane.height as usize * channels);

        for row in plane
            .data
            .chunks_exact(plane.stride)
            .take(plane.height as usize)
        {
            for sample in row[..row_size].chunks_exact(2) {
                let value = if little_endian {
                    u16::from_le_bytes([sample[0], sample[1]])
                } else {
                    u16::from_be_bytes([sample[0], sample[1]])
                };
                pixels.push(value);
            }
        }

        let image = if has_alpha {
            let rgba =
                ImageBuffer::<Rgba<u16>, Vec<u16>>::from_raw(plane.width, plane.height, pixels)
                    .ok_or_else(|| {
                        anyhow!("failed to construct HDR RGBA image {}", path.display())
                    })?;
            DynamicImage::ImageRgba16(rgba)
        } else {
            let rgb =
                ImageBuffer::<Rgb<u16>, Vec<u16>>::from_raw(plane.width, plane.height, pixels)
                    .ok_or_else(|| {
                        anyhow!("failed to construct HDR RGB image {}", path.display())
                    })?;
            DynamicImage::ImageRgb16(rgb)
        };

        return Ok(LoadedImage { image, bit_depth });
    }

    let channels = if has_alpha { 4usize } else { 3usize };
    let row_size = plane.width as usize * channels;
    let mut pixels = Vec::with_capacity(row_size * plane.height as usize);

    for row in plane
        .data
        .chunks_exact(plane.stride)
        .take(plane.height as usize)
    {
        pixels.extend_from_slice(&row[..row_size]);
    }

    let image = if has_alpha {
        let rgba = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(plane.width, plane.height, pixels)
            .ok_or_else(|| anyhow!("failed to construct RGBA image buffer {}", path.display()))?;
        DynamicImage::ImageRgba8(rgba)
    } else {
        let rgb = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(plane.width, plane.height, pixels)
            .ok_or_else(|| anyhow!("failed to construct RGB image buffer {}", path.display()))?;
        DynamicImage::ImageRgb8(rgb)
    };

    Ok(LoadedImage {
        image,
        bit_depth: 8,
    })
}

fn save_original_avif(
    loaded: &LoadedImage,
    path: &Path,
    avif_quality: u8,
    avif_threads: usize,
) -> Result<()> {
    let encoder = RavifEncoder::new()
        .with_quality(f32::from(avif_quality))
        .with_alpha_quality(f32::from(avif_quality))
        .with_speed(AVIF_SPEED)
        .with_bit_depth(AvifBitDepth::Ten)
        .with_internal_color_model(AvifColorModel::RGB)
        .with_num_threads(Some(avif_threads));

    let avif_file = if loaded.bit_depth > 8 {
        let rgba = loaded.image.to_rgba16();
        let width = rgba.width() as usize;
        let height = rgba.height() as usize;
        let bit_depth = loaded.bit_depth.min(16).max(10);
        let has_alpha = rgba.pixels().any(|pixel| pixel.0[3] != u16::MAX);
        let planes = rgba.pixels().map(|pixel| {
            [
                scale_to_ten(pixel.0[1], bit_depth),
                scale_to_ten(pixel.0[2], bit_depth),
                scale_to_ten(pixel.0[0], bit_depth),
            ]
        });

        encoder
            .encode_raw_planes_10_bit(
                width,
                height,
                planes,
                has_alpha.then(|| {
                    rgba.pixels()
                        .map(|pixel| scale_to_ten(pixel.0[3], bit_depth))
                }),
                ravif::PixelRange::Full,
                ravif::MatrixCoefficients::Identity,
            )
            .map_err(|error| anyhow!("failed to encode 10-bit AVIF: {error}"))?
            .avif_file
    } else {
        let rgba = loaded.image.to_rgba8();
        let pixels = rgba.as_raw().as_rgba();
        encoder
            .encode_rgba(Img::new(
                pixels,
                rgba.width() as usize,
                rgba.height() as usize,
            ))
            .map_err(|error| anyhow!("failed to encode AVIF: {error}"))?
            .avif_file
    };

    fs::write(path, avif_file).with_context(|| format!("failed to create {}", path.display()))?;

    Ok(())
}

fn save_thumbnail(
    image: &DynamicImage,
    path: &Path,
    format: ThumbnailFormat,
    quality: u8,
) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);

    match format {
        ThumbnailFormat::Jpeg => {
            let rgb = image.to_rgb8();
            let mut encoder = JpegEncoder::new_with_quality(&mut writer, quality);
            encoder.encode(&rgb, rgb.width(), rgb.height(), ColorType::Rgb8.into())?;
        }
        ThumbnailFormat::Png => {
            image.write_to(&mut writer, ImageFormat::Png)?;
        }
        ThumbnailFormat::Webp => {
            let rgba = image.to_rgba8();
            let encoder = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height());
            let encoded = encoder.encode((quality as f32).clamp(1.0, 100.0));
            writer
                .write_all(encoded.as_ref())
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }

    Ok(())
}

fn compute_blurhash(image: &DynamicImage) -> Result<String> {
    let reduced = image.thumbnail(32, 32).to_rgba8();
    let width = reduced.width();
    let height = reduced.height();
    let pixels = reduced.into_raw();

    encode(4, 3, width, height, &pixels)
        .map_err(|error| anyhow!("failed to encode blurhash: {error}"))
}

fn build_original_path(originals_dir: &Path, relative_source: &Path) -> PathBuf {
    let mut path = originals_dir.join(relative_source);
    path.set_extension(AVIF_EXTENSION);
    path
}

fn build_thumbnail_path(thumbnails_dir: &Path, config: &Config, relative_source: &Path) -> PathBuf {
    let mut path = thumbnails_dir.join(relative_source);
    path.set_extension(config.thumbnail_format.extension());
    path
}

fn load_previous_manifest(path: &Path) -> Result<LoadedManifest> {
    if !path.exists() {
        return Ok(LoadedManifest {
            photos_by_key: BTreeMap::new(),
        });
    }

    let file = File::open(path)
        .with_context(|| format!("failed to open previous manifest {}", path.display()))?;
    let manifest: ManifestFile = serde_json::from_reader(file)
        .with_context(|| format!("failed to parse previous manifest {}", path.display()))?;
    let photos_by_key = manifest
        .photos
        .into_iter()
        .map(|photo| (photo.original.url.clone(), photo))
        .collect();

    Ok(LoadedManifest { photos_by_key })
}

fn recommended_parallelism() -> usize {
    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let physical = num_cpus::get_physical();
    let baseline = match physical {
        0 => available,
        n => available.min(n),
    };

    (baseline / 2).clamp(1, 8)
}

fn recommended_avif_threads(worker_count: usize, total_jobs: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let concurrent_jobs = total_jobs.max(1).min(worker_count.max(1));

    (available / concurrent_jobs).clamp(1, 4)
}

fn progress_style() -> Result<ProgressStyle> {
    ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} {msg}",
    )
    .map(|style| style.progress_chars("=>-"))
    .map_err(|error| anyhow!("failed to configure progress bar: {error}"))
}

fn load_previous_state(path: &Path) -> Result<StateFile> {
    if !path.exists() {
        return Ok(StateFile {
            version: 1,
            updated_at: String::new(),
            files: BTreeMap::new(),
        });
    }

    let file = File::open(path)
        .with_context(|| format!("failed to open previous state {}", path.display()))?;
    let state: StateFile = serde_json::from_reader(file)
        .with_context(|| format!("failed to parse previous state {}", path.display()))?;
    Ok(state)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    {
        let file = File::create(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, value)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

fn metadata_mtime_ms(metadata: &fs::Metadata) -> Result<u128> {
    let modified = metadata.modified()?;
    let duration = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| anyhow!("invalid file mtime: {error}"))?;
    Ok(duration.as_millis())
}

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn inferred_bit_depth(image: &DynamicImage) -> u8 {
    match image.color() {
        ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16 => 16,
        _ => 8,
    }
}

fn scale_to_ten(value: u16, source_bit_depth: u8) -> u16 {
    let source_bit_depth = source_bit_depth.clamp(1, 16);
    let source_max = ((1u32 << source_bit_depth) - 1).max(1);
    ((u32::from(value).min(source_max) * 1023 + (source_max / 2)) / source_max) as u16
}

fn normalize_relative_path(base_dir: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(base_dir)
        .with_context(|| format!("failed to strip prefix from {}", path.display()))?;

    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            let ext = extension.to_ascii_lowercase();
            SUPPORTED_EXTENSIONS.contains(&ext.as_str())
        })
        .unwrap_or(false)
}

fn is_heif_family(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "hif" | "heif" | "heic"
            )
        })
        .unwrap_or(false)
}

fn mime_from_format(format: ThumbnailFormat) -> &'static str {
    match format {
        ThumbnailFormat::Jpeg => "image/jpeg",
        ThumbnailFormat::Png => "image/png",
        ThumbnailFormat::Webp => "image/webp",
    }
}

struct SourceItem {
    path: PathBuf,
    relative_path: PathBuf,
    source_key: String,
    size: u64,
    mtime_ms: u128,
}

struct LoadedImage {
    image: DynamicImage,
    bit_depth: u8,
}

struct ProcessedPhoto {
    state_key: String,
    state_entry: StateEntry,
    photo_entry: PhotoEntry,
}

struct LoadedManifest {
    photos_by_key: BTreeMap<String, PhotoEntry>,
}

#[derive(Deserialize, Serialize)]
struct ManifestFile {
    version: u8,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    photos: Vec<PhotoEntry>,
}

#[derive(Clone, Deserialize, Serialize)]
struct PhotoEntry {
    original: Asset,
    thumbnail: Asset,
    #[serde(skip_serializing_if = "Option::is_none")]
    blurhash: Option<String>,
    title: String,
    #[serde(rename = "takenAt", skip_serializing_if = "Option::is_none")]
    taken_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    camera: Option<Camera>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<ImageMetadata>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Asset {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    bytes: u64,
    mime: String,
}

#[derive(Deserialize, Serialize)]
struct StateFile {
    version: u8,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    files: BTreeMap<String, StateEntry>,
}

#[derive(Clone, Deserialize, Serialize)]
struct StateEntry {
    size: u64,
    #[serde(rename = "mtimeMs")]
    mtime_ms: u128,
    #[serde(default)]
    original: String,
    thumbnail: String,
    #[serde(rename = "processedAt")]
    processed_at: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Location {
    lat: f64,
    lng: f64,
    alt: f64,
    country: String,
    city: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Camera {
    make: String,
    model: String,
    lens: String,
    #[serde(rename = "focalLengthMm")]
    focal_length_mm: u32,
    #[serde(rename = "focalLengthIn35mm")]
    focal_length_in_35mm: u32,
    aperture: f32,
    shutter: String,
    iso: u32,
}

#[derive(Clone, Deserialize, Serialize)]
struct ImageMetadata {
    orientation: u8,
    #[serde(rename = "colorSpace")]
    color_space: String,
    #[serde(rename = "hasHdr")]
    has_hdr: bool,
    #[serde(rename = "isLivePhoto")]
    is_live_photo: bool,
}
