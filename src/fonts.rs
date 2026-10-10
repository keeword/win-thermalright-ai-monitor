//! Font bytes shared by the LCD renderer and egui, whose borrowed fonts require
//! static storage. Each startup font is retained once until process exit.
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

static FONT_BYTES: OnceLock<Mutex<HashMap<PathBuf, &'static [u8]>>> = OnceLock::new();

pub fn directory() -> PathBuf {
    std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "C:/Windows".into())
        .join("Fonts")
}

pub fn default_path() -> PathBuf {
    directory().join("msyh.ttc")
}

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
        let storage = Box::leak(Box::new(FontStorage::open(&path)?));
        storage.as_slice()
    };
    #[cfg(not(windows))]
    let bytes: &'static [u8] = Box::leak(std::fs::read(&path)?.into_boxed_slice());
    fonts.insert(path, bytes);
    Ok(bytes)
}

#[cfg(windows)]
enum FontStorage {
    Mapped(MappedFont),
    Owned(Box<[u8]>),
}

#[cfg(windows)]
impl FontStorage {
    fn open(path: &Path) -> Result<Self> {
        match MappedFont::open(path) {
            Ok(mapped) => Ok(Self::Mapped(mapped)),
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    error.raw_os_error()
                        == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
                }) =>
            {
                // An existing writer/deleter can prevent a protected mapping.
                // A private copy remains immutable even if that handle changes
                // the file later; never relax the mapped file's sharing mode.
                let bytes = std::fs::read(path).with_context(|| {
                    format!("Reading font {} after sharing conflict", path.display())
                })?;
                Ok(Self::Owned(bytes.into_boxed_slice()))
            }
            Err(error) => Err(error),
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Mapped(mapped) => &mapped.data,
            Self::Owned(bytes) => bytes,
        }
    }
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
    fn sharing_conflict_uses_an_immutable_private_copy() {
        use std::io::{Seek, SeekFrom, Write};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("font.bin");
        std::fs::write(&path, b"original font bytes").unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let font = FontStorage::open(&path).unwrap();
        assert!(matches!(font, FontStorage::Owned(_)));
        writer.seek(SeekFrom::Start(0)).unwrap();
        writer.write_all(b"modified font bytes").unwrap();
        writer.flush().unwrap();
        assert_eq!(font.as_slice(), b"original font bytes");
        assert_eq!(std::fs::read(&path).unwrap(), b"modified font bytes");
    }

    #[test]
    fn ordinary_font_uses_mapping_and_exclusive_access_still_reports_an_error() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("font.bin");
        std::fs::write(&path, b"font bytes").unwrap();
        assert!(matches!(
            FontStorage::open(&path).unwrap(),
            FontStorage::Mapped(_)
        ));
        let _exclusive = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(FontStorage::open(&path).is_err());
    }

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
