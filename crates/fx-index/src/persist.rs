//! Index persistence (format FXIDX002): the column arrays are written
//! through the IndexBuilder push API on load, which also compacts away any
//! tombstones accumulated from journal tailing.
//!
//! Layout, all little-endian:
//!   magic            8 bytes  "FXIDX002"
//!   root             u16 len + utf8 bytes
//!   root_id          u64
//!   journal_id       u64      (0 = unknown)
//!   next_usn         i64      (0 = unknown)
//!   count            u64      (live entries)
//!   entries          count of:
//!     name           u16 len + utf8 bytes
//!     id             u64
//!     parent         u64
//!     flags          u8       bit0 = is_dir

use crate::{FileIndex, IndexBuilder, FLAG_DIR};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"FXIDX002";

/// Default on-disk location for a volume's index, e.g.
/// `%LOCALAPPDATA%\fx-spike\index-C.bin`.
pub fn default_index_path(root: &Path) -> PathBuf {
    let letter = root
        .to_string_lossy()
        .chars()
        .next()
        .filter(char::is_ascii_alphabetic)
        .unwrap_or('X');
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("fx-spike").join(format!("index-{letter}.bin"))
}

pub fn save(index: &FileIndex, path: &Path) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    // Write to a sibling temp file then rename, so a crash mid-write can't
    // leave a truncated index that fails to load next launch.
    let tmp = path.with_extension("bin.tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp).map_err(err)?);
        w.write_all(MAGIC).map_err(err)?;
        let root = index.root.to_string_lossy();
        w.write_all(&(root.len() as u16).to_le_bytes())
            .map_err(err)?;
        w.write_all(root.as_bytes()).map_err(err)?;
        w.write_all(&index.root_id.to_le_bytes()).map_err(err)?;
        w.write_all(&index.journal_id.to_le_bytes()).map_err(err)?;
        w.write_all(&index.next_usn.to_le_bytes()).map_err(err)?;
        w.write_all(&(index.len() as u64).to_le_bytes())
            .map_err(err)?;
        for i in 0..index.raw_len() as u32 {
            if index.is_deleted(i) {
                continue; // compact tombstones out
            }
            let name = index.name(i);
            w.write_all(&(name.len() as u16).to_le_bytes())
                .map_err(err)?;
            w.write_all(name.as_bytes()).map_err(err)?;
            w.write_all(&index.entry_id(i).to_le_bytes()).map_err(err)?;
            w.write_all(&index.parent_id(i).to_le_bytes())
                .map_err(err)?;
            w.write_all(&[if index.is_dir(i) { FLAG_DIR } else { 0 }])
                .map_err(err)?;
        }
        w.flush().map_err(err)?;
    }
    std::fs::rename(&tmp, path).map_err(err)
}

pub fn load(path: &Path) -> Result<FileIndex, String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut r = BufReader::with_capacity(1 << 20, std::fs::File::open(path).map_err(err)?);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(err)?;
    if &magic != MAGIC {
        return Err(format!(
            "{}: not an fx index v2 (bad magic)",
            path.display()
        ));
    }
    let root_len = read_u16(&mut r).map_err(err)? as usize;
    let mut root = vec![0u8; root_len];
    r.read_exact(&mut root).map_err(err)?;
    let root = PathBuf::from(String::from_utf8_lossy(&root).into_owned());
    let root_id = read_u64(&mut r).map_err(err)?;
    let journal_id = read_u64(&mut r).map_err(err)?;
    let next_usn = read_u64(&mut r).map_err(err)? as i64;
    let count = read_u64(&mut r).map_err(err)? as usize;
    // Sanity cap: a corrupt count must not trigger a huge allocation.
    if count > 100_000_000 {
        return Err(format!(
            "{}: implausible entry count {count}",
            path.display()
        ));
    }

    let mut builder = IndexBuilder::with_capacity(count);
    let mut name_buf = vec![0u8; u16::MAX as usize];
    for _ in 0..count {
        let name_len = read_u16(&mut r).map_err(err)? as usize;
        r.read_exact(&mut name_buf[..name_len]).map_err(err)?;
        let id = read_u64(&mut r).map_err(err)?;
        let parent = read_u64(&mut r).map_err(err)?;
        let mut flags = [0u8; 1];
        r.read_exact(&mut flags).map_err(err)?;
        builder.push(
            &String::from_utf8_lossy(&name_buf[..name_len]),
            id,
            parent,
            flags[0] & FLAG_DIR != 0,
        );
    }

    Ok(builder
        .finish(root, root_id)
        .with_journal(journal_id, next_usn))
}

fn read_u16(r: &mut impl Read) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_everything() {
        let mut b = IndexBuilder::with_capacity(3);
        b.push("docs", 10, 1, true);
        b.push("R\u{e9}sum\u{e9}.pdf", 11, 10, false);
        b.push("readme.md", 12, 1, false);
        let mut index = b.finish(PathBuf::from("C:\\"), 1).with_journal(77, 999);
        // Journal-style mutations before saving: a delete (must compact
        // away) and a rename.
        index.remove(12);
        index.upsert(11, 10, "CV.pdf", false);

        let path = std::env::temp_dir().join("fx-persist-test.bin");
        save(&index, &path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(loaded.root, index.root);
        assert_eq!(loaded.root_id, 1);
        assert_eq!(loaded.journal_id, 77);
        assert_eq!(loaded.next_usn, 999);
        assert_eq!(loaded.len(), 2); // readme.md compacted out
        assert_eq!(loaded.raw_len(), 2);
        assert_eq!(loaded.search("readme", 10).total, 0);
        let hit = loaded.search("cv.pdf", 10).hits[0];
        assert_eq!(loaded.resolve_path(hit), PathBuf::from("C:\\docs\\CV.pdf"));
        // Mutation still works on a loaded index (lookup structures intact).
        let mut loaded = loaded;
        loaded.upsert(50, 10, "new.txt", false);
        assert_eq!(
            loaded.resolve_path(loaded.lookup(50).unwrap()),
            PathBuf::from("C:\\docs\\new.txt")
        );
    }

    #[test]
    fn load_rejects_garbage() {
        let path = std::env::temp_dir().join("fx-persist-garbage.bin");
        std::fs::write(&path, b"not an index at all").unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
