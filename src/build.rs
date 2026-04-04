use std::{
    collections::BTreeMap,
    fs,
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use blurhash::encode;
use image::{
    ColorType, DynamicImage, GenericImageView, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::{info, warn};
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

    let mut photos = Vec::new();
    let mut files = BTreeMap::new();
    let mut failures = Vec::new();
    let originals = collect_originals(&originals_dir)?;
    let total = originals.len();

    println!("starting build");
    println!("root: {}", root_dir.display());
    println!("originals: {}", originals_dir.display());
    println!("thumbnails: {}", thumbnails_dir.display());
    println!("found {} supported images", total);

    for (index, path) in originals.iter().enumerate() {
        let current = index + 1;
        let display_path =
            normalize_relative_path(&root_dir, path).unwrap_or_else(|_| path.display().to_string());
        println!("[{current}/{total}] processing {display_path}");

        match process_one(&config, &root_dir, &originals_dir, &thumbnails_dir, &path) {
            Ok(processed) => {
                files.insert(processed.state_key, processed.state_entry);
                photos.push(processed.photo_entry);
                println!("[{current}/{total}] done {display_path}");
            }
            Err(error) => {
                warn!("skipped {}: {error:#}", path.display());
                println!("[{current}/{total}] failed {display_path}: {error:#}");
                failures.push(path.clone());
            }
        }
    }

    photos.sort_by(|left, right| left.original.url.cmp(&right.original.url));

    let processed_count = photos.len();
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

    info!(
        processed = processed_count,
        failed = failed_count,
        "build completed"
    );

    println!(
        "build completed: {} processed, {} failed, elapsed {:.2?}",
        processed_count,
        failed_count,
        started_at.elapsed()
    );

    if failures.is_empty() {
        Ok(BuildExit::Success)
    } else {
        Ok(BuildExit::PartialFailure)
    }
}

fn collect_originals(originals_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(originals_dir) {
        let entry = entry?;
        let path = entry.path();

        if entry.file_type().is_dir() {
            continue;
        }

        if is_supported(path) {
            files.push(path.to_path_buf());
        }
    }

    Ok(files)
}

fn process_one(
    config: &Config,
    root_dir: &Path,
    originals_dir: &Path,
    thumbnails_dir: &Path,
    original_path: &Path,
) -> Result<ProcessedPhoto> {
    let relative = original_path.strip_prefix(originals_dir).with_context(|| {
        format!(
            "failed to strip originals prefix from {}",
            original_path.display()
        )
    })?;

    let image = ImageReader::open(original_path)
        .with_context(|| format!("failed to open {}", original_path.display()))?
        .with_guessed_format()
        .with_context(|| {
            format!(
                "failed to guess image format for {}",
                original_path.display()
            )
        })?
        .decode()
        .with_context(|| format!("failed to decode {}", original_path.display()))?;

    let original_metadata = fs::metadata(original_path)
        .with_context(|| format!("failed to read metadata for {}", original_path.display()))?;

    let thumbnail_path = build_thumbnail_path(thumbnails_dir, config, relative);
    if let Some(parent) = thumbnail_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let thumbnail_image = resize_image(&image, config.thumbnail_width);
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
        Some(compute_blurhash(&image)?)
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
            size: original_metadata.len(),
            mtime_ms: metadata_mtime_ms(&original_metadata)?,
            thumbnail: thumbnail_key.clone(),
            processed_at: now_rfc3339()?,
        },
        photo_entry: PhotoEntry {
            original: Asset {
                url: original_key,
                width: Some(original_width),
                height: Some(original_height),
                bytes: original_metadata.len(),
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

fn resize_image(image: &DynamicImage, target_width: u32) -> DynamicImage {
    let width = image.width();
    if width <= target_width {
        return image.clone();
    }

    let ratio = target_width as f64 / width as f64;
    let target_height = (image.height() as f64 * ratio).round() as u32;

    image.resize_exact(target_width, target_height.max(1), FilterType::Lanczos3)
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
            image.write_to(&mut writer, ImageFormat::WebP)?;
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

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, value)
        .with_context(|| format!("failed to write {}", path.display()))?;
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

struct ProcessedPhoto {
    state_key: String,
    state_entry: StateEntry,
    photo_entry: PhotoEntry,
}

#[derive(Serialize)]
struct ManifestFile {
    version: u8,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    photos: Vec<PhotoEntry>,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
struct Asset {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    bytes: u64,
    mime: String,
}

#[derive(Serialize)]
struct StateFile {
    version: u8,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    files: BTreeMap<String, StateEntry>,
}

#[derive(Serialize)]
struct StateEntry {
    size: u64,
    #[serde(rename = "mtimeMs")]
    mtime_ms: u128,
    thumbnail: String,
    #[serde(rename = "processedAt")]
    processed_at: String,
}

#[derive(Serialize)]
struct Location {
    lat: f64,
    lng: f64,
    alt: f64,
    country: String,
    city: String,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
struct ImageMetadata {
    orientation: u8,
    #[serde(rename = "colorSpace")]
    color_space: String,
    #[serde(rename = "hasHdr")]
    has_hdr: bool,
    #[serde(rename = "isLivePhoto")]
    is_live_photo: bool,
}
