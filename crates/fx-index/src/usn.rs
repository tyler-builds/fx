//! USN journal tailing: keeps a built index synchronized with the volume.
//!
//! NTFS logs every create/delete/rename into the change journal. The tailer
//! thread starts at the index's recorded position (`next_usn`) — so its
//! first pass naturally replays everything that happened since the index
//! was built or saved (the "catch-up") — then polls once a second for new
//! records, applying them to the shared index under a brief write lock.
//! Requires elevation, like everything else touching the volume handle.

use crate::mft::{open_volume, query_journal, Handle};
use crate::FileIndex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_RECORD_V2,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// Only these reasons change what the index stores (a rename's NEW_NAME
/// record carries both the new name and the new parent, so moves are
/// covered too).
const REASON_MASK: u32 =
    USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME;

const READ_BUF_LEN: usize = 1 << 18; // 256 KB of records per read
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub enum TailMsg {
    /// One batch of journal records was applied to the index.
    Applied { count: usize },
    /// The tailer stopped for a reason worth surfacing (journal wrapped,
    /// id changed, volume unreadable). A rebuild is the way back.
    Stopped(String),
}

/// Start tailing the volume's journal into `index` on a worker thread.
/// Returns the notification channel and a stop flag (set it, and the
/// thread winds down within one poll interval).
pub fn spawn_tail(
    index: Arc<RwLock<FileIndex>>,
    letter: char,
) -> (Receiver<TailMsg>, Arc<AtomicBool>) {
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_out = stop.clone();
    std::thread::Builder::new()
        .name("fx-usn-tail".into())
        .spawn(move || tail_loop(index, letter, &tx, &stop))
        .expect("spawn fx-usn-tail thread");
    (rx, stop_out)
}

fn tail_loop(index: Arc<RwLock<FileIndex>>, letter: char, tx: &Sender<TailMsg>, stop: &AtomicBool) {
    let volume = match open_volume(letter) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(TailMsg::Stopped(format!("cannot open volume (error {e})")));
            return;
        }
    };
    let (expect_id, start_usn) = {
        let idx = index.read().unwrap();
        (idx.journal_id, idx.next_usn)
    };
    match query_journal(&volume) {
        Some((jid, _)) if jid == expect_id => {}
        Some(_) => {
            let _ = tx.send(TailMsg::Stopped(
                "journal was reset since the index was built; Rebuild".into(),
            ));
            return;
        }
        None => {
            let _ = tx.send(TailMsg::Stopped("journal not readable".into()));
            return;
        }
    }

    let mut buf = vec![0u64; READ_BUF_LEN / 8];
    let mut usn = start_usn;
    while !stop.load(Ordering::Relaxed) {
        match read_and_apply(&volume, expect_id, usn, &mut buf, &index) {
            Ok((next, applied)) => {
                if applied > 0 {
                    index.write().unwrap().next_usn = next;
                    if tx.send(TailMsg::Applied { count: applied }).is_err() {
                        return; // app dropped the receiver
                    }
                } else if next != usn {
                    // Records existed but none we care about; still advance.
                    index.write().unwrap().next_usn = next;
                }
                usn = next;
            }
            Err(e) => {
                let _ = tx.send(TailMsg::Stopped(e));
                return;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One FSCTL_READ_USN_JOURNAL drain from `usn`: returns (new position,
/// records applied). Reads repeatedly until the journal has nothing more.
fn read_and_apply(
    volume: &Handle,
    journal_id: u64,
    mut usn: i64,
    buf: &mut [u64],
    index: &RwLock<FileIndex>,
) -> Result<(i64, usize), String> {
    const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;
    let mut applied = 0usize;
    loop {
        let read_data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: usn,
            ReasonMask: REASON_MASK,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };
        let mut bytes = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                volume.0,
                FSCTL_READ_USN_JOURNAL,
                &read_data as *const _ as *const c_void,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_JOURNAL_ENTRY_DELETED {
                // Our position aged out of the journal: the index has holes
                // we can't replay. Only a rebuild recovers.
                return Err("journal wrapped past our position; Rebuild".into());
            }
            return Err(format!("FSCTL_READ_USN_JOURNAL failed (error {err})"));
        }
        if (bytes as usize) < 8 {
            return Ok((usn, applied));
        }

        let next = buf[0] as i64;
        let base = buf.as_ptr() as *const u8;
        let mut offset = 8usize;
        // Apply the whole batch under one write lock.
        if bytes as usize > 8 {
            let mut idx = index.write().unwrap();
            let mut name_buf = String::with_capacity(256);
            while offset + std::mem::size_of::<USN_RECORD_V2>() - 2 <= bytes as usize {
                let rec = unsafe { &*(base.add(offset) as *const USN_RECORD_V2) };
                if rec.RecordLength == 0 {
                    break;
                }
                let units = unsafe {
                    let p = (rec as *const USN_RECORD_V2 as *const u8)
                        .add(rec.FileNameOffset as usize) as *const u16;
                    std::slice::from_raw_parts(p, rec.FileNameLength as usize / 2)
                };
                name_buf.clear();
                name_buf.extend(
                    char::decode_utf16(units.iter().copied())
                        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)),
                );
                if rec.Reason & USN_REASON_FILE_DELETE != 0 {
                    idx.remove(rec.FileReferenceNumber);
                    applied += 1;
                } else if rec.Reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) != 0 {
                    idx.upsert(
                        rec.FileReferenceNumber,
                        rec.ParentFileReferenceNumber,
                        &name_buf,
                        rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    );
                    applied += 1;
                }
                offset += rec.RecordLength as usize;
            }
        }
        if next == usn {
            return Ok((usn, applied)); // no forward progress; done
        }
        usn = next;
    }
}
