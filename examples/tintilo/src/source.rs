use std::cell::OnceCell;
use std::collections::HashMap;
use std::fs;
use std::hash::Hash;
use std::num::NonZeroU32;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileEntryId(usize);

/// A reference to a file entry that can be used to retrieve the file entry from the `FileManager`.
/// Using this ref with a different FileManager is a logic error.
#[derive(Debug, Clone)]
pub struct FileEntryRef {
    /// The ID of the `FileManager` that owns the referenced `FileEntryId`.
    manager_id: FileManagerId,
    /// The path as requested (e.g. as written in `#include`).
    include_path: PathBuf,
    file_entry_id: FileEntryId,
}

impl FileEntryRef {
    pub fn id(&self) -> FileEntryId {
        self.file_entry_id
    }

    pub fn name(&self) -> &Path {
        &self.include_path
    }

    pub fn dir(&self) -> Option<&Path> {
        self.include_path.parent()
    }

    pub fn is_same_file(&self, other: &Self) -> bool {
        self.manager_id == other.manager_id && self.file_entry_id == other.file_entry_id
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
    unique_files: HashMap<UniqueFileId, FileEntryId>,
    seen_files: HashMap<PathBuf, FileEntryId>,
    next_file_entry_id: usize,
}

impl FileManager {
    pub fn new() -> Self {
        Self {
            id: FileManagerId::new(),
            unique_files: HashMap::new(),
            seen_files: HashMap::new(),
            next_file_entry_id: 0,
        }
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
            let file_entry_id = FileEntryId(self.next_file_entry_id);
            self.next_file_entry_id += 1;
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

    pub fn get_buf_for_file(&self, file_entry_ref: &FileEntryRef) -> Result<Vec<u8>> {
        debug_assert_eq!(self.id, file_entry_ref.manager_id);

        fs::read(&file_entry_ref.include_path)
            .with_context(|| format!("cannot open {}", file_entry_ref.include_path.display()))
    }
}

impl Default for FileManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A cache for the contents of a file or memory buffer.
#[derive(Debug, Clone)]
pub struct ContentCache {
    /// The buffer containing the contents of the file or memory buffer.
    buf: Rc<[u8]>,
    #[allow(unused)]
    line_offsets: OnceCell<Vec<u32>>,
}

impl ContentCache {
    pub fn new(buf: &[u8]) -> Self {
        Self {
            buf: Rc::from(buf),
            line_offsets: OnceCell::new(),
        }
    }

    pub fn get_buf(&self) -> Rc<[u8]> {
        Rc::clone(&self.buf)
    }

    pub fn get_size(&self) -> u32 {
        self.buf.len() as u32
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileRefId(usize);

#[derive(Debug, Clone)]
pub enum SLocEntryKind {
    File {
        include_loc: Option<SourceLocation>,
        content: ContentId,
        file_ref: Option<FileRefId>,
    },
    // TODO
    Macro,
}

#[derive(Debug)]
pub struct SourceManager {
    /// The list of content caches for files and memory buffers.
    /// The ownership of the content caches is held by the `SourceManager`, and they are shared with `SLocEntry`s.
    ///
    /// In Clang, the content cache is allocated by `llvm::BumpPtrAllocator` and owned by the `SourceManager`.
    contents: Vec<ContentCache>,

    /// The mapping from `FileEntryRef` to `ContentId`, which is used to retrieve the corresponding `ContentCache` from the `contents` vector.
    file_infos: HashMap<FileEntryId, ContentId>,

    /// The list of source location entries, which are used to map from `SourceLocation` to the corresponding file or macro expansion.
    sloc_entries: Vec<SLocEntry>,
    /// The starting offset of the next `SLocEntry`.
    /// This is in global offset space, which is the same as the offset of `SourceLocation`.
    next_offset: u32,

    file_refs: Vec<FileEntryRef>,
}

impl SourceManager {
    pub fn new() -> Self {
        Self {
            contents: Vec::new(),
            file_infos: HashMap::new(),
            sloc_entries: vec![],
            // 0 is invalid SourceLocation.
            next_offset: 1,
            file_refs: vec![],
        }
    }

    pub fn get_file_entry_ref_for_id(&self, file_id: FileId) -> Option<&FileEntryRef> {
        match self.sloc_entries.get(file_id.index())?.kind {
            SLocEntryKind::File {
                file_ref: Some(id), ..
            } => self.file_refs.get(id.0),
            _ => None,
        }
    }

    pub fn get_or_create_content_cache(
        &mut self,
        file_manager: &FileManager,
        file_entry_ref: &FileEntryRef,
    ) -> Result<ContentId> {
        // Different paths to the same file share one ContentCache.
        if let Some(&content_id) = self.file_infos.get(&file_entry_ref.id()) {
            return Ok(content_id);
        }

        let buf = file_manager.get_buf_for_file(file_entry_ref)?;
        let content_cache = ContentCache::new(&buf);
        self.contents.push(content_cache);
        let content_id = ContentId(self.contents.len() - 1);
        self.file_infos.insert(file_entry_ref.id(), content_id);

        Ok(content_id)
    }

    pub fn create_content_cache_for_mem_buf(&mut self, buf: &[u8]) -> Result<ContentId> {
        let content_cache = ContentCache::new(buf);
        self.contents.push(content_cache);
        let content_id = ContentId(self.contents.len() - 1);

        Ok(content_id)
    }

    pub fn create_file_id_for_file(
        &mut self,
        file_manager: &FileManager,
        source_file: &FileEntryRef,
        include_pos: Option<SourceLocation>,
    ) -> Result<FileId> {
        let content_id = self.get_or_create_content_cache(file_manager, source_file)?;
        self.create_file_id_impl(content_id, Some(source_file), include_pos)
    }

    pub fn create_file_id_for_buf(
        &mut self,
        file: &[u8],
        include_pos: Option<SourceLocation>,
    ) -> Result<FileId> {
        let content_id = self.create_content_cache_for_mem_buf(file)?;
        self.create_file_id_impl(content_id, None, include_pos)
    }

    fn create_file_id_impl(
        &mut self,
        file: ContentId,
        file_ref: Option<&FileEntryRef>,
        include_pos: Option<SourceLocation>,
    ) -> Result<FileId> {
        let file_path = if let Some(file_ref) = file_ref {
            file_ref.include_path.display().to_string()
        } else {
            "<memory buffer>".to_string()
        };

        let file_size_with_eof = self.contents[file.0]
            .get_size()
            // +1 means EOF.
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("File size overflow for file: {}", file_path))?;

        let next_offset = self
            .next_offset
            .checked_add(file_size_with_eof)
            .ok_or_else(|| anyhow::anyhow!("File size overflow for file: {}", file_path))?;

        let file_id = FileId::from_index(self.sloc_entries.len());

        let file_ref_id = file_ref.map(|r| {
            self.file_refs.push(r.clone());
            FileRefId(self.file_refs.len() - 1)
        });

        self.sloc_entries.push(SLocEntry {
            base_offset: self.next_offset,
            kind: SLocEntryKind::File {
                include_loc: include_pos,
                content: file,
                file_ref: file_ref_id,
            },
        });

        self.next_offset = next_offset;

        Ok(file_id)
    }

    pub fn get_buf_data(&self, file_id: FileId) -> Option<Rc<[u8]>> {
        if let SLocEntryKind::File { content, .. } = self.sloc_entries.get(file_id.index())?.kind {
            Some(self.contents[content.0].get_buf())
        } else {
            None
        }
    }

    pub fn get_loc_for_start_of_file(&self, file_id: FileId) -> Option<SourceLocation> {
        self.sloc_entries
            .get(file_id.index())
            .filter(|entry| entry.is_file())
            .map(|entry| SourceLocation::from_file_loc(entry.base_offset))
    }

    pub fn get_composed_loc(&self, file_id: FileId, offset: u32) -> Option<SourceLocation> {
        // NOTE: The EOF appears where offset == get_file_id_size(file_id).
        debug_assert!(offset <= self.get_file_id_size(file_id)?);

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

    pub fn get_file_id_size(&self, file_id: FileId) -> Option<u32> {
        let next_offset = if file_id.index() + 1 == self.sloc_entries.len() {
            // If this is the last SLocEntry, the next offset is the next_offset of the SourceManager.
            self.next_offset
        } else {
            // Otherwise, the next offset is the base_offset of the next SLocEntry.
            self.sloc_entries.get(file_id.index() + 1)?.base_offset
        };
        // Ignore the EOF by subtracting 1 from the size.
        Some(next_offset - self.sloc_entries.get(file_id.index())?.base_offset - 1)
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
