//! Cache audio only after a local proof. Reuse requires a fresh authenticated server ticket.
use sha2::{Digest, Sha256};
use std::{fs, io, path::{Path, PathBuf}, sync::Arc, time::{Duration, SystemTime}};
use crate::stream::Buf;

const MAX_BYTES: u64 = 128 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(7 * 86400);

pub(crate) fn digest(mut input: impl io::Read) -> io::Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut input, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn path(session: &str, track: &str, original: &str, candidate: &str) -> PathBuf {
    let key = format!("restored-speech-v2\n{session}\n{track}\n{original}\n{candidate}");
    crate::config::cache_dir().join("verified-media").join(format!("{:x}.mp3", Sha256::digest(key)))
}

pub(super) fn read(path: &Path, expected: &str) -> Option<Arc<Buf>> {
    let file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.modified().ok()?.elapsed().ok()? > TTL || !(1..=32*1024*1024).contains(&metadata.len()) {
        return None;
    }
    if digest(&file).ok()? != expected { return None; }
    Some(Buf::from_complete_file(file, metadata.len()))
}

pub(super) fn store(path: &Path, buf: &Arc<Buf>) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or_else(|| io::Error::other("cache path"))?;
    fs::create_dir_all(dir)?;
    let temp = path.with_extension(format!("{}.{}.tmp",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed)));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let saved = io::copy(&mut buf.reader(), &mut file);
    drop(file);
    let result = saved.and_then(|_| fs::rename(&temp,path));
    if result.is_err() { let _ = fs::remove_file(&temp); }
    prune(dir);
    result
}

fn prune(dir: &Path) {
    let Ok(entries) = dir.read_dir() else { return };
    let mut entries: Vec<_> = entries.flatten().filter_map(|e| {
        let m = e.metadata().ok()?;
        Some((m.modified().ok()?,m.len(),e.path()))
    }).collect();
    entries.sort_unstable_by_key(|item| std::cmp::Reverse(item.0));
    let mut bytes = 0;
    for (modified, size, path) in entries {
        let age = SystemTime::now().duration_since(modified).unwrap_or_default();
        if path.extension().is_some_and(|ext| ext == "tmp") {
            if age > TTL { let _ = fs::remove_file(path); }
            continue;
        }
        bytes += size;
        if bytes > MAX_BYTES || age > TTL { let _ = fs::remove_file(path); }
    }
}
