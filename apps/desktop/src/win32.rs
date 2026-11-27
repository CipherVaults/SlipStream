use crate::paths::{canonical_directory, local_drive_path, validate_existing_ancestors};
use std::{
    ffi::OsString,
    io,
    mem::{size_of, size_of_val},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{self, null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, GlobalFree, SetLastError, ERROR_SUCCESS, HGLOBAL, HWND, POINT, RECT,
    },
    Graphics::{
        Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND},
        Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    },
    Storage::FileSystem::GetDiskFreeSpaceExW,
    System::{
        Com::{
            CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
            COINIT_DISABLE_OLE1DDE,
        },
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            OpenClipboard, SetClipboardData,
        },
        Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
        Ole::CF_UNICODETEXT,
        Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
    },
    UI::{
        Controls::Dialogs::{
            CommDlgExtendedError, GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
            OFN_PATHMUSTEXIST, OPENFILENAMEW,
        },
        Shell::{
            SHBrowseForFolderW, SHGetPathFromIDListEx, ShellExecuteW, BFFM_INITIALIZED,
            BFFM_SETSELECTIONW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
            GPFIDL_DEFAULT,
        },
        WindowsAndMessaging::{
            BringWindowToTop, CreateWindowExW, DestroyWindow, FlashWindowEx, GetCursorPos,
            GetWindowRect, IsIconic, SendMessageW, SetForegroundWindow, SetWindowPos, ShowWindow,
            FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY, HWND_MESSAGE, HWND_NOTOPMOST, HWND_TOPMOST,
            SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_RESTORE,
            SW_SHOWNORMAL,
        },
    },
};

pub fn bring_window_to_front(hwnd: isize) {
    if hwnd == 0 {
        return;
    }

    let hwnd = hwnd as HWND;
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
        let activated = SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        if activated == 0 {
            FlashWindowEx(&FLASHWINFO {
                cbSize: size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                uCount: 0,
                dwTimeout: 0,
            });
        }
    }
}

pub fn center_window_on_active_monitor(hwnd: isize, cascade_index: i32) {
    if hwnd == 0 {
        return;
    }
    let hwnd = hwnd as HWND;
    let mut cursor = POINT { x: 0, y: 0 };
    let empty_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut monitor_info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        rcMonitor: empty_rect,
        rcWork: empty_rect,
        dwFlags: 0,
    };
    let mut window = empty_rect;
    unsafe {
        if GetCursorPos(&mut cursor) == 0 {
            return;
        }
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null()
            || GetMonitorInfoW(monitor, &mut monitor_info) == 0
            || GetWindowRect(hwnd, &mut window) == 0
        {
            return;
        }

        let work = monitor_info.rcWork;
        let current_width = window.right - window.left;
        let current_height = window.bottom - window.top;
        let width = current_width.min(work.right - work.left);
        let height = current_height.min(work.bottom - work.top);
        let offset = (cascade_index % 5) * 24;
        let center = |start: i32, end: i32, size: i32| {
            let last = (end - size).max(start);
            (start + (last - start) / 2 + offset).clamp(start, last)
        };
        let x = center(work.left, work.right, width);
        let y = center(work.top, work.bottom, height);
        let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
        if width == current_width && height == current_height {
            flags |= SWP_NOSIZE;
        }
        SetWindowPos(hwnd, null_mut(), x, y, width, height, flags);
    }
}

pub fn open_file(path: &Path) -> io::Result<()> {
    let file = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            windows_sys::w!("open"),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(io::Error::other(format!(
            "Windows could not open the file (shell error {}).",
            result as isize
        )));
    }
    Ok(())
}

pub fn pick_download_folder(
    owner: isize,
    initial_dir: Option<String>,
) -> io::Result<Option<String>> {
    unsafe extern "system" fn select_initial_folder(
        window: HWND,
        message: u32,
        _: isize,
        initial: isize,
    ) -> i32 {
        if message == BFFM_INITIALIZED && initial != 0 {
            // The caller owns the NUL-terminated path for the lifetime of the browse dialog.
            unsafe { SendMessageW(window, BFFM_SETSELECTIONW, 1, initial) };
        }
        0
    }

    let initial = initial_dir
        .filter(|path| !path.trim().is_empty())
        .map(|path| {
            let directory = local_drive_path(Path::new(&path))?;
            validate_existing_ancestors(&directory)?;
            Ok::<_, io::Error>(
                directory
                    .ancestors()
                    .find(|path| path.is_dir())
                    .map(|path| {
                        path.as_os_str()
                            .encode_wide()
                            .chain(Some(0))
                            .collect::<Vec<_>>()
                    }),
            )
        })
        .transpose()?
        .flatten();

    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    let code = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if code < 0 {
        return Err(io::Error::other(format!(
            "Could not initialize the folder picker (0x{code:08x})."
        )));
    }
    let _apartment = ComApartment;

    let mut display = [0_u16; 260];
    let info = BROWSEINFOW {
        hwndOwner: owner as HWND,
        pidlRoot: null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: windows_sys::w!("Select a download folder"),
        ulFlags: BIF_NEWDIALOGSTYLE | BIF_RETURNONLYFSDIRS,
        lpfn: Some(select_initial_folder),
        lParam: initial.as_ref().map_or(0, |path| path.as_ptr() as isize),
        iImage: 0,
    };
    let mut selected = vec![0_u16; 32_768];
    let resolved = unsafe {
        let item = SHBrowseForFolderW(&info);
        if item.is_null() {
            return Ok(None);
        }
        let found = SHGetPathFromIDListEx(
            item,
            selected.as_mut_ptr(),
            selected.len() as u32,
            GPFIDL_DEFAULT,
        );
        CoTaskMemFree(item.cast());
        found != 0
    };
    if !resolved {
        return Err(io::Error::other(
            "Could not resolve the selected download folder.",
        ));
    }

    let end = selected
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(selected.len());
    let path = PathBuf::from(OsString::from_wide(&selected[..end]));
    let path = local_drive_path(&canonical_directory(&path)?)?;
    Ok(Some(path.into_os_string().into_string().map_err(|_| {
        io::Error::other("Download folder is not Unicode.")
    })?))
}

pub fn round_corners(hwnd: isize) {
    // Windows 10 can reject this attribute.
    unsafe {
        DwmSetWindowAttribute(
            hwnd as HWND,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            ptr::from_ref(&DWMWCP_ROUND).cast(),
            size_of::<i32>() as u32,
        );
    }
}

pub fn is_system_light_theme() -> bool {
    let mut light = 0_u32;
    let mut len = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows_sys::w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            windows_sys::w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            null_mut(),
            ptr::from_mut(&mut light).cast(),
            &mut len,
        )
    };
    status == ERROR_SUCCESS && light == 1
}

pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    if text.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Clipboard text contains a NUL character.",
        ));
    }
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();

    struct ClipboardOwner(HWND);
    impl Drop for ClipboardOwner {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0) };
        }
    }

    struct Clipboard;
    impl Drop for Clipboard {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    struct ClipboardMemory(HGLOBAL);
    impl Drop for ClipboardMemory {
        fn drop(&mut self) {
            unsafe { GlobalFree(self.0) };
        }
    }

    // EmptyClipboard needs a window owner for the subsequent SetClipboardData call.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            windows_sys::w!("STATIC"),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    let owner = ClipboardOwner(hwnd);
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, size_of_val(text.as_slice())) };
    if hmem.is_null() {
        return Err(io::Error::last_os_error());
    }
    let memory = ClipboardMemory(hmem);
    unsafe {
        let data = GlobalLock(memory.0).cast::<u16>();
        if data.is_null() {
            return Err(io::Error::last_os_error());
        }
        ptr::copy_nonoverlapping(text.as_ptr(), data, text.len());
        SetLastError(ERROR_SUCCESS);
        if GlobalUnlock(memory.0) == 0 && GetLastError() != ERROR_SUCCESS {
            return Err(io::Error::last_os_error());
        }
        if OpenClipboard(owner.0) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _clipboard = Clipboard;
        if EmptyClipboard() == 0 {
            return Err(io::Error::last_os_error());
        }
        if SetClipboardData(CF_UNICODETEXT.into(), memory.0).is_null() {
            return Err(io::Error::last_os_error());
        }
        std::mem::forget(memory); // SetClipboardData transferred ownership to Windows.
    }
    Ok(())
}

pub fn open_file_with(path: &Path) -> io::Result<()> {
    let file = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            windows_sys::w!("openas"),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(io::Error::other(format!(
            "Windows could not open the app chooser for this file (shell error {}).",
            result as isize
        )));
    }
    Ok(())
}

pub fn pick_torrent_file(owner: isize, initial_dir: Option<String>) -> io::Result<Option<String>> {
    let filter: Vec<u16> = "Torrent files (*.torrent)\0*.torrent\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Select a .torrent file\0".encode_utf16().collect();
    let initial = initial_dir
        .filter(|path| !path.trim().is_empty())
        .and_then(|path| {
            let p = PathBuf::from(path);
            let existing = p.ancestors().find(|a| a.is_dir())?;
            Some(
                existing
                    .as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect::<Vec<_>>(),
            )
        });
    let mut file_buf = vec![0_u16; 32_768];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner as HWND;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.lpstrInitialDir = initial.as_ref().map_or(null(), |v| v.as_ptr());
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        let code = unsafe { CommDlgExtendedError() };
        if code == 0 {
            return Ok(None);
        }
        return Err(io::Error::other(format!(
            "File picker dialog failed (0x{code:04x})."
        )));
    }
    let end = file_buf
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(file_buf.len());
    let path = PathBuf::from(OsString::from_wide(&file_buf[..end]));
    let path_str = path
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("Torrent path is not Unicode."))?;
    Ok(Some(path_str))
}

pub fn disk_space_info(dir: &Path) -> Option<(u64, u64)> {
    let existing = dir.ancestors().find(|p| p.is_dir())?;
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free_to_caller = 0_u64;
    let mut total_bytes = 0_u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total_bytes,
            null_mut(),
        )
    };
    if ok == 0 || total_bytes == 0 {
        None
    } else {
        Some((free_to_caller, total_bytes))
    }
}

pub fn clipboard_sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

fn decode_bounded_utf16(units: &[u16], cap: usize) -> Option<String> {
    let limit = units.len().min(cap);
    let end = units[..limit].iter().position(|&ch| ch == 0)?;
    String::from_utf16(&units[..end]).ok()
}

pub fn read_clipboard_text() -> Option<String> {
    struct Clipboard;
    impl Drop for Clipboard {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    const MAX_CLIPBOARD_UNITS: usize = 32 * 1024;
    if unsafe { OpenClipboard(null_mut()) } == 0 {
        return None;
    }
    let _clipboard = Clipboard;
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT.into()) };
    if handle.is_null() {
        return None;
    }
    let alloc_bytes = unsafe { GlobalSize(handle.cast()) };
    let alloc_units = alloc_bytes / size_of::<u16>();
    if alloc_units == 0 {
        return None;
    }
    let bounded_units = alloc_units.min(MAX_CLIPBOARD_UNITS);
    let ptr = unsafe { GlobalLock(handle.cast()) }.cast::<u16>();
    if ptr.is_null() {
        return None;
    }
    let text = unsafe {
        let slice = std::slice::from_raw_parts(ptr, bounded_units);
        decode_bounded_utf16(slice, MAX_CLIPBOARD_UNITS)
    };
    unsafe { GlobalUnlock(handle.cast()) };
    text
}

#[cfg(test)]
mod tests {
    use super::decode_bounded_utf16;

    #[test]
    fn bounded_utf16_requires_nul_within_allocation_and_cap() {
        assert_eq!(
            decode_bounded_utf16(&[b'h' as u16, b'i' as u16, 0], 16),
            Some("hi".into())
        );
        assert_eq!(decode_bounded_utf16(&[b'h' as u16, b'i' as u16], 16), None);
        assert_eq!(
            decode_bounded_utf16(&[b'a' as u16, b'b' as u16, b'c' as u16, 0], 2),
            None
        );
    }
}
