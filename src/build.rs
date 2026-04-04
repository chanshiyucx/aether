use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    fs::File,
    io::{BufWriter, Write},
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
use rayon::{ThreadPoolBuilder, prelude::*};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::warn;
use walkdir::WalkDir;

use crate::config::{Config, ThumbnailFormat};

const SUPPORTED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "heif", "heic", "hif"];
pub enum BuildExit {
    Success,
    PartialFailure,
}

pub fn run() -> Result<BuildExit> {
    let config = Config::load()?;
    let originals_dir = config.originals_path();
    let thumbnails_dir = config.thumbnails_path();
    let root_dir = config.root_dir();
    let started_at = Instant::now();

    if !originals_dir.exists() {
        bail!(
            "originals directory does not exist: {}",
            originals_dir.display()
        );
    }

    fs::create_dir_all(&thumbnails_dir).with_context(|| {
        format!(
            "failed to create thumbnails directory {}",
            thumbnails_dir.display()
        )
    })?;

    let previous_manifest = load_previous_manifest(&config.manifest_path())?;
    let previous_state = load_previous_state(&config.state_path())?;
    let mut originals = collect_originals(&originals_dir, &root_dir)?;
    originals.sort_by(|left, right| left.original_key.cmp(&right.original_key));

    let total = originals.len();
    let parallelism = recommended_parallelism();

    println!("starting build");
    println!("root: {}", root_dir.display());
    println!("originals: {}", originals_dir.display());
    println!("thumbnails: {}", thumbnails_dir.display());
    println!("found {} supported images", total);
    println!("workers: {}", parallelism);

    let progress = ProgressBar::new(total as u64);
    progress.set_style(progress_style()?);
    progress.set_message("building");

    let current_keys: BTreeSet<_> = originals
        .iter()
        .map(|item| item.original_key.clone())
        .collect();
    cleanup_removed_outputs(&root_dir, &current_keys, &previous_state)?;

    let mut photos = Vec::new();
    let mut files = BTreeMap::new();
    let mut pending = Vec::new();
    let mut reused_count = 0usize;

    for item in originals {
        if let Some((photo, state_entry)) =
            try_reuse(&root_dir, &item, &previous_manifest, &previous_state)
        {
            progress.inc(1);
            photos.push(photo);
            files.insert(item.original_key, state_entry);
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
                    &item.path,
                    item.size,
                    item.mtime_ms,
                );
                progress.inc(1);

                if let Err(error) = &result {
                    warn!("skipped {}: {error:#}", item.path.display());
                    progress.println(format!("failed {}: {error:#}", item.original_key));
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

fn collect_originals(originals_dir: &Path, root_dir: &Path) -> Result<Vec<OriginalItem>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(originals_dir) {
        let entry = entry?;
        let path = entry.path();

        if entry.file_type().is_dir() {
            continue;
        }

        if is_supported(path) {
            let metadata = fs::metadata(path)
                .with_context(|| format!("failed to read metadata for {}", path.display()))?;
            files.push(OriginalItem {
                path: path.to_path_buf(),
                original_key: normalize_relative_path(root_dir, path)?,
                size: metadata.len(),
                mtime_ms: metadata_mtime_ms(&metadata)?,
            });
        }
    }

    Ok(files)
}

fn try_reuse(
    root_dir: &Path,
    item: &OriginalItem,
    previous_manifest: &LoadedManifest,
    previous_state: &StateFile,
) -> Option<(PhotoEntry, StateEntry)> {
    let state_entry = previous_state.files.get(&item.original_key)?;
    let photo_entry = previous_manifest.photos_by_key.get(&item.original_key)?;

    if state_entry.size != item.size || state_entry.mtime_ms != item.mtime_ms {
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
    for (original_key, state_entry) in &previous_state.files {
        if current_keys.contains(original_key) {
            continue;
        }

        let thumbnail_path = root_dir.join(&state_entry.thumbnail);
        if thumbnail_path.exists() {
            fs::remove_file(&thumbnail_path).with_context(|| {
                format!(
                    "failed to remove stale thumbnail {}",
                    thumbnail_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn process_one(
    config: &Config,
    root_dir: &Path,
    originals_dir: &Path,
    thumbnails_dir: &Path,
    original_path: &Path,
    original_size: u64,
    original_mtime_ms: u128,
) -> Result<ProcessedPhoto> {
    let relative = original_path.strip_prefix(originals_dir).with_context(|| {
        format!(
            "failed to strip originals prefix from {}",
            original_path.display()
        )
    })?;

    let image = load_image(original_path)
        .with_context(|| format!("failed to decode {}", original_path.display()))?;

    let thumbnail_path = build_thumbnail_path(thumbnails_dir, config, relative);
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

    let thumbnail_metadata = fs::metadata(&thumbnail_path)
        .with_context(|| format!("failed to read metadata for {}", thumbnail_path.display()))?;
    let (original_width, original_height) = image.dimensions();
    let (thumbnail_width, thumbnail_height) = thumbnail_image.dimensions();

    let blurhash = if config.enable_blurhash {
        Some(compute_blurhash(&thumbnail_image)?)
    } else {
        None
    };

    let original_key = normalize_relative_path(root_dir, original_path)?;
    let thumbnail_key = normalize_relative_path(root_dir, &thumbnail_path)?;
    let title = original_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("missing file stem for {}", original_path.display()))?;

    Ok(ProcessedPhoto {
        state_key: original_key.clone(),
        state_entry: StateEntry {
            size: original_size,
            mtime_ms: original_mtime_ms,
            thumbnail: thumbnail_key.clone(),
            processed_at: now_rfc3339()?,
        },
        photo_entry: PhotoEntry {
            original: Asset {
                url: original_key,
                width: Some(original_width),
                height: Some(original_height),
                bytes: original_size,
                mime: mime_from_extension(original_path),
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

fn load_image(path: &Path) -> Result<DynamicImage> {
    if is_heif_family(path) {
        return decode_heif_image(path);
    }

    ImageReader::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("failed to guess image format for {}", path.display()))?
        .decode()
        .with_context(|| format!("failed to decode {}", path.display()))
}

fn decode_heif_image(path: &Path) -> Result<DynamicImage> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))?;
    let context = HeifContext::read_from_file(path_str)
        .with_context(|| format!("failed to open HEIF container {}", path.display()))?;
    let handle = context
        .primary_image_handle()
        .with_context(|| format!("failed to read primary image handle {}", path.display()))?;
    let image = LibHeif::new()
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .with_context(|| format!("failed to decode HEIF image {}", path.display()))?;
    let planes = image.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| anyhow!("HEIF image is not interleaved: {}", path.display()))?;

    let row_size = plane.width as usize * 3;
    let mut pixels = Vec::with_capacity(row_size * plane.height as usize);

    for row in plane
        .data
        .chunks_exact(plane.stride)
        .take(plane.height as usize)
    {
        pixels.extend_from_slice(&row[..row_size]);
    }

    let rgb = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(plane.width, plane.height, pixels)
        .ok_or_else(|| anyhow!("failed to construct RGB image buffer {}", path.display()))?;

    Ok(DynamicImage::ImageRgb8(rgb))
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

fn build_thumbnail_path(
    thumbnails_dir: &Path,
    config: &Config,
    relative_original: &Path,
) -> PathBuf {
    let mut path = thumbnails_dir.join(relative_original);
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

    ((baseline * 3) / 4).max(1)
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

fn normalize_relative_path(root_dir: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root_dir)
        .with_context(|| format!("failed to strip root prefix from {}", path.display()))?;

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

fn mime_from_extension(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .map(|extension| match extension.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "webp" => "image/webp",
            "heif" | "heic" | "hif" => "image/heif",
            _ => "application/octet-stream",
        })
        .unwrap_or("application/octet-stream")
        .to_string()
}

fn mime_from_format(format: ThumbnailFormat) -> &'static str {
    match format {
        ThumbnailFormat::Jpeg => "image/jpeg",
        ThumbnailFormat::Png => "image/png",
        ThumbnailFormat::Webp => "image/webp",
    }
}

struct OriginalItem {
    path: PathBuf,
    original_key: String,
    size: u64,
    mtime_ms: u128,
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
