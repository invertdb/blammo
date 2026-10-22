use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Frame header: length (u32) and CRC-32 of the record (u32), little-endian.
const FRAME: usize = 8;

struct Current {
    segment: u64,
    file: File,
    length: u64,
}

/// What a log's own bookkeeping (rotations and checkpoints) survives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// A crash of the machine: new segments, rotated-out segments and checkpoints are synced (and so is the
    /// directory) before they count. For a layer that syncs its records.
    #[default]
    Full,
    /// A crash of the process: the same steps, written atomically but not synced (the operating system keeps what
    /// was written). For a layer whose records aren't synced either, so syncing the bookkeeping buys nothing.
    Process,
}

/// A write-ahead log in a directory of its own.
pub struct Log {
    directory: PathBuf,
    durability: Durability,
    current: Mutex<Current>,
    /// Held while rotating, so rotations don't interleave.
    rotating: Mutex<()>,
    /// The latest checkpoint's segment (0 if none), and its bytes.
    checkpoint: Mutex<(u64, Option<Vec<u8>>)>,
}

fn segment_path(directory: &Path, id: u64) -> PathBuf {
    directory.join(format!("{id:016x}.log"))
}

fn checkpoint_path(directory: &Path, id: u64) -> PathBuf {
    directory.join(format!("{id:016x}.checkpoint"))
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn invalid(message: String) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

/// Segment and checkpoint ids in a directory, each sorted.
fn listing(directory: &Path) -> io::Result<(Vec<u64>, Vec<u64>)> {
    let (mut segments, mut checkpoints) = (Vec::new(), Vec::new());
    for entry in fs::read_dir(directory)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let parse = |stem: &str| u64::from_str_radix(stem, 16).ok().filter(|_| stem.len() == 16);
        if let Some(id) = name.strip_suffix(".log").and_then(parse) {
            segments.push(id);
        } else if let Some(id) = name.strip_suffix(".checkpoint").and_then(parse) {
            checkpoints.push(id);
        }
    }
    segments.sort_unstable();
    checkpoints.sort_unstable();
    Ok((segments, checkpoints))
}

/// The length of a segment's valid frames, and whether anything after them is damaged.
fn valid_length(bytes: &[u8]) -> (u64, bool) {
    let mut at = 0usize;
    while at + FRAME <= bytes.len() {
        let length = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
        let Some(record) = bytes.get(at + FRAME..at + FRAME + length) else { break };
        if crc32fast::hash(record) != crc {
            break;
        }
        at += FRAME + length;
    }
    (at as u64, at < bytes.len())
}

fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path)
}

impl Log {
    /// Opens a log, creating it if need be. A torn tail on the last segment is dropped; damage anywhere else is an
    /// error. Segments and checkpoints older than the latest checkpoint are deleted.
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Log> {
        Log::open_with(directory, Durability::Full)
    }

    /// Opens a log with a [`Durability`] for its rotations and checkpoints.
    pub fn open_with(directory: impl AsRef<Path>, durability: Durability) -> io::Result<Log> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;
        for entry in fs::read_dir(&directory)?.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".new") {
                fs::remove_file(entry.path())?; // a checkpoint a crash interrupted
            }
        }
        let (segments, checkpoints) = listing(&directory)?;
        let checkpoint = checkpoints.last().copied();
        let from = checkpoint.unwrap_or(0);
        for &id in segments.iter().filter(|&&id| id < from) {
            fs::remove_file(segment_path(&directory, id))?;
        }
        for &id in checkpoints.iter().filter(|&&id| id < from) {
            fs::remove_file(checkpoint_path(&directory, id))?;
        }
        let live: Vec<u64> = segments.into_iter().filter(|&id| id >= from).collect();
        for (n, &id) in live.iter().enumerate() {
            let path = segment_path(&directory, id);
            let (length, damaged) = valid_length(&read_file(&path)?);
            if damaged {
                if n + 1 < live.len() {
                    return Err(invalid(format!("{}: damaged before its end, and later segments exist", path.display())));
                }
                OpenOptions::new().write(true).open(&path)?.set_len(length)?;
            }
        }
        let segment = live.last().copied().unwrap_or(from);
        let path = segment_path(&directory, segment);
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
        let length = file.metadata()?.len();
        if live.is_empty() && durability == Durability::Full {
            sync_directory(&directory)?;
        }
        let data = match checkpoint {
            Some(id) => Some(read_file(&checkpoint_path(&directory, id))?),
            None => None,
        };
        Ok(Log { directory, durability, current: Mutex::new(Current { segment, file, length }), rotating: Mutex::new(()), checkpoint: Mutex::new((from, data)) })
    }

    /// The latest checkpoint's bytes, or None if there's never been one.
    pub fn checkpoint(&self) -> Option<Vec<u8>> {
        self.checkpoint.lock().unwrap().1.clone()
    }

    /// Calls `apply` with every record since the latest checkpoint, in order.
    pub fn replay(&self, mut apply: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<u64> {
        let from = self.checkpoint.lock().unwrap().0;
        let (segments, _) = listing(&self.directory)?;
        let mut count = 0;
        for id in segments.into_iter().filter(|&id| id >= from) {
            let file = File::open(segment_path(&self.directory, id))?;
            if file.metadata()?.len() == 0 {
                continue;
            }
            let map = unsafe { memmap2::Mmap::map(&file)? };
            let mut at = 0usize;
            while at + FRAME <= map.len() {
                let length = u32::from_le_bytes(map[at..at + 4].try_into().unwrap()) as usize;
                let Some(record) = map.get(at + FRAME..at + FRAME + length) else { break };
                apply(record)?;
                count += 1;
                at += FRAME + length;
            }
        }
        Ok(count)
    }

    /// Appends one record. It's durable after [`Log::sync`].
    pub fn append(&self, record: &[u8]) -> io::Result<()> {
        self.append_all([record])
    }

    /// Appends records with one write. They're durable after [`Log::sync`].
    pub fn append_all<'a>(&self, records: impl IntoIterator<Item = &'a [u8]>) -> io::Result<()> {
        let mut bytes = Vec::new();
        for record in records {
            let length = u32::try_from(record.len()).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "a record of 4 GB or more"))?;
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(&crc32fast::hash(record).to_le_bytes());
            bytes.extend_from_slice(record);
        }
        let mut current = self.current.lock().unwrap();
        let at = current.length;
        current.file.write_all_at(&bytes, at)?;
        current.length += bytes.len() as u64;
        Ok(())
    }

    /// Makes every record appended so far durable.
    pub fn sync(&self) -> io::Result<()> {
        let file = self.current.lock().unwrap().file.try_clone()?;
        file.sync_data()
    }

    /// Bytes in the current segment.
    pub fn segment_length(&self) -> u64 {
        self.current.lock().unwrap().length
    }

    /// Bytes in every segment since the latest checkpoint.
    pub fn len(&self) -> io::Result<u64> {
        let from = self.checkpoint.lock().unwrap().0;
        let (segments, _) = listing(&self.directory)?;
        let mut total = 0;
        for id in segments.into_iter().filter(|&id| id >= from) {
            total += fs::metadata(segment_path(&self.directory, id))?.len();
        }
        Ok(total)
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Syncs the current segment and starts a new one; every later record goes there. Returns the new segment's id,
    /// for [`Log::write_checkpoint`].
    ///
    /// Appends wait only for the switch itself: the new segment is created (and made durable) before it, and the
    /// previous one is synced after.
    pub fn rotate(&self) -> io::Result<u64> {
        let _one = self.rotating.lock().unwrap();
        let segment = self.current.lock().unwrap().segment + 1;
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(segment_path(&self.directory, segment))?;
        if self.durability == Durability::Full {
            sync_directory(&self.directory)?;
        }
        let previous = std::mem::replace(&mut *self.current.lock().unwrap(), Current { segment, file, length: 0 });
        if self.durability == Durability::Full {
            previous.file.sync_data()?;
        }
        Ok(segment)
    }

    /// Records `state` as what holds before segment `segment` (from [`Log::rotate`]), then deletes the earlier
    /// segments and checkpoints.
    pub fn write_checkpoint(&self, segment: u64, state: &[u8]) -> io::Result<()> {
        if segment > self.current.lock().unwrap().segment {
            return Err(io::Error::new(ErrorKind::InvalidInput, format!("segment {segment} hasn't been started")));
        }
        let mut checkpoint = self.checkpoint.lock().unwrap();
        if segment < checkpoint.0 {
            return Err(io::Error::new(ErrorKind::InvalidInput, format!("a checkpoint for segment {} is newer", checkpoint.0)));
        }
        let path = checkpoint_path(&self.directory, segment);
        let temporary = path.with_extension("checkpoint.new");
        let mut file = File::create(&temporary)?;
        file.write_all(state)?;
        if self.durability == Durability::Full {
            file.sync_all()?;
        }
        fs::rename(&temporary, &path)?;
        if self.durability == Durability::Full {
            sync_directory(&self.directory)?;
        }
        *checkpoint = (segment, Some(state.to_vec()));
        let (segments, checkpoints) = listing(&self.directory)?;
        for id in segments.into_iter().filter(|&id| id < segment) {
            fs::remove_file(segment_path(&self.directory, id))?;
        }
        for id in checkpoints.into_iter().filter(|&id| id < segment) {
            fs::remove_file(checkpoint_path(&self.directory, id))?;
        }
        Ok(())
    }

    /// Starts over from `state`: rotates, and checkpoints the new segment with it. For a layer that has just made
    /// everything logged durable (a commit), so only `state` (if anything) needs to survive.
    pub fn reset(&self, state: &[u8]) -> io::Result<()> {
        let segment = self.rotate()?;
        self.write_checkpoint(segment, state)
    }
}
