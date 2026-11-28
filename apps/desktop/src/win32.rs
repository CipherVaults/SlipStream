use crate::paths::{canonical_directory, local_drive_path, validate_existing_ancestors};
use std::{
    collections::BTreeSet,
    ffi::{c_void, OsString},
    io,
    mem::{align_of, size_of, size_of_val},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{self, null, null_mut},
};
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, GlobalFree, SetLastError, ERROR_BUFFER_OVERFLOW,
            ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, HGLOBAL, HWND, INVALID_HANDLE_VALUE,
            POINT, RECT, WIN32_ERROR,
        },
        Graphics::{
            Dwm::{
                DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
            },
            Gdi::{
                GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
                MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
            },
        },
        NetworkManagement::{
            IpHelper::{
                GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
                GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, IP_ADAPTER_ADDRESSES_LH,
            },
            Ndis::IfOperStatusUp,
        },
        Networking::WinSock::AF_UNSPEC,
        Storage::FileSystem::GetDiskFreeSpaceExW,
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
            },
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
                OpenClipboard, SetClipboardData,
            },
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
            Ole::CF_UNICODETEXT,
            Registry::{
                RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW,
                RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_DWORD,
            },
            Threading::GetCurrentProcessId,
        },
        UI::{
            Controls::Dialogs::{
                CommDlgExtendedError, GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
                OFN_PATHMUSTEXIST, OPENFILENAMEW,
            },
            Input::KeyboardAndMouse::{GetActiveWindow, GetKeyState, VK_CONTROL, VK_SHIFT},
            Shell::{
                Common::COMDLG_FILTERSPEC, SHBrowseForFolderW, SHGetPathFromIDListEx,
                ShellExecuteW, BFFM_INITIALIZED, BFFM_SETSELECTIONW, BIF_NEWDIALOGSTYLE,
                BIF_RETURNONLYFSDIRS, BROWSEINFOW, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM,
                FOS_NOCHANGEDIR, FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, GPFIDL_DEFAULT,
                SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{
                BringWindowToTop, CreateWindowExW, DestroyWindow, FlashWindowEx, GetClassNameW,
                GetCursorPos, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
                IsIconic, SendMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, FLASHWINFO,
                FLASHW_TIMERNOFG, FLASHW_TRAY, HWND_MESSAGE, HWND_NOTOPMOST, HWND_TOPMOST, MB_OK,
                SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_RESTORE,
                SW_SHOWNORMAL,
            },
        },
    },
};

fn active_network_adapters() -> Vec<(String, String)> {
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    const MAX_ADAPTERS: usize = 4096;
    const FLAGS: u32 = GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    type Adapter = IP_ADAPTER_ADDRESSES_LH;
    const _: () = assert!(align_of::<u64>() >= align_of::<Adapter>());
    let mut size = 15 * 1024;
    for _ in 0..3 {
        if size == 0 || size as usize > MAX_BYTES {
            break;
        }
        // Allocate aligned, initialized storage for the structures and strings
        // returned together by GetAdaptersAddresses.
        let mut buffer = vec![0u64; (size as usize).div_ceil(size_of::<u64>())];
        let start = buffer.as_mut_ptr().cast::<Adapter>();
        let capacity = size as usize;
        match unsafe { GetAdaptersAddresses(u32::from(AF_UNSPEC), FLAGS, null(), start, &mut size) }
        {
            ERROR_BUFFER_OVERFLOW => continue,
            ERROR_SUCCESS => {}
            _ => break,
        }
        let contains = |addr: usize, len: usize| {
            addr.checked_sub(start.addr())
                .is_some_and(|offset| offset <= capacity && len <= capacity - offset)
        };
        let decode = |ptr: *const u16| {
            if !ptr.is_aligned() || !contains(ptr.addr(), size_of::<u16>()) {
                return String::new();
            }
            let remaining = capacity - (ptr.addr() - start.addr());
            let text = unsafe { std::slice::from_raw_parts(ptr, (remaining / 2).min(512)) };
            let len = text
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(text.len());
            String::from_utf16_lossy(&text[..len]).trim().to_owned()
        };
        let mut adapters = Vec::new();
        let mut next = start;
        for _ in 0..MAX_ADAPTERS {
            if !next.addr().is_multiple_of(align_of::<Adapter>())
                || !contains(next.addr(), size_of::<Adapter>())
            {
                break;
            }
            let adapter = unsafe { &*next };
            if adapter.OperStatus == IfOperStatusUp && adapter.IfType != 24 {
                adapters.push((decode(adapter.FriendlyName), decode(adapter.Description)));
            }
            next = adapter.Next;
        }
        return adapters;
    }
    Vec::new()
}

pub fn list_active_network_adapters() -> Vec<String> {
    active_network_adapters()
        .into_iter()
        .map(|(name, description)| if name.is_empty() { description } else { name })
        .filter(|name| !name.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn is_network_adapter_up(pattern: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    !pattern.is_empty()
        && active_network_adapters()
            .into_iter()
            .any(|(name, description)| {
                name.to_lowercase().contains(&pattern)
                    || description.to_lowercase().contains(&pattern)
            })
}

pub fn set_launch_on_startup(enabled: bool, minimized: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    set_launch_on_startup_at(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        &exe,
        enabled,
        minimized,
    )
}

fn set_launch_on_startup_at(
    subkey: &str,
    exe_path: &Path,
    enabled: bool,
    minimized: bool,
) -> Result<(), String> {
    struct RegistryKey(HKEY);
    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }
    fn check(status: WIN32_ERROR) -> Result<(), String> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32).to_string())
        }
    }
    let name = windows_sys::w!("SlipStream");
    let subkey: Vec<_> = subkey.encode_utf16().chain([0]).collect();
    let mut key = null_mut();
    if !enabled {
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(status)?;
        let key = RegistryKey(key);
        let status = unsafe { RegDeleteValueW(key.0, name) };
        return if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            check(status)
        };
    }

    let exe: Vec<_> = exe_path.as_os_str().encode_wide().collect();
    if exe.is_empty() || exe.iter().any(|&unit| unit == 0 || unit == u16::from(b'"')) {
        return Err("Invalid startup executable path".into());
    }
    let mut command = vec![u16::from(b'"')];
    command.extend(exe);
    command.push(u16::from(b'"'));
    if minimized {
        command.extend(" --minimized".encode_utf16());
    }
    command.push(0);
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        )
    })?;
    let key = RegistryKey(key);
    check(unsafe {
        RegSetValueExW(
            key.0,
            name,
            0,
            REG_SZ,
            command.as_ptr().cast(),
            size_of_val(command.as_slice()) as u32,
        )
    })
}

pub fn play_completion_sound() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBeep(kind: u32) -> i32;
    }
    unsafe { MessageBeep(MB_OK) };
}

pub fn is_ctrl_pressed() -> bool {
    (unsafe { GetKeyState(i32::from(VK_CONTROL)) } as u16 & 0x8000) != 0
}

pub fn is_shift_pressed() -> bool {
    (unsafe { GetKeyState(i32::from(VK_SHIFT)) } as u16 & 0x8000) != 0
}

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

pub fn position_drop_basket(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let hwnd = hwnd as HWND;
    let mut cursor = POINT { x: 0, y: 0 };
    let mut monitor_info: MONITORINFO = unsafe { std::mem::zeroed() };
    monitor_info.cbSize = size_of::<MONITORINFO>() as u32;
    let mut window: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        let monitor = if GetCursorPos(&mut cursor) != 0 {
            MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST)
        } else {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };
        if monitor.is_null()
            || GetMonitorInfoW(monitor, &mut monitor_info) == 0
            || GetWindowRect(hwnd, &mut window) == 0
        {
            return;
        }
        let work = monitor_info.rcWork;
        let x = (work.right - (window.right - window.left) - 20).max(work.left);
        let y = (work.bottom - (window.bottom - window.top) - 20).max(work.top);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

struct ComGuard;

impl ComGuard {
    fn initialize() -> io::Result<Self> {
        let code = unsafe {
            CoInitializeEx(
                null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        if code < 0 {
            return Err(io::Error::other(format!(
                "Could not initialize the file picker (0x{code:08x})."
            )));
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
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
    pick_folder(owner, initial_dir, "Select a download folder")
}

pub(crate) fn pick_folder(
    owner: isize,
    initial_dir: Option<String>,
    title: &str,
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

    let _apartment = ComGuard::initialize()?;

    let title: Vec<_> = title.encode_utf16().chain(Some(0)).collect();
    let mut display = [0_u16; 260];
    let info = BROWSEINFOW {
        hwndOwner: owner as HWND,
        pidlRoot: null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
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
        return Err(io::Error::other("Could not resolve the selected folder."));
    }

    let end = selected
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(selected.len());
    let path = PathBuf::from(OsString::from_wide(&selected[..end]));
    let path = local_drive_path(&canonical_directory(&path)?)?;
    Ok(Some(path.into_os_string().into_string().map_err(|_| {
        io::Error::other("Selected folder is not Unicode.")
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

pub fn pick_text_import_file(owner: isize) -> Option<String> {
    pick_text_file(owner, None)
}

pub fn pick_text_export_file(owner: isize, default_name: &str) -> Option<String> {
    pick_text_file(owner, Some(default_name))
}

// windows-sys exposes COM activation but omits the interface vtables.
// The unused slots preserve the layout of IFileDialog and IShellItem.
#[repr(C)]
struct UnknownVtable {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct FileDialogVtable {
    unknown: UnknownVtable,
    show: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
    set_file_types: unsafe extern "system" fn(*mut c_void, u32, *const COMDLG_FILTERSPEC) -> i32,
    file_type_and_events: [usize; 4],
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    folders_and_selection: [usize; 4],
    set_file_name: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    text_and_labels: [usize; 4],
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    add_place: usize,
    set_default_extension: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
}

#[repr(C)]
struct ShellItemVtable {
    unknown: UnknownVtable,
    bind_to_handler: usize,
    get_parent: usize,
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> i32,
}

struct ComInterface(*mut c_void);

impl Drop for ComInterface {
    fn drop(&mut self) {
        unsafe {
            let vtable = *self.0.cast::<*const UnknownVtable>();
            ((*vtable).release)(self.0);
        }
    }
}

fn pick_dialog_file(
    owner: isize,
    save: bool,
    default_name: Option<&str>,
    filter_name: *const u16,
    filter_spec: *const u16,
    default_ext: *const u16,
) -> Option<String> {
    const FILE_OPEN_DIALOG: GUID = GUID::from_u128(0xdc1c5a9c_e88a_4dde_a5a1_60f82a20aef7);
    const IFILE_OPEN_DIALOG: GUID = GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
    const FILE_SAVE_DIALOG: GUID = GUID::from_u128(0xc0b4e2f3_ba21_4773_8dba_335ec946eb8b);
    const IFILE_SAVE_DIALOG: GUID = GUID::from_u128(0x84bccd23_5fde_4cdb_aea4_af64b83d78ab);

    let _apartment = ComGuard::initialize().ok()?;
    let (class, interface, extra_options) = if save {
        (FILE_SAVE_DIALOG, IFILE_SAVE_DIALOG, FOS_OVERWRITEPROMPT)
    } else {
        (FILE_OPEN_DIALOG, IFILE_OPEN_DIALOG, FOS_FILEMUSTEXIST)
    };
    let filter = COMDLG_FILTERSPEC {
        pszName: filter_name,
        pszSpec: filter_spec,
    };

    // Show pumps the calling STA's messages while the dialog is open.
    unsafe {
        let mut object = null_mut();
        if CoCreateInstance(
            &class,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &interface,
            &mut object,
        ) < 0
            || object.is_null()
        {
            return None;
        }
        let _dialog = ComInterface(object);
        let dialog = &**object.cast::<*const FileDialogVtable>();
        let mut options = 0;
        if (dialog.get_options)(object, &mut options) < 0
            || (dialog.set_options)(
                object,
                options | extra_options | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR | FOS_PATHMUSTEXIST,
            ) < 0
            || (dialog.set_file_types)(object, 1, &filter) < 0
        {
            return None;
        }
        if let Some(name) = default_name {
            let name_wide: Vec<_> = name.encode_utf16().chain([0]).collect();
            if (dialog.set_default_extension)(object, default_ext) < 0
                || (dialog.set_file_name)(object, name_wide.as_ptr()) < 0
            {
                return None;
            }
        }
        let dialog_owner = if owner != 0 {
            owner as HWND
        } else {
            GetActiveWindow()
        };
        if (dialog.show)(object, dialog_owner) < 0 {
            return None;
        }
        let mut item = null_mut();
        if (dialog.get_result)(object, &mut item) < 0 || item.is_null() {
            return None;
        }
        let _item = ComInterface(item);
        let vtable = &**item.cast::<*const ShellItemVtable>();
        let mut path = null_mut();
        if (vtable.get_display_name)(item, SIGDN_FILESYSPATH, &mut path) < 0 || path.is_null() {
            return None;
        }
        struct ComString(*mut u16);
        impl Drop for ComString {
            fn drop(&mut self) {
                unsafe { CoTaskMemFree(self.0.cast()) };
            }
        }
        let path = ComString(path);
        let len = (0..).find(|&i| *path.0.add(i) == 0)?;
        String::from_utf16(std::slice::from_raw_parts(path.0, len)).ok()
    }
}

fn pick_text_file(owner: isize, default_name: Option<&str>) -> Option<String> {
    if let Some(name) = default_name {
        let ext = if Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("log"))
        {
            windows_sys::w!("log")
        } else {
            windows_sys::w!("txt")
        };
        pick_dialog_file(
            owner,
            true,
            Some(name),
            windows_sys::w!("Text and log files (*.txt; *.log)"),
            windows_sys::w!("*.txt;*.log"),
            ext,
        )
    } else {
        pick_dialog_file(
            owner,
            false,
            None,
            windows_sys::w!("URL lists (*.txt; *.ef2)"),
            windows_sys::w!("*.txt;*.ef2"),
            windows_sys::w!("txt"),
        )
    }
}

pub fn pick_json_backup_file(
    save: bool,
    default_name: Option<&str>,
    owner: isize,
) -> Option<String> {
    pick_dialog_file(
        owner,
        save,
        default_name,
        windows_sys::w!("SlipStream Settings Backup (*.json)"),
        windows_sys::w!("*.json"),
        windows_sys::w!("json"),
    )
}

pub fn pick_blocklist_file(owner: isize) -> Option<String> {
    pick_dialog_file(
        owner,
        false,
        None,
        windows_sys::w!("Blocklists (*.p2p; *.txt; *.gz)"),
        windows_sys::w!("*.p2p;*.txt;*.gz"),
        windows_sys::w!("p2p"),
    )
}

pub fn pick_metalink_file(owner: isize) -> Option<String> {
    pick_dialog_file(
        owner,
        false,
        None,
        windows_sys::w!("Metalink files (*.meta4; *.metalink)"),
        windows_sys::w!("*.meta4;*.metalink"),
        windows_sys::w!("meta4"),
    )
}

pub fn pick_torrent_source_file(owner: isize) -> Option<String> {
    pick_dialog_file(
        owner,
        false,
        None,
        windows_sys::w!("All files (*.*)"),
        windows_sys::w!("*.*"),
        windows_sys::w!(""),
    )
}

pub fn pick_save_torrent_file(owner: isize, default_name: &str) -> Option<String> {
    pick_dialog_file(
        owner,
        true,
        Some(default_name),
        windows_sys::w!("Torrent files (*.torrent)"),
        windows_sys::w!("*.torrent"),
        windows_sys::w!("torrent"),
    )
}

pub fn is_fullscreen_app_active() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return false;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 || pid == GetCurrentProcessId() {
            return false;
        }
        let mut class_buf = [0u16; 128];
        let len = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_buf[..len as usize]);
            if matches!(
                class_name.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            ) {
                return false;
            }
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&raw mut rect).cast(),
            size_of::<RECT>() as u32,
        ) < 0
            && GetWindowRect(hwnd, &mut rect) == 0
        {
            return false;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return false;
        }
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return false;
        }
        rect.left <= info.rcMonitor.left
            && rect.top <= info.rcMonitor.top
            && rect.right >= info.rcMonitor.right
            && rect.bottom >= info.rcMonitor.bottom
    }
}

pub fn find_running_watched_process(watched: &[String]) -> Option<String> {
    if watched.is_empty() {
        return None;
    }
    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap == INVALID_HANDLE_VALUE {
        return None;
    }
    let _guard = HandleGuard(snap);

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
    while ok != 0 {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if watched
            .iter()
            .any(|target| !target.is_empty() && name.eq_ignore_ascii_case(target))
        {
            return Some(name);
        }
        ok = unsafe { Process32NextW(snap, &mut entry) };
    }
    None
}

#[cfg(not(test))]
struct ShutdownPrivilegeGuard {
    token: std::os::windows::io::OwnedHandle,
    previous: windows_sys::Win32::Security::TOKEN_PRIVILEGES,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(not(test))]
impl ShutdownPrivilegeGuard {
    fn enable() -> Result<Self, String> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::LUID;
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
            SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
            TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        static PRIVILEGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = PRIVILEGE_LOCK.lock().unwrap();
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            ) == 0
            {
                return Err(format!(
                    "OpenProcessToken failed with error {}",
                    GetLastError()
                ));
            }
            let mut guard = Self {
                token: std::os::windows::io::OwnedHandle::from_raw_handle(token),
                previous: std::mem::zeroed(),
                _lock: lock,
            };
            let mut luid: LUID = std::mem::zeroed();
            if LookupPrivilegeValueW(null(), SE_SHUTDOWN_NAME, &mut luid) == 0 {
                return Err(format!(
                    "LookupPrivilegeValueW failed with error {}",
                    GetLastError()
                ));
            }
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut length = 0;
            SetLastError(ERROR_SUCCESS);
            let ok = AdjustTokenPrivileges(
                token,
                0,
                &privileges,
                size_of::<TOKEN_PRIVILEGES>() as u32,
                &mut guard.previous,
                &mut length,
            );
            let error = GetLastError();
            if ok == 0 || error != ERROR_SUCCESS {
                return Err(format!("AdjustTokenPrivileges failed with error {error}"));
            }
            Ok(guard)
        }
    }
}

#[cfg(not(test))]
impl Drop for ShutdownPrivilegeGuard {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::AdjustTokenPrivileges;
        if self.previous.PrivilegeCount != 0 {
            unsafe {
                AdjustTokenPrivileges(
                    self.token.as_raw_handle(),
                    0,
                    &self.previous,
                    0,
                    null_mut(),
                    null_mut(),
                );
            }
        }
    }
}

pub fn execute_power_action(action: slipstream_protocol::PostQueueAction) -> Result<(), String> {
    #[cfg(test)]
    {
        let _ = action;
        Ok(())
    }
    #[cfg(not(test))]
    match action {
        slipstream_protocol::PostQueueAction::None
        | slipstream_protocol::PostQueueAction::Notify => Ok(()),
        slipstream_protocol::PostQueueAction::Sleep => {
            use windows_sys::Win32::System::Power::SetSuspendState;
            let _privilege = ShutdownPrivilegeGuard::enable()?;
            if unsafe { SetSuspendState(0, 0, 0) } != 0 {
                Ok(())
            } else {
                Err("Windows sleep request failed".into())
            }
        }
        slipstream_protocol::PostQueueAction::Shutdown => {
            use windows_sys::Win32::System::Shutdown::{
                InitiateSystemShutdownExW, SHTDN_REASON_MAJOR_APPLICATION,
            };
            let _privilege = ShutdownPrivilegeGuard::enable()?;
            let result = unsafe {
                InitiateSystemShutdownExW(
                    null(),
                    windows_sys::w!("SlipStream download queue completed"),
                    15,
                    0,
                    0,
                    SHTDN_REASON_MAJOR_APPLICATION,
                )
            };
            if result == 0 {
                return Err(format!(
                    "InitiateSystemShutdownExW failed with error {}",
                    unsafe { GetLastError() }
                ));
            }
            Ok(())
        }
    }
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
    #[test]
    fn active_network_adapters_are_sorted_unique_and_empty_or_unknown_patterns_do_not_match() {
        let names = super::list_active_network_adapters();
        assert!(names.iter().all(|name| !name.is_empty()));
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!super::is_network_adapter_up(""));
        assert!(!super::is_network_adapter_up(
            "__impossible_vpn_adapter_99999__"
        ));
    }

    use super::*;
    use windows_sys::Win32::System::Registry::{
        RegDeleteTreeW, KEY_QUERY_VALUE, REG_CREATED_NEW_KEY, RRF_RT_REG_SZ,
    };

    #[test]
    fn startup_only_changes_slipstream_value_in_isolated_key() {
        const SUBKEY: &str = r"Software\SlipStream\TestStartup";
        let subkey = windows_sys::w!("Software\\SlipStream\\TestStartup");
        let run_value = windows_sys::w!("SlipStream");
        let other_value = windows_sys::w!("OtherApp");
        let mut key = null_mut();
        let mut disposition = 0;
        assert_eq!(
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    subkey,
                    0,
                    null(),
                    0,
                    KEY_SET_VALUE | KEY_QUERY_VALUE,
                    null(),
                    &mut key,
                    &mut disposition,
                )
            },
            ERROR_SUCCESS
        );
        unsafe { RegCloseKey(key) };
        assert_eq!(disposition, REG_CREATED_NEW_KEY);
        struct Cleanup(*const u16);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, self.0) };
            }
        }
        let _cleanup = Cleanup(subkey);
        let other = [u16::from(b'x'), 0];
        assert_eq!(
            unsafe {
                windows_sys::Win32::System::Registry::RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    subkey,
                    other_value,
                    REG_SZ,
                    other.as_ptr().cast(),
                    size_of_val(&other) as u32,
                )
            },
            ERROR_SUCCESS
        );
        let read = |name| {
            let mut buffer = [0u16; 256];
            let mut bytes = size_of_val(&buffer) as u32;
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey,
                    name,
                    RRF_RT_REG_SZ,
                    null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return None;
            }
            assert_eq!(status, ERROR_SUCCESS);
            Some(String::from_utf16(&buffer[..bytes as usize / 2 - 1]).unwrap())
        };
        let exe = Path::new(r"C:\Program Files\SlipStream\SlipStream.exe");
        set_launch_on_startup_at(SUBKEY, exe, true, false).unwrap();
        assert_eq!(read(run_value), Some(format!("\"{}\"", exe.display())));
        set_launch_on_startup_at(SUBKEY, exe, true, true).unwrap();
        assert_eq!(
            read(run_value),
            Some(format!("\"{}\" --minimized", exe.display()))
        );
        set_launch_on_startup_at(SUBKEY, exe, false, true).unwrap();
        set_launch_on_startup_at(SUBKEY, exe, false, false).unwrap();
        assert_eq!(read(run_value), None);
        assert_eq!(read(other_value).as_deref(), Some("x"));
    }

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

    #[test]
    fn watched_process_lookup_finds_non_matching_and_returns_safe_none() {
        assert_eq!(find_running_watched_process(&[]), None);
        assert_eq!(
            find_running_watched_process(&["definitely_missing_process_99999.exe".into()]),
            None
        );
        // Under #[cfg(test)], power actions are strictly safe no-ops.
        assert!(execute_power_action(slipstream_protocol::PostQueueAction::Shutdown).is_ok());
        assert!(execute_power_action(slipstream_protocol::PostQueueAction::Sleep).is_ok());
    }
}
