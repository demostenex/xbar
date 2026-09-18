use crate::core::{
    HistoryEntryId, NotificationHistoryEntry, NotificationIconMetadata, NotificationImageData,
};
use image::imageops::FilterType;
use image::{ImageReader, RgbaImage};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_ICON_DIMENSION: u32 = 4096;
const MAX_ICON_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 32;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedNotificationIcon {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, row-major RGBA8 pixels.
    pub pixels: Arc<[u8]>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    source: String,
    target: Option<(u32, u32)>,
}

struct CacheEntry {
    key: CacheKey,
    icon: Arc<ResolvedNotificationIcon>,
    bytes: usize,
}

#[derive(Default)]
struct IconCache {
    entries: VecDeque<CacheEntry>,
    bytes: usize,
}

impl IconCache {
    fn get(&mut self, key: &CacheKey) -> Option<Arc<ResolvedNotificationIcon>> {
        let index = self.entries.iter().position(|entry| entry.key == *key)?;
        let entry = self.entries.remove(index)?;
        let icon = Arc::clone(&entry.icon);
        self.entries.push_back(entry);
        Some(icon)
    }

    fn insert(&mut self, key: CacheKey, icon: Arc<ResolvedNotificationIcon>) {
        let bytes = icon.pixels.len();
        if bytes > MAX_CACHE_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            if let Some(old) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.push_back(CacheEntry { key, icon, bytes });
        while self.entries.len() > MAX_CACHE_ENTRIES || self.bytes > MAX_CACHE_BYTES {
            if let Some(old) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            } else {
                break;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

pub struct NotificationIconResolver {
    cache: IconCache,
    history_results: HashMap<HistoryEntryId, (u64, Option<Arc<ResolvedNotificationIcon>>)>,
    data_dirs: Vec<PathBuf>,
    icon_themes: Vec<String>,
}

impl Default for NotificationIconResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationIconResolver {
    pub fn new() -> Self {
        Self {
            cache: IconCache::default(),
            history_results: HashMap::new(),
            data_dirs: xdg_data_dirs(),
            icon_themes: configured_icon_themes(),
        }
    }

    #[cfg(test)]
    fn with_search_roots(data_dirs: Vec<PathBuf>, icon_themes: Vec<String>) -> Self {
        Self {
            cache: IconCache::default(),
            history_results: HashMap::new(),
            data_dirs,
            icon_themes,
        }
    }

    pub fn resolve(
        &mut self,
        metadata: &NotificationIconMetadata,
        target_size: Option<(u32, u32)>,
    ) -> Option<Arc<ResolvedNotificationIcon>> {
        if let Some(image_data) = &metadata.image_data {
            if let Some(icon) = resolve_image_data(image_data, target_size) {
                return Some(Arc::new(icon));
            }
        }
        if let Some(image_path) = &metadata.image_path {
            if let Some(icon) = self.resolve_path_or_uri(image_path, target_size) {
                return Some(icon);
            }
        }
        if let Some(app_icon) = &metadata.app_icon {
            if let Some(icon) = self.resolve_icon_candidate(app_icon, target_size) {
                return Some(icon);
            }
        }
        metadata
            .desktop_entry
            .as_deref()
            .and_then(|entry| resolve_desktop_entry_icon_in_dirs(entry, &self.data_dirs))
            .and_then(|icon| self.resolve_icon_candidate(&icon, target_size))
    }

    /// Resolve changed history entries outside the renderer. The timestamp is
    /// the existing replacement identity, so expiration can reuse the same
    /// immutable result without comparing or copying raw image data.
    pub fn resolve_history(&mut self, history: &[NotificationHistoryEntry]) {
        let live_ids = history.iter().map(|entry| entry.id).collect::<Vec<_>>();
        self.history_results.retain(|id, _| live_ids.contains(id));
        for entry in history {
            if self
                .history_results
                .get(&entry.id)
                .is_some_and(|(updated_at, _)| *updated_at == entry.updated_at)
            {
                continue;
            }
            let icon = self.resolve(&entry.icon_metadata, Some((32, 32)));
            self.history_results
                .insert(entry.id, (entry.updated_at, icon));
        }
    }

    pub fn resolved_history_icons(&self) -> HashMap<HistoryEntryId, Arc<ResolvedNotificationIcon>> {
        self.history_results
            .iter()
            .filter_map(|(id, (_, icon))| icon.as_ref().map(|icon| (*id, Arc::clone(icon))))
            .collect()
    }

    #[allow(dead_code)]
    pub fn history_icon(
        &self,
        history_id: HistoryEntryId,
    ) -> Option<Arc<ResolvedNotificationIcon>> {
        self.history_results
            .get(&history_id)
            .and_then(|(_, icon)| icon.clone())
    }

    fn resolve_path_or_uri(
        &mut self,
        value: &str,
        target_size: Option<(u32, u32)>,
    ) -> Option<Arc<ResolvedNotificationIcon>> {
        if let Some(path) = local_path(value) {
            return self.resolve_file(&path, target_size);
        }
        self.resolve_icon_candidate(value, target_size)
    }

    fn resolve_icon_candidate(
        &mut self,
        value: &str,
        target_size: Option<(u32, u32)>,
    ) -> Option<Arc<ResolvedNotificationIcon>> {
        if let Some(path) = local_path(value) {
            return self.resolve_file(&path, target_size);
        }
        let candidates =
            find_icon_paths_in_dirs(value, &self.data_dirs, &self.icon_themes, target_size);
        candidates
            .iter()
            .find_map(|path| self.resolve_file(path, target_size))
    }

    fn resolve_file(
        &mut self,
        path: &Path,
        target_size: Option<(u32, u32)>,
    ) -> Option<Arc<ResolvedNotificationIcon>> {
        let key = CacheKey {
            source: format!("file:{}", path.display()),
            target: target_size,
        };
        if let Some(icon) = self.cache.get(&key) {
            return Some(icon);
        }
        let icon = decode_raster(path, target_size)?;
        let icon = Arc::new(icon);
        self.cache.insert(key, Arc::clone(&icon));
        Some(icon)
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.len()
    }
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_ICON_DIMENSION
        && height <= MAX_ICON_DIMENSION
        && u64::from(width).saturating_mul(u64::from(height)) <= MAX_ICON_PIXELS
}

fn normalized_image(
    image: RgbaImage,
    target_size: Option<(u32, u32)>,
) -> Option<ResolvedNotificationIcon> {
    let image = if let Some((width, height)) = target_size {
        if !valid_dimensions(width, height) {
            return None;
        }
        let (source_width, source_height) = image.dimensions();
        let scale = (u64::from(width) * u64::from(source_height))
            .min(u64::from(height) * u64::from(source_width));
        let fitted_width = (scale / u64::from(source_height)).max(1) as u32;
        let fitted_height = (scale / u64::from(source_width)).max(1) as u32;
        image::imageops::resize(&image, fitted_width, fitted_height, FilterType::Triangle)
    } else {
        image
    };
    let (width, height) = image.dimensions();
    valid_dimensions(width, height).then(|| ResolvedNotificationIcon {
        width,
        height,
        pixels: Arc::from(image.into_raw().into_boxed_slice()),
    })
}

fn resolve_image_data(
    image: &NotificationImageData,
    target_size: Option<(u32, u32)>,
) -> Option<ResolvedNotificationIcon> {
    let width = u32::try_from(image.width).ok()?;
    let height = u32::try_from(image.height).ok()?;
    if !valid_dimensions(width, height) {
        return None;
    }
    let channels = usize::try_from(image.channels).ok()?;
    let rowstride = usize::try_from(image.rowstride).ok()?;
    let width_usize = usize::try_from(width).ok()?;
    let height_usize = usize::try_from(height).ok()?;
    let mut pixels = vec![0_u8; width_usize.checked_mul(height_usize)?.checked_mul(4)?];
    for y in 0..height_usize {
        let source_row = y.checked_mul(rowstride)?;
        for x in 0..width_usize {
            let source = source_row.checked_add(x.checked_mul(channels)?)?;
            let destination = y.checked_mul(width_usize)?.checked_add(x)?.checked_mul(4)?;
            let source_end = source.checked_add(channels)?;
            let source_pixel = image.data.get(source..source_end)?;
            pixels[destination..destination + 3].copy_from_slice(&source_pixel[..3]);
            pixels[destination + 3] = if channels == 4 { source_pixel[3] } else { 255 };
        }
    }
    normalized_image(RgbaImage::from_raw(width, height, pixels)?, target_size)
}

fn decode_raster(path: &Path, target_size: Option<(u32, u32)>) -> Option<ResolvedNotificationIcon> {
    let (width, height) = image::image_dimensions(path).ok()?;
    if !valid_dimensions(width, height) {
        return None;
    }
    let image = ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    normalized_image(image.to_rgba8(), target_size)
}

fn local_path(value: &str) -> Option<PathBuf> {
    if let Some(uri_path) = value.strip_prefix("file://") {
        let uri_path = uri_path.strip_prefix("localhost").unwrap_or(uri_path);
        return (uri_path.starts_with('/')).then(|| percent_decode(uri_path));
    }
    Path::new(value).is_absolute().then(|| PathBuf::from(value))
}

fn percent_decode(value: &str) -> PathBuf {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(home));
    } else if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    let system = std::env::var_os("XDG_DATA_DIRS")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

fn configured_icon_themes() -> Vec<String> {
    let mut themes = Vec::new();
    for path in [
        ".config/gtk-4.0/settings.ini",
        ".config/gtk-3.0/settings.ini",
    ] {
        if let Some(home) = std::env::var_os("HOME") {
            if let Ok(contents) = fs::read_to_string(PathBuf::from(home).join(path)) {
                if let Some(theme) = contents.lines().find_map(|line| {
                    line.strip_prefix("gtk-icon-theme-name=")
                        .map(str::trim)
                        .filter(|theme| !theme.is_empty())
                }) {
                    themes.push(theme.to_owned());
                }
            }
        }
    }
    themes.extend(["hicolor".into(), "Adwaita".into()]);
    themes.sort();
    themes.dedup();
    themes
}

fn find_icon_paths_in_dirs(
    name: &str,
    data_dirs: &[PathBuf],
    icon_themes: &[String],
    target_size: Option<(u32, u32)>,
) -> Vec<PathBuf> {
    let file_names = supported_file_names(name);
    let mut paths = Vec::new();
    for data_dir in data_dirs {
        for theme in icon_themes {
            let mut theme_order = Vec::new();
            append_theme_inheritance(data_dir, theme, &mut HashSet::new(), &mut theme_order);
            for inherited_theme in theme_order {
                let theme_dir = data_dir.join("icons").join(inherited_theme);
                let mut candidates = find_icons_in_theme(&theme_dir, &file_names, target_size);
                candidates.sort_by_key(|candidate| candidate.rank);
                paths.extend(candidates.into_iter().map(|candidate| candidate.path));
            }
        }
        paths.extend(find_icons_in_dir(&data_dir.join("pixmaps"), &file_names, 1));
    }
    paths
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum IconDirectoryType {
    Fixed,
    Scalable,
    Threshold,
}

#[derive(Clone, Debug)]
struct IconDirectory {
    path: PathBuf,
    size: u32,
    min_size: u32,
    max_size: u32,
    threshold: u32,
    scale: u32,
    directory_type: IconDirectoryType,
    order: usize,
}

struct IconCandidate {
    path: PathBuf,
    rank: (u8, u32, usize),
}

fn find_icons_in_theme(
    theme_dir: &Path,
    names: &[String],
    target_size: Option<(u32, u32)>,
) -> Vec<IconCandidate> {
    let directories = read_icon_directories(theme_dir);
    let mut candidates = if directories.is_empty() {
        find_icons_in_dir(theme_dir, names, 3)
            .into_iter()
            .enumerate()
            .map(|(order, path)| IconCandidate {
                path,
                rank: (2, u32::MAX, order),
            })
            .collect::<Vec<_>>()
    } else {
        directories
            .iter()
            .flat_map(|directory| {
                let dir = theme_dir.join(&directory.path);
                find_icons_in_dir(&dir, names, 1)
                    .into_iter()
                    .map(|path| (directory, path))
                    .collect::<Vec<_>>()
            })
            .map(|(directory, path)| {
                let target = target_size.map(|(width, height)| width.max(height));
                let rank = target.map_or((0, 0, directory.order), |target| {
                    let size = directory.effective_size(target);
                    size_rank(size, target, directory.order)
                });
                IconCandidate { path, rank }
            })
            .collect::<Vec<_>>()
    };
    candidates.sort_by_key(|candidate| candidate.rank);
    candidates
}

fn size_rank(size: u32, target: u32, order: usize) -> (u8, u32, usize) {
    if size == target {
        (0, 0, order)
    } else if size > target {
        (1, size - target, order)
    } else {
        (2, target - size, order)
    }
}

impl IconDirectory {
    fn effective_size(&self, target: u32) -> u32 {
        let scale = self.scale.max(1);
        match self.directory_type {
            IconDirectoryType::Fixed => self.size.saturating_mul(scale),
            IconDirectoryType::Scalable => target.saturating_mul(scale).clamp(
                self.min_size.saturating_mul(scale),
                self.max_size.saturating_mul(scale),
            ),
            IconDirectoryType::Threshold => {
                let size = self.size.saturating_mul(scale);
                let threshold = self.threshold.saturating_mul(scale);
                target.saturating_mul(scale).clamp(
                    size.saturating_sub(threshold),
                    size.saturating_add(threshold),
                )
            }
        }
    }
}

fn read_icon_directories(theme_dir: &Path) -> Vec<IconDirectory> {
    let index = fs::read_to_string(theme_dir.join("index.theme")).ok();
    let Some(index) = index else {
        return Vec::new();
    };
    let mut declared = Vec::new();
    let mut directories: Vec<IconDirectory> = Vec::new();
    let mut section = None;
    for line in index.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("Directories=") {
            declared = value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .collect();
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = (name != "Icon Theme").then_some(name.to_owned());
            continue;
        }
        let Some(section_name) = section.as_deref() else {
            continue;
        };
        let Some(directory) = declared
            .iter()
            .position(|path| path == Path::new(section_name))
        else {
            continue;
        };
        if !directories.iter().any(|entry| entry.order == directory) {
            directories.push(IconDirectory {
                path: declared[directory].clone(),
                size: 1,
                min_size: 1,
                max_size: MAX_ICON_DIMENSION,
                threshold: 0,
                scale: 1,
                directory_type: IconDirectoryType::Fixed,
                order: directory,
            });
        }
        let entry = directories
            .iter_mut()
            .find(|entry| entry.order == directory)
            .unwrap();
        let (key, value) = line.split_once('=').unwrap_or(("", ""));
        match key {
            "Size" => entry.size = value.parse().unwrap_or(entry.size),
            "MinSize" => entry.min_size = value.parse().unwrap_or(entry.min_size),
            "MaxSize" => entry.max_size = value.parse().unwrap_or(entry.max_size),
            "Threshold" => entry.threshold = value.parse().unwrap_or(entry.threshold),
            "Scale" => entry.scale = value.parse().unwrap_or(entry.scale).max(1),
            "Type" => {
                entry.directory_type = match value {
                    "Scalable" => IconDirectoryType::Scalable,
                    "Threshold" => IconDirectoryType::Threshold,
                    _ => IconDirectoryType::Fixed,
                }
            }
            _ => {}
        }
    }
    directories.sort_by_key(|directory| directory.order);
    directories
}

fn supported_file_names(name: &str) -> Vec<String> {
    ["svg", "png", "jpg", "jpeg", "bmp", "ico", "webp"]
        .into_iter()
        .map(|extension| format!("{name}.{extension}"))
        .collect()
}

fn append_theme_inheritance(
    data_dir: &Path,
    theme: &str,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
) {
    if !visited.insert(theme.to_owned()) {
        return;
    }
    order.push(theme.to_owned());
    let index = data_dir.join("icons").join(theme).join("index.theme");
    let Ok(contents) = fs::read_to_string(index) else {
        return;
    };
    let Some(inherits) = contents
        .lines()
        .find_map(|line| line.strip_prefix("Inherits="))
    else {
        return;
    };
    for inherited in inherits
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        append_theme_inheritance(data_dir, inherited, visited, order);
    }
}

fn find_icons_in_dir(dir: &Path, names: &[String], depth: usize) -> Vec<PathBuf> {
    if depth == 0 {
        return Vec::new();
    }
    let mut entries = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut found = Vec::new();
    for entry in entries {
        let path = entry.path();
        let is_dir = path.is_dir();
        if !is_dir
            && path
                .file_name()
                .is_some_and(|file| names.iter().any(|name| file == OsStr::new(name)))
        {
            found.push(path);
        } else if is_dir {
            found.extend(find_icons_in_dir(&path, names, depth - 1));
        }
    }
    found
}

fn resolve_desktop_entry_icon_in_dirs(entry: &str, data_dirs: &[PathBuf]) -> Option<String> {
    let file_name = if entry.ends_with(".desktop") {
        entry.to_owned()
    } else {
        format!("{entry}.desktop")
    };
    for directory in data_dirs {
        let path = directory.join("applications").join(&file_name);
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let mut in_desktop_entry = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_desktop_entry = line == "[Desktop Entry]";
            } else if in_desktop_entry {
                if let Some(icon) = line.strip_prefix("Icon=") {
                    if !icon.is_empty() {
                        return Some(icon.to_owned());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{NotificationIconMetadata, NotificationSource};
    use image::{ImageBuffer, Rgba};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(suffix: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("xbar-icon-{stamp}-{suffix}"))
    }

    fn write_png(path: &Path) {
        let image = ImageBuffer::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgba([255_u8, 0, 0, 255])
            } else {
                Rgba([0_u8, 255, 0, 128])
            }
        });
        image.save(path).unwrap();
    }

    fn write_solid_png(path: &Path, size: u32, color: [u8; 4]) {
        let image = ImageBuffer::from_pixel(size, size, Rgba(color));
        image.save(path).unwrap();
    }

    fn metadata_for_path(path: &Path) -> NotificationIconMetadata {
        NotificationIconMetadata {
            image_path: Some(path.display().to_string()),
            ..Default::default()
        }
    }

    fn write_theme_index(path: &Path, inherits: Option<&str>) {
        let inherits = inherits.map_or(String::new(), |value| format!("Inherits={value}\n"));
        fs::write(
            path,
            format!("[Icon Theme]\nName=Fixture\n{inherits}Directories=32x32/status\n\n[32x32/status]\nSize=32\nType=Fixed\n"),
        )
        .unwrap();
    }

    fn theme_fixture_root(name: &str) -> (PathBuf, PathBuf) {
        let root = temp_path(name);
        let base = root.join("icons/Base/32x32/status");
        let parent = root.join("icons/Parent/32x32/status");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&parent).unwrap();
        write_theme_index(&root.join("icons/Base/index.theme"), Some("Parent"));
        write_theme_index(&root.join("icons/Parent/index.theme"), None);
        (root, base)
    }

    #[test]
    fn raw_image_data_normalizes_rgb_and_honors_rowstride() {
        let image = NotificationImageData {
            width: 2,
            height: 1,
            rowstride: 8,
            has_alpha: false,
            bits_per_sample: 8,
            channels: 3,
            data: vec![255, 0, 0, 0, 255, 0, 99, 100],
        };
        let resolved = resolve_image_data(&image, None).unwrap();
        assert_eq!(&*resolved.pixels, &[255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn raw_image_data_rejects_invalid_dimensions_and_short_data() {
        for image in [
            NotificationImageData {
                width: 0,
                ..Default::default()
            },
            NotificationImageData {
                width: i32::MAX,
                height: i32::MAX,
                ..Default::default()
            },
            NotificationImageData {
                width: 1,
                height: 1,
                rowstride: 4,
                channels: 4,
                bits_per_sample: 8,
                has_alpha: true,
                data: vec![],
            },
        ] {
            assert!(resolve_image_data(&image, None).is_none());
        }
    }

    #[test]
    fn png_path_resolves_to_canonical_rgba_and_cache_hits() {
        let path = temp_path("icon.png");
        write_png(&path);
        let mut resolver = NotificationIconResolver::new();
        let first = resolver.resolve(&metadata_for_path(&path), None).unwrap();
        let second = resolver.resolve(&metadata_for_path(&path), None).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!((first.width, first.height), (2, 1));
        assert_eq!(first.pixels.len(), 8);
        assert_eq!(resolver.cache_len(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn failed_high_priority_source_falls_through_to_app_icon_path() {
        let path = temp_path("fallback.png");
        write_png(&path);
        let metadata = NotificationIconMetadata {
            image_path: Some("/does/not/exist.png".into()),
            app_icon: Some(path.display().to_string()),
            ..Default::default()
        };
        let mut resolver = NotificationIconResolver::new();
        assert!(resolver.resolve(&metadata, None).is_some());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn theme_name_tries_inherited_raster_after_unsupported_svg() {
        let (root, base) = theme_fixture_root("theme-inherited-raster");
        fs::write(base.join("dialog-information.svg"), b"<svg>").unwrap();
        let parent_icon = root.join("icons/Parent/32x32/status/dialog-information.png");
        write_png(&parent_icon);
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        let icon = resolver
            .resolve(
                &NotificationIconMetadata {
                    app_icon: Some("dialog-information".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!((icon.width, icon.height), (2, 1));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn theme_name_skips_corrupt_candidate_and_uses_valid_inherited_raster() {
        let (root, base) = theme_fixture_root("theme-corrupt-inherited-raster");
        fs::write(base.join("dialog-information.png"), b"corrupt").unwrap();
        let parent_icon = root.join("icons/Parent/32x32/status/dialog-information.png");
        write_png(&parent_icon);
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        assert!(resolver
            .resolve(
                &NotificationIconMetadata {
                    app_icon: Some("dialog-information".into()),
                    ..Default::default()
                },
                None,
            )
            .is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn theme_name_prefers_exact_target_size_with_index_metadata() {
        let root = temp_path("theme-size-ranking");
        let theme = root.join("icons/Base");
        let directories = [
            "16x16/status",
            "22x22/status",
            "24x24/status",
            "32x32/status",
            "48x48/status",
        ];
        for directory in directories {
            fs::create_dir_all(theme.join(directory)).unwrap();
        }
        fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=Base\nDirectories=16x16/status,22x22/status,24x24/status,32x32/status,48x48/status\n\n[16x16/status]\nSize=16\nType=Fixed\n\n[22x22/status]\nSize=22\nType=Fixed\n\n[24x24/status]\nSize=24\nType=Fixed\n\n[32x32/status]\nSize=32\nType=Fixed\n\n[48x48/status]\nSize=48\nType=Fixed\n",
        )
        .unwrap();
        for (directory, color) in [
            ("16x16/status", [16, 0, 0, 255]),
            ("22x22/status", [22, 0, 0, 255]),
            ("24x24/status", [24, 0, 0, 255]),
            ("32x32/status", [32, 0, 0, 255]),
            ("48x48/status", [48, 0, 0, 255]),
        ] {
            write_solid_png(
                &theme.join(directory).join("dialog-information.png"),
                directory[..2].parse().unwrap(),
                color,
            );
        }
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        let icon = resolver
            .resolve(
                &NotificationIconMetadata {
                    app_icon: Some("dialog-information".into()),
                    ..Default::default()
                },
                Some((32, 32)),
            )
            .unwrap();
        assert_eq!(icon.pixels[0], 32);
        assert_eq!((icon.width, icon.height), (32, 32));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn theme_name_uses_nearest_larger_then_smaller_candidate() {
        let root = temp_path("theme-size-fallback");
        let theme = root.join("icons/Base");
        for directory in [
            "16x16/status",
            "24x24/status",
            "48x48/status",
            "64x64/status",
        ] {
            fs::create_dir_all(theme.join(directory)).unwrap();
        }
        fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=Base\nDirectories=16x16/status,24x24/status,48x48/status,64x64/status\n\n[16x16/status]\nSize=16\nType=Fixed\n\n[24x24/status]\nSize=24\nType=Fixed\n\n[48x48/status]\nSize=48\nType=Fixed\n\n[64x64/status]\nSize=64\nType=Fixed\n",
        )
        .unwrap();
        for (directory, size) in [
            ("16x16/status", 16),
            ("24x24/status", 24),
            ("48x48/status", 48),
            ("64x64/status", 64),
        ] {
            write_solid_png(
                &theme.join(directory).join("dialog-warning.png"),
                size,
                [size as u8, 0, 0, 255],
            );
        }
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        let icon = resolver
            .resolve(
                &NotificationIconMetadata {
                    app_icon: Some("dialog-warning".into()),
                    ..Default::default()
                },
                Some((32, 32)),
            )
            .unwrap();
        assert_eq!(icon.pixels[0], 48);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_best_size_falls_through_to_next_ranked_candidate() {
        let root = temp_path("theme-size-corrupt");
        let theme = root.join("icons/Base");
        for directory in ["16x16/status", "32x32/status", "48x48/status"] {
            fs::create_dir_all(theme.join(directory)).unwrap();
        }
        fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=Base\nDirectories=16x16/status,32x32/status,48x48/status\n\n[16x16/status]\nSize=16\nType=Fixed\n\n[32x32/status]\nSize=32\nType=Fixed\n\n[48x48/status]\nSize=48\nType=Fixed\n",
        )
        .unwrap();
        fs::write(
            theme.join("32x32/status/dialog-information.png"),
            b"corrupt",
        )
        .unwrap();
        write_solid_png(
            &theme.join("48x48/status/dialog-information.png"),
            48,
            [48, 0, 0, 255],
        );
        write_solid_png(
            &theme.join("16x16/status/dialog-information.png"),
            16,
            [16, 0, 0, 255],
        );
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        let icon = resolver
            .resolve(
                &NotificationIconMetadata {
                    app_icon: Some("dialog-information".into()),
                    ..Default::default()
                },
                Some((32, 32)),
            )
            .unwrap();
        assert_eq!(icon.pixels[0], 48);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relative_image_path_theme_name_uses_the_generic_name_resolver() {
        let (root, _) = theme_fixture_root("image-path-theme-name");
        let parent_dir = root.join("icons/Parent/32x32/status");
        write_png(&parent_dir.join("dialog-information.png"));
        write_png(&parent_dir.join("dialog-warning.png"));
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        for name in ["dialog-information", "dialog-warning"] {
            assert!(resolver
                .resolve(
                    &NotificationIconMetadata {
                        image_path: Some(name.into()),
                        ..Default::default()
                    },
                    None,
                )
                .is_some());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn theme_name_without_raster_candidates_remains_text_only() {
        let (root, base) = theme_fixture_root("theme-only-svg");
        fs::write(base.join("dialog-information.svg"), b"<svg>").unwrap();
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        assert!(resolver
            .resolve(
                &NotificationIconMetadata {
                    app_icon: Some("dialog-information".into()),
                    ..Default::default()
                },
                None,
            )
            .is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn desktop_entry_theme_name_uses_the_same_inherited_candidate_iteration() {
        let (root, _) = theme_fixture_root("desktop-theme-inherited-raster");
        let applications = root.join("applications");
        fs::create_dir_all(&applications).unwrap();
        write_png(&root.join("icons/Parent/32x32/status/dialog-information.png"));
        fs::write(
            applications.join("fixture.desktop"),
            "[Desktop Entry]\nIcon=dialog-information\n",
        )
        .unwrap();
        let mut resolver =
            NotificationIconResolver::with_search_roots(vec![root.clone()], vec!["Base".into()]);
        assert!(resolver
            .resolve(
                &NotificationIconMetadata {
                    desktop_entry: Some("fixture".into()),
                    ..Default::default()
                },
                None,
            )
            .is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn image_data_has_priority_over_image_path() {
        let path = temp_path("priority.png");
        write_png(&path);
        let metadata = NotificationIconMetadata {
            image_data: Some(NotificationImageData {
                width: 1,
                height: 1,
                rowstride: 4,
                has_alpha: true,
                bits_per_sample: 8,
                channels: 4,
                data: vec![1, 2, 3, 4],
            }),
            image_path: Some(path.display().to_string()),
            ..Default::default()
        };
        let mut resolver = NotificationIconResolver::new();
        let icon = resolver.resolve(&metadata, None).unwrap();
        assert_eq!(&*icon.pixels, &[1, 2, 3, 4]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_uri_and_desktop_entry_use_the_same_raster_resolver() {
        let data_root = temp_path("data");
        let icon_dir = data_root.join("icons/hicolor/48x48/apps");
        let applications = data_root.join("applications");
        fs::create_dir_all(&icon_dir).unwrap();
        fs::create_dir_all(&applications).unwrap();
        let icon_path = icon_dir.join("fixture.png");
        write_png(&icon_path);
        fs::write(
            applications.join("fixture.desktop"),
            "[Desktop Entry]\nName=Fixture\nIcon=fixture\n",
        )
        .unwrap();
        let mut resolver = NotificationIconResolver::with_search_roots(
            vec![data_root.clone()],
            vec!["hicolor".into()],
        );
        let file_uri = format!("file://{}", icon_path.display());
        assert!(resolver
            .resolve(
                &NotificationIconMetadata {
                    image_path: Some(file_uri),
                    ..Default::default()
                },
                None
            )
            .is_some());
        assert!(resolver
            .resolve(
                &NotificationIconMetadata {
                    desktop_entry: Some("fixture".into()),
                    ..Default::default()
                },
                None
            )
            .is_some());
        let _ = fs::remove_dir_all(data_root);
    }

    #[test]
    fn cache_has_a_deterministic_entry_bound_and_target_sizes_are_distinct() {
        let mut cache = IconCache::default();
        for index in 0..(MAX_CACHE_ENTRIES + 4) {
            let icon = Arc::new(ResolvedNotificationIcon {
                width: 1,
                height: 1,
                pixels: Arc::from(vec![index as u8, 0, 0, 255].into_boxed_slice()),
            });
            cache.insert(
                CacheKey {
                    source: format!("fixture-{index}"),
                    target: None,
                },
                icon,
            );
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        let path = temp_path("sizes.png");
        write_png(&path);
        let mut resolver = NotificationIconResolver::new();
        let natural = resolver.resolve(&metadata_for_path(&path), None).unwrap();
        let scaled = resolver
            .resolve(&metadata_for_path(&path), Some((1, 1)))
            .unwrap();
        assert_ne!(
            (natural.width, natural.height),
            (scaled.width, scaled.height)
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn target_size_fits_icons_without_stretching_or_cropping() {
        let square = RgbaImage::new(64, 64);
        let landscape = RgbaImage::new(64, 32);
        let portrait = RgbaImage::new(16, 32);
        assert_eq!(
            (
                normalized_image(square, Some((32, 32))).unwrap().width,
                normalized_image(RgbaImage::new(64, 64), Some((32, 32)))
                    .unwrap()
                    .height
            ),
            (32, 32)
        );
        assert_eq!(
            (
                normalized_image(landscape, Some((32, 32))).unwrap().width,
                normalized_image(RgbaImage::new(64, 32), Some((32, 32)))
                    .unwrap()
                    .height
            ),
            (32, 16)
        );
        assert_eq!(
            (
                normalized_image(portrait, Some((32, 32))).unwrap().width,
                normalized_image(RgbaImage::new(16, 32), Some((32, 32)))
                    .unwrap()
                    .height
            ),
            (16, 32)
        );
    }

    #[test]
    fn unsupported_uri_and_corrupt_file_fail_without_panic() {
        let corrupt = temp_path("corrupt.png");
        fs::write(&corrupt, b"not-an-image").unwrap();
        let mut resolver = NotificationIconResolver::new();
        assert!(resolver
            .resolve(
                &NotificationIconMetadata {
                    image_path: Some("https://example.invalid/icon.png".into()),
                    ..Default::default()
                },
                None
            )
            .is_none());
        assert!(resolver
            .resolve(&metadata_for_path(&corrupt), None)
            .is_none());
        let _ = fs::remove_file(corrupt);
    }

    #[test]
    fn history_resolution_reuses_result_until_replacement_timestamp_changes() {
        let path = temp_path("history.png");
        write_png(&path);
        let metadata = metadata_for_path(&path);
        let history = vec![NotificationHistoryEntry {
            id: HistoryEntryId(1),
            live_notification_id: None,
            source: NotificationSource::Freedesktop,
            app_name: "app".into(),
            summary: "summary".into(),
            body: "body".into(),
            icon_metadata: metadata,
            order: 1,
            received_at: 1,
            updated_at: 1,
        }];
        let mut resolver = NotificationIconResolver::new();
        resolver.resolve_history(&history);
        let first = resolver.history_icon(HistoryEntryId(1)).unwrap();
        resolver.resolve_history(&history);
        let second = resolver.history_icon(HistoryEntryId(1)).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let _ = fs::remove_file(path);
    }
}
