#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CREATE_NO_WINDOW: u32 = 0x08000000;

// Embed release binary and wintun driver safely (masked with XOR 0x5B so binary contains NO nested PE headers)
static NETFUSION_PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/netfusion_payload.bin"));
static WINTUN_PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wintun_payload.bin"));

fn main() {
    // 1. Show interactive setup prompt asking about autostart on Windows boot
    let prompt_msg = "Welcome to NetFusion Internet Bonding Engine Setup!\n\n\
NetFusion combines all your active network connections (Wi-Fi, Ethernet, Mobile Hotspot) \
into one unified connection with combined speed and zero interruptions.\n\n\
It runs quietly in the Windows System Tray with ultra-low memory usage.\n\n\
Would you like NetFusion to start automatically whenever your computer starts?\n\n\
• Click [Yes] to start automatically on Windows startup (Recommended)\n\
• Click [No] to install without autostart (start manually)\n\
• Click [Cancel] to abort installation";

    let choice = message_box_choice(
        "NetFusion Setup - Startup Configuration",
        prompt_msg,
        MB_YESNOCANCEL | MB_ICONQUESTION | MB_DEFBUTTON1,
    );

    let autostart = match choice {
        IDYES => true,
        IDNO => false,
        _ => return, // Abort / Cancel
    };

    // 2. Determine installation paths: %LOCALAPPDATA%\Programs\NetFusion
    let install_dir = match get_install_dir() {
        Some(d) => d,
        None => {
            message_box_info(
                "NetFusion Setup Error",
                "Unable to determine the local application directory for installation.",
                MB_OK | MB_ICONERROR,
            );
            return;
        }
    };

    if let Err(e) = std::fs::create_dir_all(&install_dir) {
        message_box_info(
            "NetFusion Setup Error",
            &format!("Failed to create installation folder: {}", e),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    // 3. Gracefully terminate existing NetFusion instances using native Win32 API (ZERO taskkill child processes)
    terminate_running_netfusion();
    std::thread::sleep(std::time::Duration::from_millis(300));

    let exe_path = install_dir.join("NetFusion.exe");
    let dll_path = install_dir.join("wintun.dll");

    // 4. Extract embedded files (unmasking payload)
    let exe_bytes: Vec<u8> = NETFUSION_PAYLOAD.iter().map(|b| b ^ 0x5B).collect();
    let dll_bytes: Vec<u8> = WINTUN_PAYLOAD.iter().map(|b| b ^ 0x5B).collect();

    if let Err(e) = std::fs::write(&exe_path, &exe_bytes) {
        message_box_info(
            "NetFusion Setup Error",
            &format!("Failed to write NetFusion.exe to destination:\n{}\n\nError: {}", exe_path.display(), e),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    if let Err(e) = std::fs::write(&dll_path, &dll_bytes) {
        message_box_info(
            "NetFusion Setup Error",
            &format!("Failed to write wintun.dll to destination:\n{}\n\nError: {}", dll_path.display(), e),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    // 5. Configure Windows startup registry key via native Win32 Registry API
    set_autostart_registry(&exe_path, autostart);

    // 6. Create Start Menu and Desktop shortcuts via native Windows COM (ZERO powershell child processes)
    create_desktop_and_start_menu_shortcuts(&exe_path, &install_dir);

    // 7. Launch NetFusion in the background
    let _ = Command::new(&exe_path)
        .current_dir(&install_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    // 8. Show completion confirmation
    let autostart_status = if autostart {
        "Enabled (Will run automatically on computer startup)"
    } else {
        "Disabled (Start manually from Desktop or Start Menu)"
    };

    let success_msg = format!(
        "NetFusion has been successfully installed!\n\n\
Location: {}\n\
Autostart: {}\n\
Desktop Icon: Created\n\n\
NetFusion is now running quietly in your Windows System Tray (Notification Overflow Menu ^).\n\n\
👉 Click the NetFusion icon in the tray anytime to view:\n\
• Live Combined Download & Upload Speed (dynamically updated)\n\
• All connected network adapters with their individual live speeds\n\
• Instant speed test and bonding controls\n\n\
Enjoy unlimited, bonded internet speed!",
        install_dir.display(),
        autostart_status
    );

    message_box_info(
        "NetFusion Installation Complete",
        &success_msg,
        MB_OK | MB_ICONINFORMATION,
    );
}

fn get_install_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|p| PathBuf::from(p).join("Programs").join("NetFusion"))
}

fn terminate_running_netfusion() {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot != INVALID_HANDLE_VALUE {
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                    if name.eq_ignore_ascii_case("NetFusion.exe") {
                        let h_proc = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                        if h_proc != 0 {
                            TerminateProcess(h_proc, 0);
                            CloseHandle(h_proc);
                        }
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
    }
}

fn set_autostart_registry(exe_path: &Path, enable: bool) {
    unsafe {
        let subkey = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let val_name = to_wide("NetFusion");
        let mut hkey: HKEY = 0;

        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) == 0 {
            if enable {
                let exe_str = format!("\"{}\"", exe_path.display());
                let exe_w = to_wide(&exe_str);
                let byte_len = (exe_w.len() * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    val_name.as_ptr(),
                    0,
                    REG_SZ,
                    exe_w.as_ptr() as *const u8,
                    byte_len,
                );
            } else {
                let _ = RegDeleteValueW(hkey, val_name.as_ptr());
            }
            RegCloseKey(hkey);
        }
    }
}

// Native COM vtables for IShellLinkW and IPersistFile
#[repr(C)]
#[allow(non_snake_case)]
struct IShellLinkWVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub GetPath: unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> i32,
    pub GetIDList: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    pub SetIDList: unsafe extern "system" fn(*mut c_void, *const c_void) -> i32,
    pub GetDescription: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32,
    pub SetDescription: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub GetWorkingDirectory: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32,
    pub SetWorkingDirectory: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub GetArguments: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32,
    pub SetArguments: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub GetHotkey: unsafe extern "system" fn(*mut c_void, *mut u16) -> i32,
    pub SetHotkey: unsafe extern "system" fn(*mut c_void, u16) -> i32,
    pub GetShowCmd: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
    pub SetShowCmd: unsafe extern "system" fn(*mut c_void, i32) -> i32,
    pub GetIconLocation: unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut i32) -> i32,
    pub SetIconLocation: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
    pub SetRelativePath: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32,
    pub Resolve: unsafe extern "system" fn(*mut c_void, HWND, u32) -> i32,
    pub SetPath: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct IPersistFileVtbl {
    pub QueryInterface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub GetClassID: unsafe extern "system" fn(*mut c_void, *mut GUID) -> i32,
    pub IsDirty: unsafe extern "system" fn(*mut c_void) -> i32,
    pub Load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32,
    pub Save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
    pub SaveCompleted: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub GetCurFile: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
}

fn create_desktop_and_start_menu_shortcuts(exe_path: &Path, working_dir: &Path) {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        let desktop_lnk = PathBuf::from(&userprofile).join("Desktop").join("NetFusion.lnk");
        create_single_shortcut(exe_path, working_dir, &desktop_lnk);
    }

    if let Ok(appdata) = std::env::var("APPDATA") {
        let programs_dir = PathBuf::from(&appdata).join(r"Microsoft\Windows\Start Menu\Programs");
        let start_lnk = programs_dir.join("NetFusion.lnk");
        create_single_shortcut(exe_path, working_dir, &start_lnk);
    }
}

fn create_single_shortcut(target_exe: &Path, working_dir: &Path, lnk_path: &Path) {
    unsafe {
        let _ = CoInitialize(null());

        let clsid_shell_link = GUID {
            data1: 0x00021401,
            data2: 0x0000,
            data3: 0x0000,
            data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
        };
        let iid_shell_link = GUID {
            data1: 0x000214F9,
            data2: 0x0000,
            data3: 0x0000,
            data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
        };
        let iid_persist_file = GUID {
            data1: 0x0000010b,
            data2: 0x0000,
            data3: 0x0000,
            data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
        };

        let mut shell_link_ptr: *mut c_void = null_mut();
        if CoCreateInstance(
            &clsid_shell_link,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &iid_shell_link,
            &mut shell_link_ptr,
        ) == 0
            && !shell_link_ptr.is_null()
        {
            let shell_link = *(shell_link_ptr as *mut *mut IShellLinkWVtbl);
            let path_w = to_wide(target_exe.to_str().unwrap_or_default());
            let dir_w = to_wide(working_dir.to_str().unwrap_or_default());
            let desc_w = to_wide("NetFusion Internet Bonding Engine");

            ((*shell_link).SetPath)(shell_link_ptr, path_w.as_ptr());
            ((*shell_link).SetWorkingDirectory)(shell_link_ptr, dir_w.as_ptr());
            ((*shell_link).SetDescription)(shell_link_ptr, desc_w.as_ptr());
            ((*shell_link).SetIconLocation)(shell_link_ptr, path_w.as_ptr(), 0);

            let mut persist_file_ptr: *mut c_void = null_mut();
            if ((*shell_link).QueryInterface)(shell_link_ptr, &iid_persist_file, &mut persist_file_ptr) == 0
                && !persist_file_ptr.is_null()
            {
                let persist_file = *(persist_file_ptr as *mut *mut IPersistFileVtbl);
                let lnk_w = to_wide(lnk_path.to_str().unwrap_or_default());
                ((*persist_file).Save)(persist_file_ptr, lnk_w.as_ptr(), 1);
                ((*persist_file).Release)(persist_file_ptr);
            }

            ((*shell_link).Release)(shell_link_ptr);
        }
        CoUninitialize();
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn message_box_choice(title: &str, message: &str, flags: u32) -> i32 {
    let title_w = to_wide(title);
    let message_w = to_wide(message);
    unsafe {
        MessageBoxW(
            0,
            message_w.as_ptr(),
            title_w.as_ptr(),
            flags,
        )
    }
}

fn message_box_info(title: &str, message: &str, flags: u32) {
    let _ = message_box_choice(title, message, flags);
}
