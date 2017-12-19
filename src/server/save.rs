// Copyright 2016 `multipart` Crate Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
//! Utilities for saving request entries to the filesystem.

use mime::Mime;

pub use server::buf_redux::BufReader;

pub use tempdir::TempDir;

use std::collections::HashMap;
use std::io::prelude::*;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::{env, io, mem, usize};

use server::field::{FieldHeaders, MultipartField, MultipartData, ReadEntry, ReadEntryResult};
use server::ArcStr;

use self::SaveResult::*;

const RANDOM_FILENAME_LEN: usize = 12;

fn rand_filename() -> String {
    ::random_alphanumeric(RANDOM_FILENAME_LEN)
}

macro_rules! try_start (
    ($try:expr) => (
        match $try {
            Ok(val) => val,
            Err(e) => return SaveResult::Error(e),
        }
    )
);

macro_rules! try_full (
    ($try:expr) => {
        match $try {
            SaveResult::Full(full) => full,
            other => return other,
        }
    }
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextPolicy {
    /// Attempt to read a text field as text, falling back to binary on error
    Try,
    /// Attempt to read a text field as text, returning any errors
    Force,
    /// Don't try to read text
    Ignore
}

// 8 MiB, reasonable?
const DEFAULT_MEMORY_THRESHOLD: usize = 8 * 1024 * 1024;

/// A builder for saving a file or files to the local filesystem.
///
/// ### `OpenOptions`
/// This builder holds an instance of `std::fs::OpenOptions` which is used
/// when creating the new file(s).
///
/// By default, the open options are set with `.write(true).create_new(true)`,
/// so if the file already exists then an error will be thrown. This is to avoid accidentally
/// overwriting files from other requests.
///
/// If you want to modify the options used to open the save file, you can use
/// `mod_open_opts()`.
///
/// ### File Size and Count Limits
/// You can set a size limit for individual files with `size_limit()`, which takes either `u64`
/// or `Option<u64>`.
///
/// You can also set the maximum number of files to process with `count_limit()`, which
/// takes either `u32` or `Option<u32>`. This only has an effect when using
/// `SaveBuilder<[&mut] Multipart>`.
///
/// ### Warning: Do **not** trust user input!
/// It is a serious security risk to create files or directories with paths based on user input.
/// A malicious user could craft a path which can be used to overwrite important files, such as
/// web templates, static assets, Javascript files, database files, configuration files, etc.,
/// if they are writable by the server process.
///
/// This can be mitigated somewhat by setting filesystem permissions as
/// conservatively as possible and running the server under its own user with restricted
/// permissions, but you should still not use user input directly as filesystem paths.
/// If it is truly necessary, you should sanitize user input such that it cannot cause a path to be
/// misinterpreted by the OS. Such functionality is outside the scope of this crate.
#[must_use = "nothing saved to the filesystem yet"]
pub struct SaveBuilder<S> {
    savable: S,
    open_opts: OpenOptions,
    size_limit: Option<u64>,
    count_limit: Option<u32>,
    memory_threshold: usize,
    text_policy: TextPolicy,
}

impl<S> SaveBuilder<S> {
    /// Implementation detail but not problematic to have accessible.
    #[doc(hidden)]
    pub fn new(savable: S) -> SaveBuilder<S> {
        let mut open_opts = OpenOptions::new();
        open_opts.write(true).create_new(true);

        SaveBuilder {
            savable: savable,
            open_opts: open_opts,
            size_limit: None,
            count_limit: None,
            memory_threshold: DEFAULT_MEMORY_THRESHOLD,
            text_policy: TextPolicy::Try,
        }
    }

    /// Set the maximum number of bytes to write out *per file*.
    ///
    /// Can be `u64` or `Option<u64>`. If `None`, clears the limit.
    pub fn size_limit<L: Into<Option<u64>>>(mut self, limit: L) -> Self {
        self.size_limit = limit.into();
        self
    }

    /// Modify the `OpenOptions` used to open any files for writing.
    ///
    /// The `write` flag will be reset to `true` after the closure returns. (It'd be pretty
    /// pointless otherwise, right?)
    pub fn mod_open_opts<F: FnOnce(&mut OpenOptions)>(mut self, opts_fn: F) -> Self {
        opts_fn(&mut self.open_opts);
        self.open_opts.write(true);
        self
    }

    /// Set the threshold at which to switch from copying a field into memory to copying
    /// it to disk.
    ///
    /// If `0`, forces fields to save directly to the filesystem.
    /// If `usize::MAX`, effectively forces fields to always save to memory.
    ///
    /// (RFC: usize::MAX` is technically reachable on 32-bit systems, should we test for capacity
    /// overflow and switch to disk then? What about if/when `Vec::try_reserve()` becomes a thing?)
    pub fn memory_threshold(self, memory_threshold: usize) -> Self {
        Self { memory_threshold, ..self }
    }

    /// When encountering a field that is apparently text, try to read it to a string or fall
    /// back to binary otherwise.
    ///
    /// Has no effect once `memory_threshold` has been reached.
    pub fn try_text(self) -> Self {
        Self { text_policy: TextPolicy::Try, ..self }
    }

    /// When encountering a field that is apparently text, read it to a string or return an error.
    ///
    /// (RFC: should this continue to validate UTF-8 when writing to the filesystem?)
    pub fn force_text(self) -> Self {
        Self { text_policy: TextPolicy::Force, ..self}
    }

    /// Don't try to read or validate any field data as UTF-8.
    pub fn ignore_text(self) -> Self {
        Self { text_policy: TextPolicy::Ignore, ..self }
    }

    fn fork<M_>(&self, savable: M_) -> SaveBuilder<M_> {
        // this actually forking works
        Self { savable, .. *self }
    }
}

/// Save API for whole multipart requests.
impl<M> SaveBuilder<M> where M: ReadEntry {
    /// Set the maximum number of files to write out.
    ///
    /// Can be `u32` or `Option<u32>`. If `None`, clears the limit.
    pub fn count_limit<L: Into<Option<u32>>>(mut self, count_limit: L) -> Self {
        self.count_limit = count_limit.into();
        self
    }

    /// Save the file fields in the request to a new temporary directory prefixed with
    /// `multipart-rs` in the OS temporary directory.
    ///
    /// For more options, create a `TempDir` yourself and pass it to `with_temp_dir()` instead.
    ///
    /// ### Note: Temporary
    /// See `SaveDir` for more info (the type of `Entries::save_dir`).
    pub fn temp(self) -> EntriesSaveResult<M> {
        self.temp_with_prefix("multipart-rs")
    }

    /// Save the file fields in the request to a new temporary directory with the given string
    /// as a prefix in the OS temporary directory.
    ///
    /// For more options, create a `TempDir` yourself and pass it to `with_temp_dir()` instead.
    ///
    /// ### Note: Temporary
    /// See `SaveDir` for more info (the type of `Entries::save_dir`).
    pub fn temp_with_prefix(self, prefix: &str) -> EntriesSaveResult<M> {
        match TempDir::new(prefix) {
            Ok(tempdir) => self.with_temp_dir(tempdir),
            Err(e) => SaveResult::Error(e),
        }
    }

    /// Save the file fields to the given `TempDir`.
    ///
    /// The `TempDir` is returned in the result under `Entries::save_dir`.
    pub fn with_temp_dir(self, tempdir: TempDir) -> EntriesSaveResult<M> {
        self.with_entries(Entries::new(SaveDir::Temp(tempdir)))
    }

    /// Save the file fields in the request to a new permanent directory with the given path.
    ///
    /// Any nonexistent directories in the path will be created.
    pub fn with_dir<P: Into<PathBuf>>(self, dir: P) -> EntriesSaveResult<M> {
        let dir = dir.into();

        try_start!(create_dir_all(&dir));

        self.with_entries(Entries::new(SaveDir::Perm(dir.into())))
    }

    /// Commence the save operation using the existing `Entries` instance.
    ///
    /// May be used to resume a saving operation after handling an error.
    pub fn with_entries(mut self, mut entries: Entries) -> EntriesSaveResult<M> {
        let mut count = 0;

        loop {
            let field = match ReadEntry::read_entry(self.savable) {
                ReadEntryResult::Entry(field) => field,
                ReadEntryResult::End(_) => break,
                ReadEntryResult::Error(_, e) => return Partial (
                    PartialEntries {
                        entries: entries,
                        partial_file: None,
                    },
                    e.into(),
                )
            };

            match field.data {
                MultipartData::File(mut file) => {
                    match self.count_limit {
                        Some(limit) if count >= limit => return Partial (
                            PartialEntries {
                                entries: entries,
                                partial_file: Some(PartialSavedField {
                                    field_name: field.name,
                                    source: file,
                                    dest: None,
                                })
                            },
                            PartialReason::CountLimit,
                        ),
                        _ => (),
                    }

                    count += 1;

                    match file.save().size_limit(self.size_limit).with_dir(&entries.save_dir) {
                        Full(saved_file) => {
                            self.savable = file.take_inner();
                            entries.mut_files_for(field.name).push(saved_file);
                        },
                        Partial(partial, reason) => return Partial(
                            PartialEntries {
                                entries: entries,
                                partial_file: Some(PartialSavedField {
                                    field_name: field.name,
                                    source: file,
                                    dest: Some(partial)
                                })
                            },
                            reason
                        ),
                        Error(e) => return Partial(
                            PartialEntries {
                                entries: entries,
                                partial_file: Some(PartialSavedField {
                                    field_name: field.name,
                                    source: file,
                                    dest: None,
                                }),
                            },
                            e.into(),
                        ),
                    }
                },
                MultipartData::Text(mut text) => {
                    self.savable = text.take_inner();
                    entries.fields.push((field.name, text.text));
                },
            }
        }

        SaveResult::Full(entries)
    }
}

/// Save API for individual files.
impl<'m, M: 'm> SaveBuilder<&'m mut MultipartData<M>> where MultipartData<M>: BufRead {

    /// Save the field data, potentially using a file with a random name in the
    /// OS temporary directory.
    ///
    /// See `with_path()` for more details.
    pub fn temp(&mut self) -> FieldSaveResult {
        let path = env::temp_dir().join(rand_filename());
        self.with_path(path)
    }

    /// Save the field data, potentially using a file with the given name in
    /// the OS temporary directory.
    ///
    /// See `with_path()` for more details.
    pub fn with_filename(&mut self, filename: &str) -> FieldSaveResult {
        let mut tempdir = env::temp_dir();
        tempdir.set_file_name(filename);

        self.with_path(tempdir)
    }

    /// Save the field data, potentially using a file with a random alphanumeric name
    /// in the given directory.
    ///
    /// See `with_path()` for more details.
    pub fn with_dir<P: AsRef<Path>>(&mut self, dir: P) -> FieldSaveResult {
        let path = dir.as_ref().join(rand_filename());
        self.with_path(path)
    }

    /// Save the field data, potentially using a file with the given path.
    ///
    /// The file will not be created until the set `memory_threshold` is reached.
    ///
    /// Creates any missing directories in the path.
    /// Uses the contained `OpenOptions` to create the file.
    /// Truncates the file to the given limit, if set.
    pub fn with_path<P: Into<PathBuf>>(&mut self, path: P) -> FieldSaveResult {
        let bytes = match try_full!(self.save_mem()) {
            (bytes, true) => return Full(bytes),
            (bytes, false) => bytes,
        };

        // TODO: progressively validate UTF-8 instead
        // it'd perform about the same but could give better throughput as we can do the work
        // while the network buffer refills
        let bytes = match self.text_policy {
            TextPolicy::Try => match String::from_utf8(bytes) {
                Ok(string) => return Full(SavedData::Text(string)),
                Err(e) => bytes,
            },
            TextPolicy::Force => match String::from_utf8(bytes) {
                Ok(string) => return Full(SavedData::Text(string)),
                Err(e) => return Error(io::Error::new(io::ErrorKind::InvalidData, e)),
            },
            TextPolicy::Ignore => bytes,
        };

        let path = path.into();

        let file = match create_dir_all(&path).and_then(|_| self.open_opts.open(&path)) {
            Ok(file) => file,
            Err(e) => return Error(e),
        };

        let data = try_full!(try_write_all(&bytes).map(move |size| SavedData::File(path, size)));

        self.write_to(file).map(move |written| data.add_size(written))
    }


    /// Write out the field data to `dest`, truncating if a limit was set.
    ///
    /// Returns the number of bytes copied, and whether or not the limit was reached
    /// (tested by `MultipartFile::fill_buf().is_empty()` so no bytes are consumed).
    ///
    /// Retries on interrupts.
    pub fn write_to<W: Write>(&mut self, mut dest: W) -> SaveResult<u64, u64> {
        if let Some(limit) = self.size_limit {
            try_copy_limited(&mut self.savable, dest, limit)
        } else {
            try_copy_buf(self.savable, &mut dest)
        }
    }

    fn save_mem(&mut self) -> SaveResult<(Vec<u8>, bool), Vec<u8>> {
        let mut bytes = Vec::new();

        if self.size_limit.map_or(false, |lim| lim < self.memory_threshold) {
            return self.write_to(&mut bytes).map(move |_| bytes);
        }

        match try_copy_limited(self.savable, &mut bytes, self.memory_threshold) {
            Full(_) => Full((bytes, true)),
            Partial(_, PartialReason::SizeLimit) => Full((bytes, false)),
            Partial(_, other) => Partial(bytes, other),
            Error(e) => Error(e),
        }
    }
}

/// A saved field (to memory or filesystem) from a multipart request.
#[derive(Debug)]
pub struct SavedField {
    /// The headers of the field that was saved.
    pub headers: FieldHeaders,
    /// The data of the field which may reside in-memory or on the filesystem.
    pub data: SavedData,
}

#[derive(Debug)]
pub enum SavedData {
    /// Data in the form of a Rust string.
    Text(String),
    Bytes(Vec<u8>),
    /// A path to a file on the filesystem and its size as written by `multipart`.
    File(PathBuf, u64),
}

impl SavedData {
    pub fn readable(&self) -> io::Result<DataReader> {
        use self::SavedData::*;

        match *self {
            Text(ref text) => Ok(DataReader::Bytes(text.as_ref())),
            Bytes(ref bytes) => Ok(DataReader::Bytes(bytes)),
            File(ref path, _) => Ok(DataReader::File(BufReader::new(fs::File::open(path)?))),
        }
    }

    pub fn size(&self) -> u64 {
        use self::SavedData::*;

        match *self {
            Text(ref text) => text.len() as u64,
            Bytes(ref bytes) => bytes.len() as u64,
            File(_, size) => size,
        }
    }

    fn add_size(self, add: u64) -> Self {
        use self::SavedData::File;

        match *self {
            File(path, size) => File(path, size.saturating_add(add)),
            other => other
        }
    }
}

pub enum DataReader<'a> {
    Bytes(&'a [u8]),
    File(BufReader<File>),
}

impl<'a> Read for DataReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use self::DataReader::*;

        match *self {
            Bytes(ref mut bytes) => bytes.read(buf),
            File(ref mut file) => file.read(buf),
        }
    }
}

impl<'a> BufRead for DataReader<'a> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        use self::DataReader::*;

        match *self {
            Bytes(ref mut bytes) => bytes.fill_buf(),
            File(ref mut file) => file.fill_buf(),
        }    }

    fn consume(&mut self, amt: usize) {
        use self::DataReader::*;

        match *self {
            Bytes(ref mut bytes) => bytes.consume(amt),
            File(ref mut file) => file.consume(amt),
        }
    }
}

/// A result of `Multipart::save_all()`.
#[derive(Debug)]
pub struct Entries {
    /// The fields of the multipart request, mapped by field name -> value.
    ///
    /// Each vector is guaranteed not to be empty unless externally modified.
    pub fields: HashMap<ArcStr, Vec<SavedField>>,

    pub save_dir: Option<SaveDir>,
}

impl Entries {
    fn new(save_dir: SaveDir) -> Self {
        Entries {
            fields: HashMap::new(),
            save_dir: save_dir,
        }
    }

    /// Returns `true` if both `fields` and `files` are empty, `false` otherwise.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.files.is_empty()
    }

    fn mut_files_for(&mut self, field: String) -> &mut Vec<SavedField> {
        self.files.entry(field).or_insert_with(Vec::new)
    }
}

/// The save directory for `Entries`. May be temporary (delete-on-drop) or permanent.
#[derive(Debug)]
pub enum SaveDir {
    /// This directory is temporary and will be deleted, along with its contents, when this wrapper
    /// is dropped.
    Temp(TempDir),
    /// This directory is permanent and will be left on the filesystem when this wrapper is dropped.
    ///
    /// **N.B.** If this directory is in the OS temporary directory then it may still be
    /// deleted at any time.
    Perm(PathBuf),
}

impl SaveDir {
    /// Get the path of this directory, either temporary or permanent.
    pub fn as_path(&self) -> &Path {
        use self::SaveDir::*;
        match *self {
            Temp(ref tempdir) => tempdir.path(),
            Perm(ref pathbuf) => &*pathbuf,
        }
    }

    /// Returns `true` if this is a temporary directory which will be deleted on-drop.
    pub fn is_temporary(&self) -> bool {
        use self::SaveDir::*;
        match *self {
            Temp(_) => true,
            Perm(_) => false,
        }
    }

    /// Unwrap the `PathBuf` from `self`; if this is a temporary directory,
    /// it will be converted to a permanent one.
    pub fn into_path(self) -> PathBuf {
        use self::SaveDir::*;

        match self {
            Temp(tempdir) => tempdir.into_path(),
            Perm(pathbuf) => pathbuf,
        }
    }

    /// If this `SaveDir` is temporary, convert it to permanent.
    /// This is a no-op if it already is permanent.
    ///
    /// ### Warning: Potential Data Loss
    /// Even though this will prevent deletion on-drop, the temporary folder on most OSes
    /// (where this directory is created by default) can be automatically cleared by the OS at any
    /// time, usually on reboot or when free space is low.
    ///
    /// It is recommended that you relocate the files from a request which you want to keep to a
    /// permanent folder on the filesystem.
    pub fn keep(&mut self) {
        use self::SaveDir::*;
        *self = match mem::replace(self, Perm(PathBuf::new())) {
            Temp(tempdir) => Perm(tempdir.into_path()),
            old_self => old_self,
        };
    }

    /// Delete this directory and its contents, regardless of its permanence.
    ///
    /// ### Warning: Potential Data Loss
    /// This is very likely irreversible, depending on the OS implementation.
    ///
    /// Files deleted programmatically are deleted directly from disk, as compared to most file
    /// manager applications which use a staging area from which deleted files can be safely
    /// recovered (i.e. Windows' Recycle Bin, OS X's Trash Can, etc.).
    pub fn delete(self) -> io::Result<()> {
        use self::SaveDir::*;
        match self {
            Temp(tempdir) => tempdir.close(),
            Perm(pathbuf) => fs::remove_dir_all(&pathbuf),
        }
    }
}

impl AsRef<Path> for SaveDir {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

/// The reason the save operation quit partway through.
#[derive(Debug)]
pub enum PartialReason {
    /// The count limit for files in the request was hit.
    ///
    /// The associated file has not been saved to the filesystem.
    CountLimit,
    /// The size limit for an individual file was hit.
    ///
    /// The file was partially written to the filesystem.
    SizeLimit,
    /// An error occurred during the operation.
    IoError(io::Error),
}

impl From<io::Error> for PartialReason {
    fn from(e: io::Error) -> Self {
        PartialReason::IoError(e)
    }
}

impl PartialReason {
    /// Return `io::Error` in the `IoError` case or panic otherwise.
    pub fn unwrap_err(self) -> io::Error {
        self.expect_err("`PartialReason` was not `IoError`")
    }

    /// Return `io::Error` in the `IoError` case or panic with the given
    /// message otherwise.
    pub fn expect_err(self, msg: &str) -> io::Error {
        match self {
            PartialReason::IoError(e) => e,
            _ => panic!("{}: {:?}", msg, self),
        }
    }
}

/// The field that was being read when the save operation quit.
///
/// May be partially saved to the filesystem if `dest` is `Some`.
#[derive(Debug)]
pub struct PartialSavedField<M: ReadEntry> {
    pub source: MultipartField<M>,
    /// The partial file's entry on the filesystem, if the operation got that far.
    pub dest: Option<SavedField>,
}

/// The partial result type for `Multipart::save*()`.
///
/// Contains the successfully saved entries as well as the partially
/// saved file that was in the process of being read when the error occurred,
/// if applicable.
#[derive(Debug)]
pub struct PartialEntries<M: ReadEntry> {
    /// The entries that were saved successfully.
    pub entries: Entries,
    /// The field that was in the process of being read. `None` if the error
    /// occurred between entries.
    pub partial_field: Option<PartialSavedField<M>>,
}

/// Discards `partial_file`
impl<M> Into<Entries> for PartialEntries<M> {
    fn into(self) -> Entries {
        self.entries
    }
}

impl<M> PartialEntries<M> {
    /// If `partial_file` is present and contains a `SavedFile` then just
    /// add it to the `Entries` instance and return it.
    ///
    /// Otherwise, returns `self.entries`
    pub fn keep_partial(mut self) -> Entries {
        if let Some(partial_file) = self.partial_file {
            if let Some(saved_file) = partial_file.dest {
                self.entries.mut_files_for(partial_file.field_name).push(saved_file);
            }
        }

        self.entries
    }
}

/// The ternary result type used for the `SaveBuilder<_>` API.
#[derive(Debug)]
pub enum SaveResult<Success, Partial> {
    /// The operation was a total success. Contained is the complete result.
    Full(Success),
    /// The operation quit partway through. Included is the partial
    /// result along with the reason.
    Partial(Partial, PartialReason),
    /// An error occurred at the start of the operation, before anything was done.
    Error(io::Error),
}

/// Shorthand result for methods that return `Entries`
pub type EntriesSaveResult<M> = SaveResult<Entries, PartialEntries<M>>;

/// Shorthand result for methods that return `FieldData`s.
///
/// The `MultipartData` is not provided here because it is not necessary to return
/// a borrow when the owned version is probably in the same scope. This hopefully
/// saves some headache with the borrow-checker.
pub type FieldSaveResult = SaveResult<SavedData, SavedData>;

impl<M> EntriesSaveResult<M> {
    /// Take the `Entries` from `self`, if applicable, and discarding
    /// the error, if any.
    pub fn into_entries(self) -> Option<Entries> {
        match self {
            Full(entries) | Partial(PartialEntries { entries, .. }, _) => Some(entries),
            Error(_) => None,
        }
    }
}

impl<S, P> SaveResult<S, P> where P: Into<S> {
    /// Convert `self` to `Option<S>`; there may still have been an error.
    pub fn okish(self) -> Option<S> {
        self.into_opt_both().0
    }

    /// Map the `Full` or `Partial` values to a new type, retaining the reason
    /// in the `Partial` case.
    pub fn map<T, Map>(self, map: Map) -> SaveResult<T, T> where Map: FnOnce(S) -> T {
        match self {
            Full(full) => Full(map(full)),
            Partial(partial, reason) => Partial(map(partial.into()), reason),
            Error(e) => Error(e),
        }
    }

    /// Decompose `self` to `(Option<S>, Option<io::Error>)`
    pub fn into_opt_both(self) -> (Option<S>, Option<io::Error>) {
        match self {
            Full(full)  => (Some(full), None),
            Partial(partial, PartialReason::IoError(e)) => (Some(partial.into()), Some(e)),
            Partial(partial, _) => (Some(partial.into()), None),
            Error(error) => (None, Some(error)),
        }
    }

    /// Map `self` to an `io::Result`, discarding the error in the `Partial` case.
    pub fn into_result(self) -> io::Result<S> {
        match self {
            Full(entries) => Ok(entries),
            Partial(partial, _) => Ok(partial.into()),
            Error(error) => Err(error),
        }
    }

    /// Pessimistic version of `into_result()` which will return an error even
    /// for the `Partial` case.
    ///
    /// ### Note: Possible Storage Leak
    /// It's generally not a good idea to ignore the `Partial` case, as there may still be a
    /// partially written file on-disk. If you're not using a temporary directory
    /// (OS-managed or via `TempDir`) then partially written files will remain on-disk until
    /// explicitly removed which could result in excessive disk usage if not monitored closely.
    pub fn into_result_strict(self) -> io::Result<S> {
        match self {
            Full(entries) => Ok(entries),
            Partial(_, PartialReason::IoError(e)) | Error(e) => Err(e),
            Partial(partial, _) => Ok(partial.into()),
        }
    }
}

fn create_dir_all(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
    } else {
        // RFC: return an error instead?
        warn!("Attempting to save file in what looks like a root directory. File path: {:?}", path);
        Ok(())
    }
}

fn try_copy_limited<R: BufRead, W: Write>(mut src: R, mut dest: W, limit: u64) -> SaveResult<u64, u64> {
    let copied = try_full!(try_copy_buf(src.by_ref().take(limit), &mut dest));

    // If there's more data to be read, the field was truncated
    match src.fill_buf() {
        Ok(buf) if buf.is_empty() => Full(copied),
        Ok(_) => Partial(copied, PartialReason::SizeLimit),
        Err(e) => Partial(copied, PartialReason::IoError(e))
    }
}

fn try_copy_buf<R: BufRead, W: Write>(mut src: R, mut dest: W) -> SaveResult<u64, u64> {
    let mut total_copied = 0u64;

    macro_rules! try_here (
        ($try:expr) => (
            match $try {
                Ok(val) => val,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return if total_copied == 0 { Error(e) }
                                 else { Partial(total_copied, e.into()) },
            }
        )
    );

    loop {
        let res = {
            let buf = try_here!(src.fill_buf());
            if buf.is_empty() { break; }
            try_write_all(buf, &mut dest)
        };

        match res {
            Full(copied) => { src.consume(copied); total_copied += copied as u64; }
            Partial(copied, reason) => {
                src.consume(copied); total_copied += copied as u64;
                return Partial(total_copied, reason);
            },
            Error(err) => {
                return Partial(total_copied, err.into());
            }
        }
    }

    Full(total_copied)
}

fn try_write_all<W>(mut buf: &[u8], mut dest: W) -> SaveResult<usize, usize> where W: Write {
    let mut total_copied = 0;

    macro_rules! try_here (
        ($try:expr) => (
            match $try {
                Ok(val) => val,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return if total_copied == 0 { Error(e) }
                                 else { Partial(total_copied, e.into()) },
            }
        )
    );

    while !buf.is_empty() {
        match try_here!(dest.write(buf)) {
            0 => try_here!(Err(io::Error::new(io::ErrorKind::WriteZero,
                                          "failed to write whole buffer"))),
            copied => {
                buf = &buf[copied..];
                total_copied += copied;
            },
        }
    }

    Full(total_copied)
}
