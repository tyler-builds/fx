//! Shell integration: open files, host the REAL Explorer context menu
//! (third-party extensions included), invoke shell verbs (copy/cut/paste —
//! which is Explorer clipboard interop for free), and Recycle Bin deletes
//! via IFileOperation.
//!
//! Everything runs on one dedicated STA worker thread that owns a hidden
//! window. The window matters: `TrackPopupMenuEx` needs an owner whose
//! wndproc forwards menu messages to IContextMenu2/3, or dynamic submenus
//! ("Send To", "Open With", icons) come up empty — the classic mistake of
//! context-menu hosts. Blocking that thread while a menu is open leaves
//! the UI thread untouched.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

pub enum ShellRequest {
    /// Launch with the default association (double-click).
    Open(PathBuf),
    /// Show the native context menu for these items, at the mouse cursor.
    ContextMenu(Vec<PathBuf>),
    /// Invoke a canonical verb ("copy", "cut", "delete", ...) on items.
    InvokeVerb(Vec<PathBuf>, &'static str),
    /// Recycle Bin delete (undo-able), no confirmation UI.
    Recycle(Vec<PathBuf>),
    /// Paste the clipboard's files into a folder (honours cut = move).
    PasteInto(PathBuf),
    /// Copy items into a destination folder (drag-drop / paste-into).
    CopyInto {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// Move items into a destination folder.
    MoveInto {
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// Enumerate a folder's background context menu (for rendering in our
    /// own themed menu). Replies with `ShellEvent::BackgroundMenu`.
    EnumBackground(PathBuf),
    /// Invoke a command (by the id from a prior enumeration) on a folder's
    /// background menu.
    InvokeBackground { dir: PathBuf, id: u32 },
}

/// One entry in an enumerated shell context menu. Plain data so it can be
/// rendered by the UI in a themed menu and invoked later by `id`.
#[derive(Clone)]
pub enum MenuItem {
    Command {
        id: u32,
        label: String,
        /// Canonical verb (e.g. "paste"), for the UI to spot commands it
        /// wants to override with a native implementation.
        verb: Option<String>,
        enabled: bool,
    },
    Submenu {
        label: String,
        items: Vec<MenuItem>,
    },
    Separator,
}

pub enum ShellEvent {
    /// An operation ran that may have changed the filesystem; refresh.
    Changed,
    Error(String),
    /// Enumerated background menu for `dir` (reply to `EnumBackground`).
    BackgroundMenu {
        dir: PathBuf,
        items: Vec<MenuItem>,
    },
}

/// Start the shell worker. `on_event` fires after each event is queued.
pub fn spawn_shell_worker(
    on_event: impl Fn() + Send + 'static,
) -> (Sender<ShellRequest>, Receiver<ShellEvent>) {
    let (req_tx, req_rx) = channel::<ShellRequest>();
    let (ev_tx, ev_rx) = channel::<ShellEvent>();
    std::thread::Builder::new()
        .name("fx-shell".into())
        .spawn(move || worker(req_rx, ev_tx, on_event))
        .expect("spawn fx-shell thread");
    (req_tx, ev_rx)
}

#[cfg(not(windows))]
fn worker(rx: Receiver<ShellRequest>, tx: Sender<ShellEvent>, on_event: impl Fn()) {
    while let Ok(req) = rx.recv() {
        let ev = match req {
            ShellRequest::EnumBackground(dir) => ShellEvent::BackgroundMenu { dir, items: vec![] },
            _ => ShellEvent::Error("shell integration is Windows-only".into()),
        };
        let _ = tx.send(ev);
        on_event();
    }
}

#[cfg(windows)]
fn worker(rx: Receiver<ShellRequest>, tx: Sender<ShellEvent>, on_event: impl Fn()) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let hwnd = match win::create_owner_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = tx.send(ShellEvent::Error(e));
            return;
        }
    };
    while let Ok(req) = rx.recv() {
        // Enumeration and its invocation return their own events; everything
        // else maps to Changed/None/Error.
        let event = match req {
            ShellRequest::EnumBackground(dir) => match win::enum_background(hwnd, &dir) {
                Ok(items) => Some(ShellEvent::BackgroundMenu { dir, items }),
                Err(e) => Some(ShellEvent::Error(e)),
            },
            ShellRequest::InvokeBackground { dir, id } => {
                match win::invoke_background(hwnd, &dir, id) {
                    Ok(()) => Some(ShellEvent::Changed),
                    Err(e) => Some(ShellEvent::Error(e)),
                }
            }
            other => match win::handle(hwnd, other) {
                Ok(true) => Some(ShellEvent::Changed),
                Ok(false) => None,
                Err(e) => Some(ShellEvent::Error(e)),
            },
        };
        if let Some(ev) = event {
            if tx.send(ev).is_err() {
                return;
            }
            on_event();
        }
    }
}

#[cfg(windows)]
mod win {
    use super::MenuItem;
    use super::ShellRequest;
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::{Interface, PCSTR, PCWSTR, PSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_SFObject, FileOperation, IContextMenu, IContextMenu2, IContextMenu3, IFileOperation,
        IShellFolder, IShellItem, SHCreateItemFromParsingName, ShellExecuteW, CMF_NORMAL,
        CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, FOF_ALLOWUNDO,
        FOF_NOCONFIRMATION, FOF_SILENT, GCS_VERBW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, GetMenuItemInfoW, GetMenuStringW, MENUITEMINFOW, MFS_GRAYED,
        MFT_SEPARATOR, MF_BYPOSITION, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_SUBMENU,
    };

    thread_local! {
        /// Last-enumerated background menu, kept so a later click can invoke
        /// by command id against the same IContextMenu (ids stay valid on it).
        static BG_CACHE: RefCell<Option<(std::path::PathBuf, IContextMenu)>> =
            const { RefCell::new(None) };
    }

    // Not exported by the windows crate; documented value (shellapi.h).
    // Tells InvokeCommand to read the wide (lpVerbW/lpDirectoryW) fields.
    const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos,
        RegisterClassW, SetForegroundWindow, TrackPopupMenuEx, HMENU, SW_SHOWNORMAL, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
        WS_POPUP,
    };

    thread_local! {
        /// The menu being shown, for wndproc message forwarding.
        static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn handle(hwnd: HWND, req: ShellRequest) -> Result<bool, String> {
        match req {
            ShellRequest::Open(path) => {
                open(&path)?;
                Ok(false)
            }
            ShellRequest::ContextMenu(paths) => {
                let cm = items_context_menu(hwnd, &paths)?;
                let dir = paths[0].parent().unwrap_or(&paths[0]);
                show_menu(hwnd, &cm, dir)
            }
            ShellRequest::InvokeVerb(paths, verb) => {
                let cm = items_context_menu(hwnd, &paths)?;
                prime_menu(hwnd, &cm)?;
                let dir = paths[0].parent().unwrap_or(&paths[0]);
                invoke_verb(hwnd, &cm, verb, dir)?;
                Ok(verb != "copy")
            }
            ShellRequest::Recycle(paths) => {
                recycle(&paths)?;
                Ok(true)
            }
            ShellRequest::PasteInto(dir) => {
                paste_into(hwnd, &dir)?;
                Ok(true)
            }
            ShellRequest::CopyInto { sources, dest } => {
                transfer(&sources, &dest, false)?;
                Ok(true)
            }
            ShellRequest::MoveInto { sources, dest } => {
                transfer(&sources, &dest, true)?;
                Ok(true)
            }
            // Routed directly by the worker (they produce their own events).
            ShellRequest::EnumBackground(_) | ShellRequest::InvokeBackground { .. } => {
                unreachable!("background menu requests are handled in the worker loop")
            }
        }
    }

    /// Copy or move `sources` into the `dest` folder via IFileOperation, so
    /// large transfers get the standard progress + conflict UI and land in
    /// the undo history like a native Explorer operation.
    fn transfer(sources: &[std::path::PathBuf], dest: &Path, is_move: bool) -> Result<(), String> {
        if sources.is_empty() {
            return Ok(());
        }
        let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }
            .map_err(|e| format!("FileOperation: {e}"))?;
        unsafe {
            op.SetOperationFlags(FOF_ALLOWUNDO)
                .map_err(|e| e.to_string())?;
            let dest_item: IShellItem =
                SHCreateItemFromParsingName(PCWSTR(wide(dest).as_ptr()), None)
                    .map_err(|e| format!("{}: {e}", dest.display()))?;
            for src in sources {
                let item: IShellItem =
                    SHCreateItemFromParsingName(PCWSTR(wide(src).as_ptr()), None)
                        .map_err(|e| format!("{}: {e}", src.display()))?;
                if is_move {
                    op.MoveItem(&item, &dest_item, PCWSTR::null(), None)
                        .map_err(|e| e.to_string())?;
                } else {
                    op.CopyItem(&item, &dest_item, PCWSTR::null(), None)
                        .map_err(|e| e.to_string())?;
                }
            }
            op.PerformOperations()
                .map_err(|e| format!("{}: {e}", if is_move { "move" } else { "copy" }))?;
        }
        Ok(())
    }

    fn open(path: &Path) -> Result<(), String> {
        let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        let inst = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(wide(path).as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if inst.0 as usize <= 32 {
            return Err(format!("open failed for {}", path.display()));
        }
        Ok(())
    }

    /// IContextMenu for a set of items (all sharing one parent folder).
    fn items_context_menu(
        hwnd: HWND,
        paths: &[std::path::PathBuf],
    ) -> Result<IContextMenu, String> {
        let first = paths.first().ok_or("empty selection")?;
        let parent = first.parent().ok_or("item has no parent folder")?;
        let parent_item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(wide(parent).as_ptr()), None) }
                .map_err(|e| format!("{}: {e}", parent.display()))?;
        let folder: IShellFolder = unsafe { parent_item.BindToHandler(None, &BHID_SFObject) }
            .map_err(|e| format!("bind folder: {e}"))?;

        // Child pidls, freed on drop.
        struct Pidl(*mut ITEMIDLIST);
        impl Drop for Pidl {
            fn drop(&mut self) {
                unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(self.0 as *const c_void)) }
            }
        }
        let mut pidls: Vec<Pidl> = Vec::with_capacity(paths.len());
        for path in paths {
            let name = path.file_name().ok_or("item has no file name")?;
            let name_w: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            unsafe {
                folder
                    .ParseDisplayName(
                        hwnd,
                        None,
                        PCWSTR(name_w.as_ptr()),
                        None,
                        &mut pidl,
                        std::ptr::null_mut(),
                    )
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            pidls.push(Pidl(pidl));
        }
        let raw: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.0 as *const _).collect();
        unsafe {
            folder
                .GetUIObjectOf::<IContextMenu>(hwnd, &raw, None)
                .map_err(|e| format!("GetUIObjectOf: {e}"))
        }
    }

    /// Paste the clipboard's files into `dest`. Reads CF_HDROP plus the
    /// "Preferred DropEffect" format so a cut (move) is honoured, then hands
    /// the transfer to IFileOperation. The hosted shell "paste" verb needs a
    /// full folder-view site to work, so we do it directly instead.
    fn paste_into(hwnd: HWND, dest: &Path) -> Result<(), String> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::path::PathBuf;
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        use windows::Win32::System::Ole::CF_HDROP;
        use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

        const DROPEFFECT_MOVE: u32 = 2;

        unsafe {
            OpenClipboard(Some(hwnd)).map_err(|_| "clipboard is busy".to_string())?;
            // Everything below must run before CloseClipboard; collect, then close.
            let read = (|| -> Result<(Vec<PathBuf>, bool), String> {
                let handle = GetClipboardData(CF_HDROP.0 as u32)
                    .map_err(|_| "no files on the clipboard".to_string())?;
                if handle.0.is_null() {
                    return Err("no files on the clipboard".into());
                }
                let hdrop = HDROP(handle.0);
                let count = DragQueryFileW(hdrop, u32::MAX, None);
                let mut paths = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None) as usize;
                    let mut buf = vec![0u16; len + 1];
                    let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                    paths.push(PathBuf::from(OsString::from_wide(&buf[..n])));
                }
                // "Preferred DropEffect": DROPEFFECT_MOVE means the files were cut.
                let mut is_move = false;
                let fmt: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
                let cf = RegisterClipboardFormatW(PCWSTR(fmt.as_ptr()));
                if cf != 0 {
                    if let Ok(h) = GetClipboardData(cf) {
                        if !h.0.is_null() {
                            let p = GlobalLock(HGLOBAL(h.0)) as *const u32;
                            if !p.is_null() {
                                is_move = *p & DROPEFFECT_MOVE != 0;
                                let _ = GlobalUnlock(HGLOBAL(h.0));
                            }
                        }
                    }
                }
                Ok((paths, is_move))
            })();
            let _ = CloseClipboard();
            let (paths, is_move) = read?;
            transfer(&paths, dest, is_move)
        }
    }

    /// A folder's background IContextMenu (paste target, New submenu, and
    /// any third-party `Directory\Background` handlers).
    fn background_cm(hwnd: HWND, dir: &Path) -> Result<IContextMenu, String> {
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(wide(dir).as_ptr()), None) }
                .map_err(|e| format!("{}: {e}", dir.display()))?;
        let folder: IShellFolder = unsafe { item.BindToHandler(None, &BHID_SFObject) }
            .map_err(|e| format!("bind folder: {e}"))?;
        unsafe {
            folder
                .CreateViewObject::<IContextMenu>(hwnd)
                .map_err(|e| format!("CreateViewObject: {e}"))
        }
    }

    /// Enumerate the background menu into plain data for our themed renderer,
    /// caching the IContextMenu so a later click can invoke by id.
    pub fn enum_background(hwnd: HWND, dir: &Path) -> Result<Vec<MenuItem>, String> {
        let cm = background_cm(hwnd, dir)?;
        let menu = unsafe { CreatePopupMenu() }.map_err(|e| e.to_string())?;
        unsafe {
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL)
                .ok()
                .map_err(|e| format!("QueryContextMenu: {e}"))?;
        }
        let items = walk_menu(menu, &cm, 0);
        unsafe {
            let _ = DestroyMenu(menu);
        }
        BG_CACHE.with(|c| *c.borrow_mut() = Some((dir.to_owned(), cm)));
        Ok(items)
    }

    fn walk_menu(menu: HMENU, cm: &IContextMenu, depth: u8) -> Vec<MenuItem> {
        let mut out = Vec::new();
        if depth > 3 {
            return out; // guard against pathological nesting
        }
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        for i in 0..count {
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_SUBMENU,
                ..Default::default()
            };
            if unsafe { GetMenuItemInfoW(menu, i as u32, true, &mut mii) }.is_err() {
                continue;
            }
            if mii.fType.0 & MFT_SEPARATOR.0 != 0 {
                out.push(MenuItem::Separator);
                continue;
            }
            let mut buf = [0u16; 260];
            let n = unsafe { GetMenuStringW(menu, i as u32, Some(&mut buf), MF_BYPOSITION) };
            let label = clean_label(&String::from_utf16_lossy(&buf[..n as usize]));

            if !mii.hSubMenu.0.is_null() {
                // Populate dynamic submenus (New, Send To, …) before reading.
                if let Ok(cm2) = cm.cast::<IContextMenu2>() {
                    let _ = unsafe {
                        cm2.HandleMenuMsg(
                            WM_INITMENUPOPUP,
                            WPARAM(mii.hSubMenu.0 as usize),
                            LPARAM(i as isize),
                        )
                    };
                }
                let items = walk_menu(mii.hSubMenu, cm, depth + 1);
                if !label.is_empty() && !items.is_empty() {
                    out.push(MenuItem::Submenu { label, items });
                }
            } else if mii.wID != 0 && !label.is_empty() {
                let enabled = mii.fState.0 & MFS_GRAYED.0 == 0;
                out.push(MenuItem::Command {
                    id: mii.wID,
                    label,
                    verb: command_verb(cm, mii.wID),
                    enabled,
                });
            }
        }
        out
    }

    /// Canonical verb (e.g. "paste") for a command id, lowercased. Many
    /// handlers don't implement GetCommandString and return Err — that's fine.
    fn command_verb(cm: &IContextMenu, id: u32) -> Option<String> {
        let mut buf = [0u16; 128];
        // The verb goes in as a wide string despite the PSTR type; idcmd is
        // the offset (QueryContextMenu used idCmdFirst = 1).
        let ok = unsafe {
            cm.GetCommandString(
                (id - 1) as usize,
                GCS_VERBW,
                None,
                PSTR(buf.as_mut_ptr() as *mut u8),
                buf.len() as u32,
            )
        }
        .is_ok();
        if !ok {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let v = String::from_utf16_lossy(&buf[..end]);
        (!v.is_empty()).then(|| v.to_lowercase())
    }

    /// Invoke a background command previously enumerated for `dir`. Uses the
    /// cached IContextMenu when it matches; otherwise rebuilds it (ids are
    /// assigned deterministically, so they still line up).
    pub fn invoke_background(hwnd: HWND, dir: &Path, id: u32) -> Result<(), String> {
        let cached = BG_CACHE.with(|c| {
            c.borrow()
                .as_ref()
                .filter(|(d, _)| d == dir)
                .map(|(_, m)| m.clone())
        });
        let cm = match cached {
            Some(cm) => cm,
            None => {
                let cm = background_cm(hwnd, dir)?;
                let menu = unsafe { CreatePopupMenu() }.map_err(|e| e.to_string())?;
                unsafe {
                    cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL)
                        .ok()
                        .map_err(|e| format!("QueryContextMenu: {e}"))?;
                    let _ = DestroyMenu(menu);
                }
                cm
            }
        };
        let verb = PCSTR((id - 1) as usize as *const u8);
        let verb_w = PCWSTR((id - 1) as usize as *const u16);
        invoke(hwnd, &cm, verb, verb_w, dir).map(|_| ())
    }

    /// Strip a menu label's accelerator marker (`&`) and shortcut suffix.
    fn clean_label(raw: &str) -> String {
        let base = raw.split('\t').next().unwrap_or(raw);
        base.replace("&&", "\u{1}")
            .replace('&', "")
            .replace('\u{1}', "&")
    }

    /// Build the HMENU. Even verb-only invocation should query first: many
    /// handlers only bind their verbs during QueryContextMenu.
    fn prime_menu(_hwnd: HWND, cm: &IContextMenu) -> Result<HMENU, String> {
        let menu = unsafe { CreatePopupMenu() }.map_err(|e| e.to_string())?;
        unsafe {
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL)
                .ok()
                .map_err(|e| format!("QueryContextMenu: {e}"))?;
        }
        Ok(menu)
    }

    fn show_menu(hwnd: HWND, cm: &IContextMenu, work_dir: &Path) -> Result<bool, String> {
        let menu = prime_menu(hwnd, cm)?;
        ACTIVE_MENU.with(|m| *m.borrow_mut() = Some(cm.clone()));

        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            // Without this, clicking elsewhere fails to dismiss the menu
            // (same quirk as notification-area menus).
            let _ = SetForegroundWindow(hwnd);
        }
        let cmd = unsafe {
            TrackPopupMenuEx(
                menu,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                pt.x,
                pt.y,
                hwnd,
                None,
            )
        };

        ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
        unsafe {
            let _ = DestroyMenu(menu);
        }

        let id = cmd.0;
        if id <= 0 {
            return Ok(false); // dismissed
        }
        // TPM_RETURNCMD gives the menu item id; offset-1 back to the
        // handler's command id (MAKEINTRESOURCE convention: the ordinal in
        // the pointer's low word, valid as both the ANSI and wide verb).
        let verb = PCSTR((id - 1) as usize as *const u8);
        let verb_w = PCWSTR((id - 1) as usize as *const u16);
        invoke(hwnd, cm, verb, verb_w, work_dir)
    }

    fn invoke_verb(
        hwnd: HWND,
        cm: &IContextMenu,
        verb: &str,
        work_dir: &Path,
    ) -> Result<(), String> {
        let verb_c = std::ffi::CString::new(verb).map_err(|e| e.to_string())?;
        let verb_w: Vec<u16> = verb.encode_utf16().chain(std::iter::once(0)).collect();
        invoke(
            hwnd,
            cm,
            PCSTR(verb_c.as_ptr() as *const u8),
            PCWSTR(verb_w.as_ptr()),
            work_dir,
        )
        .map(|_| ())
        .map_err(|e| format!("verb {verb:?}: {e}"))
    }

    /// Invoke a command, passing the target directory. Background verbs
    /// (New, Paste) resolve their target from `lpDirectory`, so without it
    /// they fail with E_FAIL — item verbs carry it in their pidls, but
    /// setting it for both is correct and harmless.
    fn invoke(
        hwnd: HWND,
        cm: &IContextMenu,
        verb: PCSTR,
        verb_w: PCWSTR,
        work_dir: &Path,
    ) -> Result<bool, String> {
        let dir_w = wide(work_dir);
        let dir_a = std::ffi::CString::new(work_dir.to_string_lossy().as_bytes().to_vec())
            .unwrap_or_default();
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
            hwnd,
            lpVerb: verb,
            lpVerbW: verb_w,
            lpDirectory: PCSTR(dir_a.as_ptr() as *const u8),
            lpDirectoryW: PCWSTR(dir_w.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: cursor,
            ..Default::default()
        };
        unsafe {
            cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO)
                .map_err(|e| format!("InvokeCommand: {e}"))?;
        }
        Ok(true)
    }

    pub fn recycle(paths: &[std::path::PathBuf]) -> Result<(), String> {
        let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }
            .map_err(|e| format!("FileOperation: {e}"))?;
        unsafe {
            op.SetOperationFlags(FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT)
                .map_err(|e| e.to_string())?;
            for path in paths {
                let item: IShellItem =
                    SHCreateItemFromParsingName(PCWSTR(wide(path).as_ptr()), None)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                op.DeleteItem(&item, None).map_err(|e| e.to_string())?;
            }
            op.PerformOperations().map_err(|e| format!("delete: {e}"))?;
        }
        Ok(())
    }

    /// Hidden window owning menus; forwards menu messages to the active
    /// IContextMenu2/3 so owner-drawn items and dynamic submenus work.
    unsafe extern "system" fn owner_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if matches!(
            msg,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        ) {
            let handled = ACTIVE_MENU.with(|m| {
                let borrowed = m.borrow();
                let Some(cm) = borrowed.as_ref() else {
                    return false;
                };
                if let Ok(cm3) = cm.cast::<IContextMenu3>() {
                    let mut result = LRESULT(0);
                    unsafe {
                        if cm3
                            .HandleMenuMsg2(msg, wparam, lparam, Some(&mut result))
                            .is_ok()
                        {
                            return true;
                        }
                    }
                } else if let Ok(cm2) = cm.cast::<IContextMenu2>() {
                    unsafe {
                        if cm2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                            return true;
                        }
                    }
                }
                false
            });
            if handled {
                return LRESULT(0);
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    pub fn create_owner_window() -> Result<HWND, String> {
        unsafe {
            let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class_name: Vec<u16> = "fx-shell-owner\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(owner_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                Default::default(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            Ok(hwnd)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

        #[test]
        fn recycle_really_deletes() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let path = std::env::temp_dir().join("fx-shell-recycle-test.txt");
            std::fs::write(&path, b"bye").unwrap();
            recycle(&[path.clone()]).unwrap();
            assert!(
                !path.exists(),
                "file should be in the recycle bin, not on disk"
            );
        }
    }
}
