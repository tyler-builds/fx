//! MFT fast path: enumerate every file record on an NTFS volume through
//! `FSCTL_ENUM_USN_DATA`. One sequential pass over the master file table —
//! no directory traversal, no per-file syscalls — which is how Everything
//! indexes millions of files in seconds. Requires an elevated process
//! (opening `\\.\C:` raw is denied otherwise); callers fall back to the
//! directory walk when this returns Err.

use crate::{send_progress, FileIndex, IndexBuilder, IndexMsg};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::Sender;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, MFT_ENUM_DATA_V0, USN_JOURNAL_DATA_V0,
    USN_RECORD_V2,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const GENERIC_READ: u32 = 0x8000_0000;
const ENUM_BUF_LEN: usize = 1 << 20; // 1 MB of records per DeviceIoControl round-trip

/// RAII so early returns can't leak the volume handle.
pub(crate) struct Handle(pub(crate) HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
// A HANDLE is just a kernel object reference; nothing thread-affine about
// the volume handles we open. Needed so the USN tailer thread can own one.
unsafe impl Send for Handle {}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Open a volume for ioctl access (`\\.\C:`). Requires elevation.
pub(crate) fn open_volume(letter: char) -> Result<Handle, u32> {
    let h = unsafe {
        CreateFileW(
            wide(&format!("\\\\.\\{letter}:")).as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

/// Cheap probe: can this process open the volume raw (i.e. is the MFT fast
/// path available)? True only in elevated processes.
pub fn volume_openable(letter: char) -> bool {
    open_volume(letter).is_ok()
}

pub fn build(root: &Path, tx: &Sender<IndexMsg>) -> Result<FileIndex, String> {
    // "C:\..." -> drive letter. Only plain drive-letter volumes for now.
    let root_str = root.to_string_lossy();
    let letter = root_str
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic() && root_str.get(1..2) == Some(":"))
        .ok_or_else(|| format!("{root_str}: not a drive-letter path"))?;
    let volume_root = format!("{letter}:\\");

    let volume = open_volume(letter)
        .map_err(|e| format!("cannot open volume {letter}: (error {e}, likely needs elevation)"))?;

    // The enumeration range must end at the journal's current position; if
    // the journal is inactive, fall back to "everything". The (id, usn)
    // pair is also stamped on the index so a later reload can detect drift.
    let journal = query_journal(&volume);
    let (journal_id, next_usn) = journal.unwrap_or((0, 0));
    let high_usn = journal.map_or(i64::MAX, |(_, next)| next);
    let mut bytes = 0u32;

    let root_id = root_frn(&volume_root)?;

    let mut builder = IndexBuilder::with_capacity(1 << 22);
    // Reused per-record decode buffer: the arena copies out of it, so the
    // whole scan does no per-record allocation.
    let mut name_buf = String::with_capacity(256);
    // u64-backed buffer so USN records (8-byte aligned) can be read in place.
    let mut buf = vec![0u64; ENUM_BUF_LEN / 8];
    let mut med = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: high_usn,
    };

    loop {
        let ok = unsafe {
            DeviceIoControl(
                volume.0,
                FSCTL_ENUM_USN_DATA,
                &med as *const _ as *const c_void,
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                ENUM_BUF_LEN as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF {
                break;
            }
            return Err(format!("FSCTL_ENUM_USN_DATA failed (error {err})"));
        }
        if (bytes as usize) < 8 {
            break;
        }

        // Payload: next start FRN (u64), then a run of USN_RECORD_V2.
        med.StartFileReferenceNumber = buf[0];
        let base = buf.as_ptr() as *const u8;
        let mut offset = 8usize;
        while offset + std::mem::size_of::<USN_RECORD_V2>() - 2 <= bytes as usize {
            // SAFETY: the kernel guarantees records are 8-byte aligned and
            // RecordLength-delimited within the returned byte count.
            let rec = unsafe { &*(base.add(offset) as *const USN_RECORD_V2) };
            if rec.RecordLength == 0 {
                break;
            }
            let units = unsafe {
                let p = (rec as *const USN_RECORD_V2 as *const u8).add(rec.FileNameOffset as usize)
                    as *const u16;
                std::slice::from_raw_parts(p, rec.FileNameLength as usize / 2)
            };
            name_buf.clear();
            name_buf.extend(
                char::decode_utf16(units.iter().copied())
                    .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)),
            );
            builder.push(
                &name_buf,
                rec.FileReferenceNumber,
                rec.ParentFileReferenceNumber,
                rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            );
            offset += rec.RecordLength as usize;
        }
        if builder.len() % (1 << 18) < 4096 {
            send_progress(tx, builder.len());
        }
    }

    Ok(builder
        .finish(volume_root.into(), root_id)
        .with_journal(journal_id, next_usn))
}

pub(crate) fn query_journal(volume: &Handle) -> Option<(u64, i64)> {
    let mut journal = unsafe { std::mem::zeroed::<USN_JOURNAL_DATA_V0>() };
    let mut bytes = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            volume.0,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut journal as *mut _ as *mut c_void,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut bytes,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some((journal.UsnJournalID, journal.NextUsn))
}

/// Current journal (id, next usn) for a volume; None when the volume can't
/// be opened (not elevated) or the journal is inactive.
pub fn journal_position(letter: char) -> Option<(u64, i64)> {
    query_journal(&open_volume(letter).ok()?)
}

/// File reference number of the volume root, so path resolution knows where
/// to stop climbing.
fn root_frn(volume_root: &str) -> Result<u64, String> {
    let handle = unsafe {
        Handle(CreateFileW(
            wide(volume_root).as_ptr(),
            0, // attributes only
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS, // required to open a directory
            std::ptr::null_mut(),
        ))
    };
    if handle.0 == INVALID_HANDLE_VALUE {
        return Err(format!("cannot open {volume_root} (error {})", unsafe {
            GetLastError()
        }));
    }
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(format!(
            "GetFileInformationByHandle failed (error {})",
            unsafe { GetLastError() }
        ));
    }
    Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}
