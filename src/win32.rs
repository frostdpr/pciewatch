//! Minimal raw Win32 plumbing: the main-thread message pump and balloon
//! notifications via Shell_NotifyIconW.
//!
//! The balloon is attached to the tray icon itself (identified by GUID — see
//! TRAY_GUID, passed to TrayIconBuilder::with_guid). With NIF_GUID set, the
//! shell identifies the icon by guidItem alone, so no second tray entry is
//! needed. (An entry without an icon both reserves an empty tray slot on Win11
//! and silently swallows balloons — hence the original two-slot bug.)
//!
//! tray-icon 0.25 requires its icon to be created on a thread with a running
//! message pump; the pump here (WaitMessage + PeekMessage) also provides the
//! ~500 ms periodic wake the coordinator uses for its staleness watchdog.

use std::ffi::c_void;
use std::mem;
use std::ptr;

use windows_sys::core::GUID;

/// Unique GUID identifying our tray icon to the shell.
/// Must be passed to TrayIconBuilder::with_guid — the balloon NIM_MODIFY below
/// targets this same guid (see guid_mem_bytes for the byte encoding).
pub const TRAY_GUID: u128 = 0x420e1bd2_5733_6c9a_8f4e_2d4b_110a_9c71;

/// The exact 16 bytes the shell stores in `guidItem` for `GUID::from_u128(v)`:
/// the in-memory layout of windows-sys's `#[repr(C)] GUID { u32 data1 (LE),
/// u16 data2 (LE), u16 data3 (LE), [u8;8] data4 (raw) }` on x86-64.
///
/// This is *not* `v.to_le_bytes()` — data4 is stored raw (big-endian of the
/// low 64 bits). We derive the bytes from the same windows-sys GUID type
/// tray-icon 0.25 uses when it registers the icon, so the two are identical
/// by construction.
fn guid_mem_bytes(v: u128) -> [u8; 16] {
    let g = GUID::from_u128(v);
    let mut b = [0u8; 16];
    unsafe {
        ptr::copy_nonoverlapping((&g as *const GUID).cast(), b.as_mut_ptr(), 16);
    }
    b
}

type Hwnd = *mut c_void;

const PM_REMOVE: u32 = 0x0001;

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: Point,
}

/// NOTIFYICONDATAW (windows.h)
#[repr(C)]
struct Nid {
    cb_size: u32,
    hwnd: Hwnd,
    uid: u32,
    uflags: u32,
    ucallback: u32,
    hicon: Hwnd,
    sz_tip: [u16; 128],
    dw_state: u32,
    dw_state_mask: u32,
    sz_info: [u16; 256],
    u_timeout: u32,
    sz_info_title: [u16; 64],
    dw_info_flags: u32,
    guid_item: [u8; 16],
    h_balloon_icon: Hwnd,
}

const NIM_MODIFY: u32 = 1;
const NIF_INFO: u32 = 0x0000_0010;
const NIF_GUID: u32 = 0x0000_0020;
const NIF_TIP: u32 = 0x0000_0004;

const NIIF_INFO: u32 = 0x0000_0001;
const NIIF_WARNING: u32 = 0x0000_0002;
const NIIF_ERROR: u32 = 0x0000_0004;

/// tray-icon 0.25's private message that updates its internal tooltip copy
/// (used when it re-registers the icon after an explorer.exe restart). We
/// must keep that copy in sync because our raw set_tip bypasses the crate's
/// own set_tooltip.
const WM_USER_UPDATE_TRAYTOOLTIP: u32 = 6006;

#[link(name = "user32")]
extern "system" {
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, filter: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn WaitMessage() -> i32;
    fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
}

#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(msg: u32, data: *const Nid) -> i32;
}

pub enum Kind {
    Info,
    Warning,
    Error,
}

/// Singleton guard: create a process-lifetime named mutex and report whether
/// it already existed (i.e. another instance is running).
///
/// The mutex is owned by the kernel for our whole lifetime and is released
/// automatically when the process exits — including a crash — so a stale
/// lock can never block a later start. Two instances would mean two tray
/// icons, so the second one must refuse to start.
pub fn ensure_singleton() -> bool {
    const ERROR_ALREADY_EXISTS: i32 = 183;
    let name: Vec<u16> = "PcieWatch_Singleton"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    }

    let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    let already = std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS);
    // The raw pointer never closes the OS handle: it stays open ("leaked") for
    // the whole process lifetime, which is exactly what the guard needs.
    if !already && handle.is_null() {
        // Creation failed for some other reason — don't block startup.
        return true;
    }
    !already
}

/// Set the tray icon's hover tooltip, targeting the icon by GUID.
///
/// tray-icon 0.25's own `set_tooltip` stamps `hWnd`/`uID` on its NIM_MODIFY but
/// forgets `NIF_GUID` (unlike its `set_icon` and `set_tray_visible`), so for a
/// GUID-registered icon the shell lookup silently matches nothing and the
/// tooltip is never updated. This raw variant is the fix.
///
/// `hwnd` is the tray icon's window (tray.window_handle()); it is not used for
/// shell addressing but is sent the crate's internal tooltip-update message so
/// the copy used on explorer-restart re-registration stays current.
pub fn set_tip(hwnd: Hwnd, title: &str) {
    let base = unsafe { mem::zeroed::<Nid>() };
    let mut d = Nid {
        cb_size: mem::size_of::<Nid>() as u32,
        uflags: NIF_TIP | NIF_GUID,
        guid_item: guid_mem_bytes(TRAY_GUID),
        ..base
    };
    let tip: Vec<u16> = title.encode_utf16().take(127).collect();
    d.sz_tip[..tip.len()].copy_from_slice(&tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        let _ = SendMessageW(
            hwnd,
            WM_USER_UPDATE_TRAYTOOLTIP,
            Box::into_raw(Box::new(Some(title.to_string()))) as usize,
            0,
        );
    }
}

/// Show a balloon on the tray icon (by GUID). hWnd/uID are ignored when
/// NIF_GUID is set, so nothing else is needed.
pub fn balloon<T: AsRef<str>>(title: &str, msg: T, kind: Kind) {
    let flags = match kind {
        Kind::Info => NIIF_INFO,
        Kind::Warning => NIIF_WARNING,
        Kind::Error => NIIF_ERROR,
    };
    let base = unsafe { mem::zeroed::<Nid>() };
    let mut d = Nid {
        cb_size: mem::size_of::<Nid>() as u32,
        uflags: NIF_INFO | NIF_GUID,
        u_timeout: 10_000, // ms; < 30000 => display duration
        dw_info_flags: flags,
        guid_item: guid_mem_bytes(TRAY_GUID),
        ..base
    };
    let info: Vec<u16> = msg.as_ref().encode_utf16().take(255).collect();
    d.sz_info[..info.len()].copy_from_slice(&info);
    let title: Vec<u16> = title.encode_utf16().take(63).collect();
    d.sz_info_title[..title.len()].copy_from_slice(&title);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
    }
}

/// Message pump: WaitMessage wakes on a message or a ~500 ms timeout, then
/// `tick` runs (draining app channels / doing the watchdog check).
pub fn pump(mut tick: impl FnMut() -> bool) {
    let mut msg = unsafe { mem::zeroed::<Msg>() };
    loop {
        unsafe {
            WaitMessage();
            while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if !tick() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_bytes_match_shell_layout() {
        // TRAY_GUID = 0x420e1bd2_5733_6c9a_8f4e_2d4b_110a_9c71
        //   data1 = 0x420e1bd2 (LE) -> d2 1b 0e 42
        //   data2 = 0x5733     (LE) -> 33 57
        //   data3 = 0x6c9a     (LE) -> 9a 6c
        //   data4 = raw bytes of low 64 bits -> 8f 4e 2d 4b 11 0a 9c 71
        // (deliberately != TRAY_GUID.to_le_bytes())
        assert_eq!(
            guid_mem_bytes(TRAY_GUID),
            [
                0xd2, 0x1b, 0x0e, 0x42, 0x33, 0x57, 0x9a, 0x6c, 0x8f, 0x4e, 0x2d, 0x4b, 0x11, 0x0a,
                0x9c, 0x71
            ]
        );
        assert_ne!(guid_mem_bytes(TRAY_GUID), TRAY_GUID.to_le_bytes());
    }
}
