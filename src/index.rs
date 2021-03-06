// Conserve backup system.
// Copyright 2015, 2016, 2017, 2018, 2019, 2020 Martin Pool.

//! Index lists the files in a band in the archive.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::vec;

use globset::GlobSet;
use snafu::ResultExt;

use super::io::file_exists;
use super::*;

pub const MAX_ENTRIES_PER_HUNK: usize = 1000;

/// Description of one archived file.
///
/// This struct is directly encoded/decoded to the json index file, and also can be constructed by
/// stat-ing (but not reading) a live file.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    /// Path of this entry relative to the base of the backup, in `apath` form.
    pub apath: Apath,

    /// Type of file.
    pub kind: Kind,

    /// File modification time, in whole seconds past the Unix epoch.
    #[serde(default)]
    pub mtime: u64,

    /// Fractional nanoseconds for modification time.
    ///
    /// This is zero in indexes written prior to 0.6.2, but treating it as
    /// zero is harmless - around the transition files will be seen as
    /// potentially touched.
    ///
    /// It seems moderately common that the nanos are zero, probably because
    /// the time was set by something that didn't preserve them. In that case,
    /// skip serializing.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub mtime_nanos: u32,

    /// For stored files, the blocks holding the file contents.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<blockdir::Address>,

    /// For symlinks only, the target of the symlink.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(a: &u32) -> bool {
    *a == 0
}

impl Entry for IndexEntry {
    /// Return apath relative to the top of the tree.
    fn apath(&self) -> &Apath {
        &self.apath
    }

    #[inline]
    fn kind(&self) -> Kind {
        self.kind
    }

    #[inline]
    fn mtime(&self) -> SystemTime {
        UNIX_EPOCH + Duration::new(self.mtime, self.mtime_nanos)
    }

    /// Size of the file, if it is a file. None for directories and symlinks.
    fn size(&self) -> Option<u64> {
        Some(self.addrs.iter().map(|a| a.len).sum())
    }

    /// Target of the symlink, if this is a symlink.
    #[inline]
    fn symlink_target(&self) -> &Option<String> {
        &self.target
    }
}

impl IndexEntry {
    /// Copy the metadata, but not the body content, from another entry.
    pub(crate) fn metadata_from<E: Entry>(source: &E) -> IndexEntry {
        let mtime_unix: Duration = source
            .mtime()
            .duration_since(UNIX_EPOCH)
            .expect("Failed to convert to unix time");
        assert_eq!(
            source.symlink_target().is_some(),
            source.kind() == Kind::Symlink
        );
        IndexEntry {
            apath: source.apath().clone(),
            kind: source.kind(),
            addrs: Vec::new(),
            target: source.symlink_target().clone(),
            mtime: mtime_unix.as_secs(),
            mtime_nanos: mtime_unix.subsec_nanos(),
        }
    }
}

/// Accumulates ordered changes to the index and streams them out to index files.
#[derive(Debug)]
pub struct IndexBuilder {
    /// The `i` directory within the band where all files for this index are written.
    dir: PathBuf,

    /// Currently queued entries to be written out.
    entries: Vec<IndexEntry>,

    /// Index hunk number, starting at 0.
    sequence: u32,

    /// The last-added filename, to enforce ordering.  At the start of the first hunk
    /// this is empty; at the start of a later hunk it's the last path from the previous
    /// hunk, and otherwise it's the last path from `entries`.
    check_order: apath::CheckOrder,
}

/// Accumulate and write out index entries into files in an index directory.
impl IndexBuilder {
    /// Make a new builder that will write files into the given directory.
    pub fn new(dir: &Path) -> IndexBuilder {
        IndexBuilder {
            dir: dir.to_path_buf(),
            entries: Vec::<IndexEntry>::with_capacity(MAX_ENTRIES_PER_HUNK),
            sequence: 0,
            check_order: apath::CheckOrder::new(),
        }
    }

    /// Append an entry to the index.
    ///
    /// The new entry must sort after everything already written to the index.
    pub fn push(&mut self, entry: IndexEntry) {
        // We do this check here rather than the Index constructor so that we
        // can still read invalid apaths...
        self.check_order.check(&entry.apath);
        self.entries.push(entry);
    }

    pub fn maybe_flush(&mut self, report: &Report) -> Result<()> {
        if self.entries.len() >= MAX_ENTRIES_PER_HUNK {
            self.finish_hunk(report)
        } else {
            Ok(())
        }
    }

    /// Finish this hunk of the index.
    ///
    /// This writes all the currently queued entries into a new index file
    /// in the band directory, and then clears the index to start receiving
    /// entries for the next hunk.
    pub fn finish_hunk(&mut self, report: &Report) -> Result<()> {
        let path = &path_for_hunk(&self.dir, self.sequence);

        ensure_dir_exists(&subdir_for_hunk(&self.dir, self.sequence))
            .context(errors::WriteIndex { path })?;

        let json = serde_json::to_vec(&self.entries).context(errors::SerializeJson { path })?;
        let uncompressed_len = json.len() as u64;
        let mut af = AtomicFile::new(path).context(errors::WriteIndex { path })?;
        let compressed_len =
            Snappy::compress_and_write(&json, &mut af).context(errors::WriteIndex { path })?;

        af.close(report).context(errors::WriteIndex { path })?;

        report.increment_size(
            "index",
            Sizes {
                uncompressed: uncompressed_len as u64,
                compressed: compressed_len as u64,
            },
        );
        report.increment("index.hunk", 1);

        // Ready for the next hunk.
        self.entries.clear();
        self.sequence += 1;
        Ok(())
    }
}

/// Return the subdirectory for a hunk numbered `hunk_number`.
fn subdir_for_hunk(dir: &Path, hunk_number: u32) -> PathBuf {
    let mut buf = dir.to_path_buf();
    buf.push(format!("{:05}", hunk_number / 10000));
    buf
}

/// Return the filename (in subdirectory) for a hunk.
fn path_for_hunk(dir: &Path, hunk_number: u32) -> PathBuf {
    let mut buf = subdir_for_hunk(dir, hunk_number);
    buf.push(format!("{:09}", hunk_number));
    buf
}

#[derive(Debug, Clone)]
pub struct ReadIndex {
    dir: PathBuf,
}

impl ReadIndex {
    pub fn new(dir: &Path) -> ReadIndex {
        ReadIndex {
            dir: dir.to_path_buf(),
        }
    }

    /// Return the (1-based) number of index hunks in an index directory.
    pub fn count_hunks(&self) -> Result<u32> {
        for i in 0.. {
            let path = path_for_hunk(&self.dir, i);
            if !file_exists(&path).context(errors::ReadIndex { path })? {
                // If hunk 1 is missing, 1 hunks exists.
                // TODO: Perhaps, list the directories and cope cleanly with
                // one hunk being missing.
                return Ok(i);
            }
        }
        unreachable!();
    }

    pub fn estimate_entry_count(&self) -> Result<u64> {
        Ok(u64::from(self.count_hunks()?) * (MAX_ENTRIES_PER_HUNK as u64))
    }

    /// Make an iterator that will return all entries in this band.
    pub fn iter(&self, report: &Report) -> Result<IndexEntryIter> {
        IndexEntryIter::open(&self.dir, report)
    }
}

/// Read out all the entries from a stored index, in apath order.
pub struct IndexEntryIter {
    /// The `i` directory within the band where all files for this index are written.
    dir: PathBuf,
    /// Temporarily buffered entries, read from the index files but not yet
    /// returned to the client.
    buffered_entries: Peekable<vec::IntoIter<IndexEntry>>,
    next_hunk_number: u32,
    pub report: Report,
    excludes: GlobSet,
}

impl fmt::Debug for IndexEntryIter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("IndexEntryIter")
            .field("dir", &self.dir)
            .field("next_hunk_number", &self.next_hunk_number)
            // .field("report", &self.report)
            // buffered_entries has no Debug itself
            .finish()
    }
}

impl Iterator for IndexEntryIter {
    type Item = IndexEntry;

    fn next(&mut self) -> Option<IndexEntry> {
        loop {
            while let Some(entry) = self.buffered_entries.next() {
                if !self.excludes.is_match(&entry.apath) {
                    return Some(entry);
                }
            }
            if !self.refill_entry_buffer_or_warn() {
                return None;
            }
        }
    }
}

impl IndexEntryIter {
    /// Create an iterator that will read all entires from an existing index.
    ///
    /// Prefer to use `Band::index_iter` instead.
    pub fn open(index_dir: &Path, report: &Report) -> Result<IndexEntryIter> {
        Ok(IndexEntryIter {
            dir: index_dir.to_path_buf(),
            buffered_entries: Vec::<IndexEntry>::new().into_iter().peekable(),
            next_hunk_number: 0,
            report: report.clone(),
            excludes: excludes::excludes_nothing(),
        })
    }

    /// Consume this iterator and return a new one with exclusions.
    pub fn with_excludes(self, excludes: globset::GlobSet) -> IndexEntryIter {
        IndexEntryIter { excludes, ..self }
    }

    /// Return the entry for given apath, if it is present, otherwise None.
    /// It follows this will also return None at the end of the index.
    ///
    /// After this is called, the iter has skipped forward to this apath,
    /// discarding entries for any earlier files. However, even if the apath
    /// is not present, other entries coming after it can still be read.
    pub fn advance_to(&mut self, apath: &Apath) -> Option<IndexEntry> {
        // This takes some care because we don't want to consume the entry
        // that tells us we went too far.
        loop {
            if let Some(cand) = self.buffered_entries.peek() {
                match cand.apath.cmp(apath) {
                    Ordering::Less => {
                        // Discard this and continue looking
                        self.buffered_entries.next().unwrap();
                    }
                    Ordering::Equal => {
                        return Some(self.buffered_entries.next().unwrap());
                    }
                    Ordering::Greater => {
                        // We passed the point where this entry would have been:
                        return None;
                    }
                }
            } else if !self.refill_entry_buffer_or_warn() {
                return None;
            }
        }
    }

    /// Refill entry buffer, converting errors to warnings.
    ///
    /// Returns true if a hunk was read; false at the end.
    fn refill_entry_buffer_or_warn(&mut self) -> bool {
        self.refill_entry_buffer().unwrap_or_else(|e| {
            self.report.show_error(&e); // Continue to read next hunk.
            true
        })
    }

    /// Read another hunk file and put it into buffered_entries.
    ///
    /// Returns true if another hunk could be found, otherwise false.
    fn refill_entry_buffer(&mut self) -> Result<bool> {
        assert!(
            self.buffered_entries.next().is_none(),
            "refill_entry_buffer called with non-empty buffer"
        );
        let path = &path_for_hunk(&self.dir, self.next_hunk_number);
        // Whether we succeed or fail, don't try to read this hunk again.
        self.next_hunk_number += 1;
        self.report.increment("index.hunk", 1);
        let mut f = match fs::File::open(&path) {
            Ok(f) => f,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                // TODO: Cope with one hunk being missing, while there are still
                // later-numbered hunks. This would require reading the whole
                // list of hunks first.
                return Ok(false);
            }
            Err(e) => return Err(e).with_context(|| errors::ReadIndex { path }),
        };
        let (comp_len, index_bytes) =
            Snappy::decompress_read(&mut f).with_context(|| errors::ReadIndex { path })?;
        self.report.increment_size(
            "index",
            Sizes {
                uncompressed: index_bytes.len() as u64,
                compressed: comp_len as u64,
            },
        );
        let entries: Vec<IndexEntry> = serde_json::from_slice(&index_bytes)
            .with_context(|| errors::DeserializeIndex { path })?;
        if entries.is_empty() {
            self.report
                .problem(&format!("Index hunk {:?} is empty", path));
        }
        // NOTE: Not updating 'skipped' counters; here. Questionable value.
        self.buffered_entries = entries.into_iter().peekable();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    pub fn scratch_indexbuilder() -> (TempDir, IndexBuilder, Report) {
        let testdir = TempDir::new().unwrap();
        let ib = IndexBuilder::new(testdir.path());
        (testdir, ib, Report::new())
    }

    pub fn add_an_entry(ib: &mut IndexBuilder, apath: &str) {
        ib.push(IndexEntry {
            apath: apath.into(),
            mtime: 1_461_736_377,
            mtime_nanos: 0,
            kind: Kind::File,
            addrs: vec![],
            target: None,
        });
    }

    #[test]
    fn serialize_index() {
        let entries = [IndexEntry {
            apath: "/a/b".into(),
            mtime: 1_461_736_377,
            mtime_nanos: 0,
            kind: Kind::File,
            addrs: vec![],
            target: None,
        }];
        let index_json = serde_json::to_string(&entries).unwrap();
        println!("{}", index_json);
        assert_eq!(
            index_json,
            "[{\"apath\":\"/a/b\",\
             \"kind\":\"File\",\
             \"mtime\":1461736377}]"
        );
    }

    #[test]
    #[should_panic]
    fn index_builder_checks_order() {
        let (_testdir, mut ib, _report) = scratch_indexbuilder();
        ib.push(IndexEntry {
            apath: "/zzz".into(),
            mtime: 1_461_736_377,
            mtime_nanos: 0,

            kind: Kind::File,
            addrs: vec![],
            target: None,
        });
        ib.push(IndexEntry {
            apath: "aaa".into(),
            mtime: 1_461_736_377,
            mtime_nanos: 0,
            kind: Kind::File,
            addrs: vec![],
            target: None,
        });
    }

    #[test]
    #[should_panic]
    fn index_builder_checks_names() {
        let (_testdir, mut ib, _report) = scratch_indexbuilder();
        ib.push(IndexEntry {
            apath: "../escapecat".into(),
            mtime: 1_461_736_377,
            kind: Kind::File,
            addrs: vec![],
            mtime_nanos: 0,
            target: None,
        })
    }

    #[test]
    fn path_for_hunk() {
        let index_dir = Path::new("/foo");
        let hunk_path = super::path_for_hunk(index_dir, 0);
        assert_eq!(file_name_as_str(&hunk_path), "000000000");
        assert_eq!(last_dir_name_as_str(&hunk_path), "00000");
    }

    fn file_name_as_str(p: &Path) -> &str {
        p.file_name().unwrap().to_str().unwrap()
    }

    fn last_dir_name_as_str(p: &Path) -> &str {
        p.parent().unwrap().file_name().unwrap().to_str().unwrap()
    }

    #[test]
    fn basic() {
        let (_testdir, mut ib, report) = scratch_indexbuilder();
        add_an_entry(&mut ib, "/apple");
        add_an_entry(&mut ib, "/banana");
        ib.finish_hunk(&report).unwrap();

        // The first hunk exists.
        let mut expected_path = ib.dir.to_path_buf();
        expected_path.push("00000");
        expected_path.push("000000000");

        let mut it = IndexEntryIter::open(&ib.dir, &report).unwrap();
        let entry = it.next().expect("Get first entry");
        assert_eq!(&entry.apath, "/apple");
        let entry = it.next().expect("Get second entry");
        assert_eq!(&entry.apath, "/banana");
        assert!(it.next().is_none(), "Expected no more entries");
    }

    #[test]
    fn multiple_hunks() {
        let (_testdir, mut ib, report) = scratch_indexbuilder();
        add_an_entry(&mut ib, "/1.1");
        add_an_entry(&mut ib, "/1.2");
        ib.finish_hunk(&report).unwrap();

        add_an_entry(&mut ib, "/2.1");
        add_an_entry(&mut ib, "/2.2");
        ib.finish_hunk(&report).unwrap();

        let it = IndexEntryIter::open(&ib.dir, &report).unwrap();
        assert_eq!(
            format!("{:?}", &it),
            format!(
                "IndexEntryIter {{ dir: {:?}, next_hunk_number: 0 }}",
                ib.dir
            )
        );

        let names: Vec<String> = it.map(|x| x.apath.into()).collect();
        assert_eq!(names, &["/1.1", "/1.2", "/2.1", "/2.2"]);
    }

    #[test]
    #[should_panic]
    fn no_duplicate_paths() {
        let (_testdir, mut ib, mut _report) = scratch_indexbuilder();
        add_an_entry(&mut ib, "/hello");
        add_an_entry(&mut ib, "/hello");
    }

    #[test]
    #[should_panic]
    fn no_duplicate_paths_across_hunks() {
        let (_testdir, mut ib, report) = scratch_indexbuilder();
        add_an_entry(&mut ib, "/hello");
        ib.finish_hunk(&report).unwrap();

        // Try to add an identically-named file within the next hunk and it should error,
        // because the IndexBuilder remembers the last file name written.
        add_an_entry(&mut ib, "hello");
    }

    #[test]
    fn excluded_entries() {
        let (_testdir, mut ib, report) = scratch_indexbuilder();
        add_an_entry(&mut ib, "/bar");
        add_an_entry(&mut ib, "/foo");
        add_an_entry(&mut ib, "/foobar");
        ib.finish_hunk(&report).unwrap();

        let excludes = excludes::from_strings(&["/fo*"]).unwrap();
        let it = IndexEntryIter::open(&ib.dir, &report)
            .unwrap()
            .with_excludes(excludes);
        assert_eq!(
            format!("{:?}", &it),
            format!(
                "IndexEntryIter {{ dir: {:?}, next_hunk_number: 0 }}",
                ib.dir
            )
        );

        let names: Vec<String> = it.map(|x| x.apath.into()).collect();
        assert_eq!(names, &["/bar"]);
    }

    #[test]
    fn advance() {
        let (_testdir, mut ib, report) = scratch_indexbuilder();
        add_an_entry(&mut ib, "/bar");
        add_an_entry(&mut ib, "/foo");
        add_an_entry(&mut ib, "/foobar");
        ib.finish_hunk(&report).unwrap();

        // Make multiple hunks to test traversal across hunks.
        add_an_entry(&mut ib, "/g01");
        add_an_entry(&mut ib, "/g02");
        add_an_entry(&mut ib, "/g03");
        ib.finish_hunk(&report).unwrap();

        // Advance to /foo and read on from there.
        let mut it = IndexEntryIter::open(&ib.dir, &report).unwrap();
        assert_eq!(it.advance_to(&Apath::from("/foo")).unwrap().apath, "/foo");
        assert_eq!(it.next().unwrap().apath, "/foobar");
        assert_eq!(it.next().unwrap().apath, "/g01");

        // Advance to before /g01
        let mut it = IndexEntryIter::open(&ib.dir, &report).unwrap();
        assert_eq!(it.advance_to(&Apath::from("/fxxx")), None);
        assert_eq!(it.next().unwrap().apath, "/g01");
        assert_eq!(it.next().unwrap().apath, "/g02");

        // Advance to before the first entry
        let mut it = IndexEntryIter::open(&ib.dir, &report).unwrap();
        assert_eq!(it.advance_to(&Apath::from("/aaaa")), None);
        assert_eq!(it.next().unwrap().apath, "/bar");
        assert_eq!(it.next().unwrap().apath, "/foo");

        // Advance to after the last entry
        let mut it = IndexEntryIter::open(&ib.dir, &report).unwrap();
        assert_eq!(it.advance_to(&Apath::from("/zz")), None);
        assert_eq!(it.next(), None);
    }
}
