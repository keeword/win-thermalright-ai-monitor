//! Font bytes shared by the LCD renderer and egui, whose borrowed fonts require
//! static storage. Each startup font is retained once until process exit.
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

static FONT_BYTES: OnceLock<Mutex<HashMap<PathBuf, &'static [u8]>>> = OnceLock::new();

pub fn bytes(path: &Path) -> Result<&'static [u8]> {
    let path = path
        .canonicalize()
        .with_context(|| format!("Finding font {}", path.display()))?;
    let mut fonts = FONT_BYTES.get_or_init(Default::default).lock().unwrap();
    if let Some(bytes) = fonts.get(&path) {
        return Ok(bytes);
    }
    #[cfg(windows)]
    let bytes = {
        let mapped = Box::leak(Box::new(MappedFont::open(&path)?));
        &mapped.data[..]
    };
    #[cfg(not(windows))]
    let bytes: &'static [u8] = Box::leak(std::fs::read(&path)?.into_boxed_slice());
    fonts.insert(path, bytes);
    Ok(bytes)
}

#[cfg(windows)]
struct MappedFont {
    data: memmap2::Mmap,
    // Keep the read-only sharing handle alive so other processes cannot change
    // or truncate bytes borrowed by the font parsers. Drop the map first.
    _file: std::fs::File,
}
#[cfg(windows)]
impl MappedFont {
    fn open(path: &Path) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .with_context(|| format!("Opening font {} for read-only mapping", path.display()))?;
        // SAFETY: FILE_SHARE_READ disallows writers/deletion while _file remains
        // open, and the immutable map is owned alongside that handle.
        let data = unsafe { memmap2::MmapOptions::new().map(&file) }
            .with_context(|| format!("Mapping font {}", path.display()))?;
        Ok(Self { data, _file: file })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn mapped_bytes_cannot_be_changed_until_the_font_is_released() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("font.bin");
        std::fs::write(&path, b"immutable font bytes").unwrap();
        let font = MappedFont::open(&path).unwrap();
        assert_eq!(&font.data[..], b"immutable font bytes");
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        drop(font);
        std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }
}
