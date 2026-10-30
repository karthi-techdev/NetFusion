#![windows_subsystem = "windows"]

use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CREATE_NO_WINDOW: u32 = 0x08000000;

// Embed release binary and wintun driver directly into the installer
static NETFUSION_EXE: &[u8] = include_bytes!("../../../target/release/NetFusion.exe");
static WINTUN_DLL: &[u8] = include_bytes!("../../../wintun.dll");

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

    // 3. Terminate running instance if updating (silent, NO console window)
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "NetFusion.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let exe_path = install_dir.join("NetFusion.exe");
    let dll_path = install_dir.join("wintun.dll");

    // 4. Extract embedded files
    if let Err(e) = std::fs::write(&exe_path, NETFUSION_EXE) {
        message_box_info(
            "NetFusion Setup Error",
            &format!("Failed to write NetFusion.exe to destination:\n{}\n\nError: {}", exe_path.display(), e),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    if let Err(e) = std::fs::write(&dll_path, WINTUN_DLL) {
        message_box_info(
            "NetFusion Setup Error",
            &format!("Failed to write wintun.dll to destination:\n{}\n\nError: {}", dll_path.display(), e),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    // 5. Configure Windows startup registry key via native Win32 API (ZERO cmd popups)
    set_autostart_registry(&exe_path, autostart);

    // 6. Create Start Menu and Desktop shortcuts (silent, with custom app icon)
    create_shortcuts(&exe_path, &install_dir);

    // 7. Launch NetFusion in the background (silent, NO console window)
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

fn create_shortcuts(exe_path: &Path, working_dir: &Path) {
    let exe_str = exe_path.to_string_lossy().to_string();
    let dir_str = working_dir.to_string_lossy().to_string();

    // PowerShell script run with CREATE_NO_WINDOW so absolutely zero console pops up
    let script = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $dt = [Environment]::GetFolderPath('Desktop') + '\\NetFusion.lnk'; \
         $s1 = $ws.CreateShortcut($dt); \
         $s1.TargetPath = '{0}'; \
         $s1.WorkingDirectory = '{1}'; \
         $s1.IconLocation = '{0},0'; \
         $s1.Description = 'NetFusion Internet Bonding Engine'; \
         $s1.Save(); \
         $sm = [Environment]::GetFolderPath('Programs') + '\\NetFusion.lnk'; \
         $s2 = $ws.CreateShortcut($sm); \
         $s2.TargetPath = '{0}'; \
         $s2.WorkingDirectory = '{1}'; \
         $s2.IconLocation = '{0},0'; \
         $s2.Description = 'NetFusion Internet Bonding Engine'; \
         $s2.Save()",
        exe_str, dir_str
    );

    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
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
