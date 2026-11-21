use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileEntryId(usize);

#[derive(Debug)]
pub struct FileEntry {
    pub file_path: PathBuf,
    pub size: u32,
}

/// A reference to a file entry that can be used to retrieve the file entry from the `FileManager`.
/// Using this ref with a different FileManager is a logic error.
/// The equality and hash of a `FileEntryRef` are based on the `FileEntry` which it refers to.
#[derive(Debug, Clone)]
pub struct FileEntryRef {
    /// The ID of the `FileManager` that owns the referenced `FileEntry`.
    manager_id: FileManagerId,
    /// The path as requested (e.g. as written in `#include`). May differ from `FileEntry::file_path` when one file is reached via different paths.
    include_path: PathBuf,
    file_entry_id: FileEntryId,
}

impl PartialEq for FileEntryRef {
    fn eq(&self, other: &Self) -> bool {
        self.manager_id == other.manager_id && self.file_entry_id == other.file_entry_id
    }
}

impl Eq for FileEntryRef {}

impl Hash for FileEntryRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.manager_id.hash(state);
        self.file_entry_id.hash(state);
    }
}

/// A unique identifier for a file, based on its (device, inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UniqueFileId(u64, u64);

static NEXT_FILE_MANAGER_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileManagerId(u64);

impl FileManagerId {
    fn new() -> Self {
        let id = NEXT_FILE_MANAGER_ID.fetch_add(1, Relaxed);
        Self(id)
    }
}

/// A manager for file entries, which keeps track of unique files and owns their corresponding file entries.
#[derive(Debug)]
pub struct FileManager {
    id: FileManagerId,
    files: Vec<FileEntry>,
    unique_files: HashMap<UniqueFileId, FileEntryId>,
    seen_files: HashMap<PathBuf, FileEntryId>,
}

impl FileManager {
    pub fn new() -> Self {
        Self {
            id: FileManagerId::new(),
            files: Vec::new(),
            unique_files: HashMap::new(),
            seen_files: HashMap::new(),
        }
    }

    pub fn get_file_entry(&self, file_entry_id: FileEntryId) -> Option<&FileEntry> {
        self.files.get(file_entry_id.0)
    }

    pub fn get_file_ref(&mut self, file_path: &Path) -> Option<FileEntryRef> {
        if let Some(&file_entry_id) = self.seen_files.get(file_path) {
            return Some(FileEntryRef {
                manager_id: self.id,
                include_path: PathBuf::from(file_path),
                file_entry_id,
            });
        }

        // https://github.com/llvm/llvm-project/blob/main/llvm/lib/Support/Unix/Path.inc

        // TODO: Separate the following logic into FileSystem as LLVM does

        let file_status = fs::metadata(file_path).ok()?;

        let unique_file_id = UniqueFileId(file_status.dev(), file_status.ino());
        let file_entry_id = if let Some(&file_entry_id) = self.unique_files.get(&unique_file_id) {
            file_entry_id
        } else {
            let file_entry = FileEntry {
                file_path: PathBuf::from(file_path),
                size: u32::try_from(file_status.len()).unwrap(),
            };
            self.files.push(file_entry);
            let file_entry_id = FileEntryId(self.files.len() - 1);
            self.unique_files.insert(unique_file_id, file_entry_id);
            file_entry_id
        };

        self.seen_files
            .insert(PathBuf::from(file_path), file_entry_id);

        Some(FileEntryRef {
            manager_id: self.id,
            include_path: file_path.into(),
            file_entry_id,
        })
    }

    pub fn get_buf_for_file(&self, file_entry_ref: &FileEntryRef) -> Option<Vec<u8>> {
        let file_entry = self.files.get(file_entry_ref.file_entry_id.0)?;
        fs::read(&file_entry.file_path).ok()
    }
}

impl Default for FileManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A cache for the contents of a file or memory buffer, which is lazily loaded when requested.
#[derive(Debug, Clone)]
pub struct ContentCache {
    /// The buffer containing the contents of the file or memory buffer. This is lazily loaded when requested by `get_buf()`.
    buf: Option<Vec<u8>>,
    contents_entry: Option<FileEntryRef>,
    // line_offsets: Vec<u32>,
}

impl ContentCache {
    pub fn new(contents_entry: Option<FileEntryRef>) -> Self {
        Self {
            buf: None,
            contents_entry,
            // line_offsets: Vec::new(),
        }
    }

    pub fn get_buf(&mut self, file_manager: &FileManager) -> Option<&[u8]> {
        if self.buf.is_some() {
            return self.buf.as_deref();
        }

        self.buf = file_manager.get_buf_for_file(self.contents_entry.as_ref()?);
        self.buf.as_deref()
    }

    pub fn set_buf(&mut self, buf: &[u8]) {
        self.buf = Some(buf.to_vec());
    }

    pub fn get_size(&self, file_manager: &FileManager) -> u32 {
        if let Some(buf) = &self.buf {
            return buf.len() as u32;
        }

        if let Some(contents_entry) = &self.contents_entry {
            file_manager
                .get_file_entry(contents_entry.file_entry_id)
                .unwrap()
                .size
        } else {
            0
        }
    }
}

/// An encoded position in the global source-location space shared by all files and macro expansions.
/// The low 31 bits hold the offset; the high bit is set for locations inside macro expansions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceLocation(NonZeroU32);

impl SourceLocation {
    const MACRO_ID_BIT: u32 = 1 << 31;

    pub fn to_global_offset(self) -> u32 {
        self.0.get() & !Self::MACRO_ID_BIT
    }

    pub fn from_file_loc(loc: u32) -> Self {
        Self(NonZeroU32::new(loc).unwrap())
    }

    pub fn from_macro_loc(loc: u32) -> Self {
        Self(NonZeroU32::new(loc | Self::MACRO_ID_BIT).unwrap())
    }

    pub fn is_file_id(self) -> bool {
        self.0.get() & Self::MACRO_ID_BIT == 0
    }

    pub fn is_macro_id(self) -> bool {
        self.0.get() & Self::MACRO_ID_BIT != 0
    }
}

/// A unique identifier for a file, buffer or a macro expansion to identify the corresponding `SLocEntry` in the `SourceManager`.
/// Note that `FileId` is not the same as `FileEntryId`, which is used to retrieve the corresponding `FileEntry` from the `FileManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(NonZeroU32);

impl FileId {
    pub fn from_index(index: usize) -> Self {
        let index = index
            .checked_add(1)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap();
        Self(NonZeroU32::new(index).unwrap())
    }

    pub fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

/// A contiguous range of the source-location space, backed by either a file/buffer or a macro expansion.
#[derive(Debug)]
pub struct SLocEntry {
    pub base_offset: u32,
    pub kind: SLocEntryKind,
}

impl SLocEntry {
    pub fn is_file(&self) -> bool {
        matches!(self.kind, SLocEntryKind::File { .. })
    }

    pub fn is_macro(&self) -> bool {
        matches!(self.kind, SLocEntryKind::Macro)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId(usize);

#[derive(Debug, Clone)]
pub enum SLocEntryKind {
    File {
        include_loc: Option<SourceLocation>,
        content: ContentId,
    },
    // TODO
    Macro,
}

#[derive(Debug)]
pub struct SourceManager {
    // TODO: pass by reference?
    file_manager: FileManager,

    /// The list of content caches for files and memory buffers.
    /// The ownership of the content caches is held by the `SourceManager`, and they are shared with `SLocEntry`s.
    ///
    /// In Clang, the content cache is allocated by `llvm::BumpPtrAllocator` and owned by the `SourceManager`.
    contents: Vec<ContentCache>,

    /// The mapping from `FileEntryRef` to `ContentId`, which is used to retrieve the corresponding `ContentCache` from the `contents` vector.
    file_infos: HashMap<FileEntryRef, ContentId>,

    /// The list of source location entries, which are used to map from `SourceLocation` to the corresponding file or macro expansion.
    sloc_entries: Vec<SLocEntry>,
    /// The starting offset of the next `SLocEntry`.
    /// This is in global offset space, which is the same as the offset of `SourceLocation`.
    next_offset: u32,
}

impl SourceManager {
    pub fn new() -> Self {
        Self {
            file_manager: FileManager::new(),
            contents: Vec::new(),
            file_infos: HashMap::new(),
            sloc_entries: vec![
                // dummy entry for invalid SourceLocation
                SLocEntry {
                    base_offset: 0,
                    kind: SLocEntryKind::Macro,
                },
            ],
            // 0 and 1 are reserved for invalid SourceLocation.
            // The length of the dummy entry is 1, but end-of-file marker takes 1 more byte, so the next offset is 2.
            // See create_file_id_impl().
            next_offset: 2,
        }
    }

    pub fn get_or_create_content_cache(&mut self, file_entry_ref: &FileEntryRef) -> ContentId {
        // Since FileEntryRef's Eq/Hash are based on the underlying FileEntry, different paths to the same file share one ContentCache.
        if let Some(&content_id) = self.file_infos.get(file_entry_ref) {
            return content_id;
        }

        let content_cache = ContentCache::new(Some(file_entry_ref.clone()));
        self.contents.push(content_cache);
        let content_id = ContentId(self.contents.len() - 1);
        self.file_infos.insert(file_entry_ref.clone(), content_id);

        content_id
    }

    pub fn create_content_cache_for_mem_buf(&mut self, buf: &[u8]) -> ContentId {
        let content_cache = ContentCache::new(None);
        self.contents.push(content_cache);
        let content_id = ContentId(self.contents.len() - 1);
        self.contents.last_mut().unwrap().set_buf(buf);

        content_id
    }

    pub fn create_file_id_for_file(
        &mut self,
        source_file: &FileEntryRef,
        include_pos: Option<SourceLocation>,
    ) -> Result<FileId> {
        let content_id = self.get_or_create_content_cache(source_file);
        self.create_file_id_impl(
            content_id,
            source_file.include_path.display().to_string().as_str(),
            include_pos,
        )
    }

    pub fn create_file_id_for_buf(
        &mut self,
        file: &[u8],
        include_pos: Option<SourceLocation>,
    ) -> Result<FileId> {
        let content_id = self.create_content_cache_for_mem_buf(file);
        self.create_file_id_impl(content_id, "<memory buffer>", include_pos)
    }

    fn create_file_id_impl(
        &mut self,
        file: ContentId,
        file_name: &str,
        include_pos: Option<SourceLocation>,
    ) -> Result<FileId> {
        let file_size_with_eof = self.contents[file.0]
            .get_size(&self.file_manager)
            // +1 means the end of file marker.
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("File size overflow for file: {}", file_name))?;

        let next_offset = self
            .next_offset
            .checked_add(file_size_with_eof)
            .ok_or_else(|| anyhow::anyhow!("File size overflow for file: {}", file_name))?;

        let file_id = FileId::from_index(self.sloc_entries.len());

        self.sloc_entries.push(SLocEntry {
            base_offset: self.next_offset,
            kind: SLocEntryKind::File {
                include_loc: include_pos,
                content: file,
            },
        });

        self.next_offset = next_offset;

        Ok(file_id)
    }

    pub fn get_buf_data(&mut self, file_id: FileId) -> Option<&[u8]> {
        if let SLocEntryKind::File { content, .. } = self.sloc_entries.get(file_id.index())?.kind {
            self.contents[content.0].get_buf(&self.file_manager)
        } else {
            None
        }
    }

    pub fn get_loc_for_start_of_file(&self, file_id: FileId) -> Option<SourceLocation> {
        self.sloc_entries
            .get(file_id.index())
            // The first SLocEntry is a dummy entry for invalid SourceLocation, so we skip it.
            .filter(|entry| entry.base_offset != 0)
            .map(|entry| SourceLocation::from_file_loc(entry.base_offset))
    }

    pub fn get_composed_loc(&self, file_id: FileId, offset: u32) -> Option<SourceLocation> {
        let entry = self.sloc_entries.get(file_id.index())?;
        let global_offset = entry.base_offset + offset;
        if entry.is_file() {
            Some(SourceLocation::from_file_loc(global_offset))
        } else {
            Some(SourceLocation::from_macro_loc(global_offset))
        }
    }

    pub fn get_decomposed_loc(&self, loc: SourceLocation) -> Option<(FileId, u32)> {
        let fid = self.get_file_id(loc)?;
        let entry = self.sloc_entries.get(fid.index())?;
        Some((fid, loc.to_global_offset() - entry.base_offset))
    }

    pub fn get_file_id(&self, loc: SourceLocation) -> Option<FileId> {
        if loc.to_global_offset() < self.next_offset {
            // binary search
            self.sloc_entries
                .partition_point(|entry| entry.base_offset <= loc.to_global_offset())
                .checked_sub(1)
                .map(FileId::from_index)
        } else {
            None
        }
    }

    pub fn get_file_id_size(&self, file_id: FileId) -> u32 {
        let next_offset = if file_id.index() + 1 == self.sloc_entries.len() {
            // If this is the last SLocEntry, the next offset is the next_offset of the SourceManager.
            self.next_offset
        } else {
            // Otherwise, the next offset is the base_offset of the next SLocEntry.
            self.sloc_entries[file_id.index() + 1].base_offset
        };
        // Ignore the end-of-file marker by subtracting 1 from the size.
        // See create_file_id_impl().
        next_offset - self.sloc_entries[file_id.index()].base_offset - 1
    }

    #[allow(unused_variables)]
    pub fn get_line_number(&self, loc: SourceLocation) -> Option<u32> {
        todo!()
    }

    #[allow(unused_variables)]
    pub fn get_column_number(&self, loc: SourceLocation) -> Option<u32> {
        todo!()
    }
}

impl Default for SourceManager {
    fn default() -> Self {
        Self::new()
    }
}
