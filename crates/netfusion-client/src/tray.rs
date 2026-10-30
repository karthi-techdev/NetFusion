use log::{error, info};
use netfusion_core::metrics::EngineMetricsSnapshot;
use parking_lot::RwLock;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Embedded NetFusion high-resolution multi-size app icon
static ICON_BYTES: &[u8] = include_bytes!("../../../assets/icon.ico");

const WM_TRAY_CALLBACK: u32 = WM_APP + 101;
const ID_TRAY_ICON: u32 = 1001;
const TIMER_MENU_REFRESH: usize = 999;

// Menu Command IDs
const ID_HEADER_INFO: usize = 2000;
const ID_COMBINED_SPEED: usize = 2001;
const ID_SEP_1: usize = 2002;
// 2010 .. 2030 reserved for dynamic adapter speed rows
const ID_ADAPTER_BASE: usize = 2010;
const ID_SEP_2: usize = 2040;
const ID_TOGGLE_BONDING: usize = 2041;
const ID_RUN_SPEED_TEST: usize = 2042;
const ID_TOGGLE_AUTOSTART: usize = 2043;
const ID_SEP_3: usize = 2044;
const ID_EXIT: usize = 2045;

static TRAY_CONTEXT: std::sync::OnceLock<TrayContext> = std::sync::OnceLock::new();
static ACTIVE_POPUP_MENU: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct TrayContext {
    metrics: Arc<RwLock<EngineMetricsSnapshot>>,
    bonding_active: Arc<AtomicBool>,
    test_trigger: Arc<AtomicBool>,
    exit_requested: Arc<AtomicBool>,
    _hwnd: HWND,
}

pub struct SystemTrayApp {
    metrics: Arc<RwLock<EngineMetricsSnapshot>>,
    bonding_active: Arc<AtomicBool>,
    test_trigger: Arc<AtomicBool>,
    exit_requested: Arc<AtomicBool>,
}

impl SystemTrayApp {
    pub fn new(
        metrics: Arc<RwLock<EngineMetricsSnapshot>>,
        bonding_active: Arc<AtomicBool>,
        test_trigger: Arc<AtomicBool>,
        exit_requested: Arc<AtomicBool>,
    ) -> Self {
        Self {
            metrics,
            bonding_active,
            test_trigger,
            exit_requested,
        }
    }

    pub fn run_loop(self) {
        unsafe {
            let class_name = to_wide_chars("NetFusionSystemTrayClass");
            let window_name = to_wide_chars("NetFusionTray");
            let hinstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
            let app_icon = load_custom_tray_icon();

            let wnd_class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(tray_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: app_icon,
                hCursor: LoadCursorW(0, IDC_ARROW),
                hbrBackground: 0,
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
            };

            RegisterClassW(&wnd_class);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                0, // Top-level unshown window
                0,
                hinstance,
                null(),
            );

            if hwnd == 0 {
                error!("Failed to create background message window for System Tray");
                return;
            }

            // Register Tray Icon with custom NetFusion icon
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = ID_TRAY_ICON;
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_TRAY_CALLBACK;
            nid.hIcon = app_icon;

            let tip = "NetFusion: Active (Merging Connections)";
            copy_str_to_buf(&mut nid.szTip, tip);

            Shell_NotifyIconW(NIM_ADD, &nid);

            let _ = TRAY_CONTEXT.set(TrayContext {
                metrics: self.metrics.clone(),
                bonding_active: self.bonding_active.clone(),
                test_trigger: self.test_trigger.clone(),
                exit_requested: self.exit_requested.clone(),
                _hwnd: hwnd,
            });

            info!("NetFusion System Tray active with custom icon in Windows Notification Area");

            // Background thread to periodically update tooltip with live throughput
            let metrics_clone = self.metrics.clone();
            let bonding_active_clone = self.bonding_active.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    let is_active = bonding_active_clone.load(Ordering::Relaxed);
                    let (comb_rx, path_count) = {
                        let snap = metrics_clone.read();
                        (snap.total_rx_bps / 1_000_000.0, snap.paths.len())
                    };

                    let tip_text = if is_active {
                        format!("NetFusion: {:.1} Mbps ({} links active)", comb_rx, path_count)
                    } else {
                        "NetFusion: Merging Paused".to_string()
                    };

                    let mut update_nid: NOTIFYICONDATAW = std::mem::zeroed();
                    update_nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                    update_nid.hWnd = hwnd;
                    update_nid.uID = ID_TRAY_ICON;
                    update_nid.uFlags = NIF_TIP;
                    copy_str_to_buf(&mut update_nid.szTip, &tip_text);
                    Shell_NotifyIconW(NIM_MODIFY, &update_nid);
                }
            });

            // Windows message pump
            let mut msg: MSG = std::mem::zeroed();
            loop {
                let res = GetMessageW(&mut msg, 0, 0, 0);
                if res <= 0 {
                    info!("Tray message pump exited with return value: {}", res);
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Cleanup tray icon on exit
            let mut del_nid: NOTIFYICONDATAW = std::mem::zeroed();
            del_nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            del_nid.hWnd = hwnd;
            del_nid.uID = ID_TRAY_ICON;
            Shell_NotifyIconW(NIM_DELETE, &del_nid);
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_CALLBACK => {
            let event = lparam as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_tray_popup_menu(hwnd);
            }
            0
        }
        WM_TIMER => {
            if wparam == TIMER_MENU_REFRESH {
                let hmenu = ACTIVE_POPUP_MENU.load(Ordering::Acquire) as HMENU;
                if hmenu != 0 {
                    if let Some(ctx) = TRAY_CONTEXT.get() {
                        let snap = ctx.metrics.read().clone();
                        let is_active = ctx.bonding_active.load(Ordering::Relaxed);

                        // 1. Live status header
                        let status_str = if is_active {
                            format!("⚡ NetFusion Engine: ACTIVE (Merging {} Connections)", snap.paths.len())
                        } else {
                            "⚡ NetFusion Engine: PAUSED".to_string()
                        };
                        let wide_status = to_wide_chars(&status_str);
                        ModifyMenuW(
                            hmenu,
                            ID_HEADER_INFO as u32,
                            MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                            ID_HEADER_INFO,
                            wide_status.as_ptr(),
                        );

                        // 2. Combined aggregate speed
                        let comb_rx = snap.total_rx_bps / 1_000_000.0;
                        let comb_tx = snap.total_tx_bps / 1_000_000.0;
                        let combined_str = format!("📶 Combined Speed: {:.2} Mbps  (TX: {:.2} Mbps)", comb_rx, comb_tx);
                        let wide_comb = to_wide_chars(&combined_str);
                        ModifyMenuW(
                            hmenu,
                            ID_COMBINED_SPEED as u32,
                            MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                            ID_COMBINED_SPEED,
                            wide_comb.as_ptr(),
                        );

                        // 3. Update active adapter rows (displaying friendly Wi-Fi / network names without raw IPs)
                        for (i, p) in snap.paths.iter().enumerate() {
                            let row_id = ID_ADAPTER_BASE + i;
                            let rx_mbps = p.rx_bps / 1_000_000.0;
                            let tx_mbps = p.tx_bps / 1_000_000.0;
                            let display_title = if !p.network_name.is_empty() && p.network_name != p.name {
                                format!("[{}] {}", p.name, p.network_name)
                            } else {
                                format!("[{}]", p.name)
                            };
                            let adapter_line = format!(
                                "  🟢 {}: {:.2} Mbps  (TX: {:.2} Mbps)",
                                display_title, rx_mbps, tx_mbps
                            );
                            let wide_line = to_wide_chars(&adapter_line);
                            ModifyMenuW(
                                hmenu,
                                row_id as u32,
                                MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                                row_id,
                                wide_line.as_ptr(),
                            );
                        }

                        // 4. Instantly remove any disconnected adapter rows so they stop showing in the list!
                        for i in snap.paths.len()..16 {
                            let row_id = (ID_ADAPTER_BASE + i) as u32;
                            DeleteMenu(hmenu, row_id, MF_BYCOMMAND);
                        }
                    }
                }
            }
            0
        }
        WM_COMMAND => {
            let cmd_id = (wparam & 0xFFFF) as usize;
            handle_menu_command(cmd_id);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_tray_popup_menu(hwnd: HWND) {
    let ctx = match TRAY_CONTEXT.get() {
        Some(c) => c,
        None => return,
    };

    let snap = ctx.metrics.read().clone();
    let is_active = ctx.bonding_active.load(Ordering::Relaxed);

    let mut cursor_pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut cursor_pt);

    let hmenu = CreatePopupMenu();
    if hmenu == 0 {
        return;
    }

    // 1. Header Item
    let status_str = if is_active {
        format!("⚡ NetFusion Engine: ACTIVE (Merging {} Connections)", snap.paths.len())
    } else {
        "⚡ NetFusion Engine: PAUSED".to_string()
    };
    append_menu_item(hmenu, MF_GRAYED | MF_DISABLED, ID_HEADER_INFO, &status_str);

    // 2. Combined Aggregate Speed
    let comb_rx = snap.total_rx_bps / 1_000_000.0;
    let comb_tx = snap.total_tx_bps / 1_000_000.0;
    let combined_str = format!("📶 Combined Speed: {:.2} Mbps  (TX: {:.2} Mbps)", comb_rx, comb_tx);
    append_menu_item(hmenu, MF_GRAYED | MF_DISABLED, ID_COMBINED_SPEED, &combined_str);

    AppendMenuW(hmenu, MF_SEPARATOR, ID_SEP_1, null());

    // 3. Dynamic Adapter Speed Entries
    if snap.paths.is_empty() {
        append_menu_item(hmenu, MF_GRAYED | MF_DISABLED, ID_ADAPTER_BASE, "  Scanning for network adapters...");
    } else {
        for (i, p) in snap.paths.iter().enumerate() {
            let row_id = ID_ADAPTER_BASE + i;
            let rx_mbps = p.rx_bps / 1_000_000.0;
            let tx_mbps = p.tx_bps / 1_000_000.0;
            let display_title = if !p.network_name.is_empty() && p.network_name != p.name {
                format!("[{}] {}", p.name, p.network_name)
            } else {
                format!("[{}]", p.name)
            };
            let adapter_line = format!(
                "  🟢 {}: {:.2} Mbps  (TX: {:.2} Mbps)",
                display_title, rx_mbps, tx_mbps
            );
            append_menu_item(hmenu, MF_GRAYED | MF_DISABLED, row_id, &adapter_line);
        }
    }

    AppendMenuW(hmenu, MF_SEPARATOR, ID_SEP_2, null());

    // 4. Pause / Resume Toggle
    let toggle_text = if is_active { "⏸ Pause Merging" } else { "▶ Resume Merging" };
    append_menu_item(hmenu, MF_STRING, ID_TOGGLE_BONDING, toggle_text);

    // 5. Test Combined Speed Now
    append_menu_item(hmenu, MF_STRING, ID_RUN_SPEED_TEST, "🚀 Test Combined Speed Now (5s)");

    // 6. Start Automatically with Windows (Query directly via Win32 Registry, ZERO cmd popup)
    let autostart_on = is_autostart_enabled();
    let autostart_flags = if autostart_on { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED };
    append_menu_item(hmenu, autostart_flags, ID_TOGGLE_AUTOSTART, "✔ Start Automatically with Windows");

    AppendMenuW(hmenu, MF_SEPARATOR, ID_SEP_3, null());

    // 7. Exit
    append_menu_item(hmenu, MF_STRING, ID_EXIT, "❌ Exit NetFusion");

    // Enable live dynamic 1-sec menu updates while menu is visible
    ACTIVE_POPUP_MENU.store(hmenu as usize, Ordering::Release);
    SetTimer(hwnd, TIMER_MENU_REFRESH, 1000, None);

    // Required for Windows tray menus to dismiss properly when clicking outside
    SetForegroundWindow(hwnd);
    TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        cursor_pt.x,
        cursor_pt.y,
        0,
        hwnd,
        null(),
    );

    KillTimer(hwnd, TIMER_MENU_REFRESH);
    ACTIVE_POPUP_MENU.store(0, Ordering::Release);
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(hmenu);
}

unsafe fn append_menu_item(hmenu: HMENU, flags: u32, id: usize, text: &str) {
    let wide = to_wide_chars(text);
    AppendMenuW(hmenu, flags, id, wide.as_ptr());
}

fn handle_menu_command(cmd_id: usize) {
    let ctx = match TRAY_CONTEXT.get() {
        Some(c) => c,
        None => return,
    };

    match cmd_id {
        ID_TOGGLE_BONDING => {
            let prev = ctx.bonding_active.load(Ordering::Relaxed);
            let new_val = !prev;
            ctx.bonding_active.store(new_val, Ordering::Relaxed);
            info!("Bonding state toggled: {}", if new_val { "ACTIVE" } else { "PAUSED" });
            if new_val {
                crate::proxy::enable_windows_proxy(8080);
            } else {
                crate::proxy::disable_windows_proxy();
            }
        }
        ID_RUN_SPEED_TEST => {
            info!("Triggering live multi-WAN speed test across all active interfaces...");
            ctx.test_trigger.store(true, Ordering::Relaxed);
            std::thread::spawn(|| {
                // Run 8 concurrent streams through local bonding dispatcher 127.0.0.1:8080.
                // Each stream alternates across all active WAN links (Ethernet and WiFi) to saturate both simultaneously.
                use std::io::{Read, Write};
                use std::net::TcpStream;
                let targets = [
                    ("proof.ovh.net", "/files/10Mb.dat"),
                    ("speedtest.tele2.net", "/10MB.zip"),
                    ("proof.ovh.net", "/files/10Mb.dat"),
                    ("speedtest.tele2.net", "/10MB.zip"),
                    ("proof.ovh.net", "/files/10Mb.dat"),
                    ("speedtest.tele2.net", "/10MB.zip"),
                    ("proof.ovh.net", "/files/10Mb.dat"),
                    ("speedtest.tele2.net", "/10MB.zip"),
                ];
                let mut handles = Vec::new();
                for &(host, path) in &targets {
                    let h = std::thread::spawn(move || {
                        if let Ok(mut stream) = TcpStream::connect("127.0.0.1:8080") {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                            let req = format!(
                                "GET http://{}{} HTTP/1.1\r\nHost: {}\r\nUser-Agent: NetFusion-BondingEngine\r\nConnection: close\r\n\r\n",
                                host, path, host
                            );
                            let _ = stream.write_all(req.as_bytes());
                            let mut buf = [0u8; 65536];
                            while let Ok(n) = stream.read(&mut buf) {
                                if n == 0 { break; }
                            }
                        }
                    });
                    handles.push(h);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                for h in handles {
                    let _ = h.join();
                }
            });
        }
        ID_TOGGLE_AUTOSTART => {
            let currently_enabled = is_autostart_enabled();
            set_autostart(!currently_enabled);
        }
        ID_EXIT => {
            info!("Exit requested from System Tray. Shutting down NetFusion cleanly...");
            ctx.exit_requested.store(true, Ordering::Relaxed);
            crate::proxy::disable_windows_proxy();
            unsafe {
                PostQuitMessage(0);
            }
        }
        _ => {}
    }
}

pub fn is_autostart_enabled() -> bool {
    unsafe {
        let subkey = to_wide_chars(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let val_name = to_wide_chars("NetFusion");
        let mut hkey: HKEY = 0;

        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) == 0 {
            let mut val_type: u32 = 0;
            let mut val_len: u32 = 0;
            let res = RegQueryValueExW(
                hkey,
                val_name.as_ptr(),
                null(),
                &mut val_type,
                null_mut(),
                &mut val_len,
            );
            RegCloseKey(hkey);
            res == 0
        } else {
            false
        }
    }
}

pub fn set_autostart(enable: bool) {
    unsafe {
        let subkey = to_wide_chars(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let val_name = to_wide_chars("NetFusion");
        let mut hkey: HKEY = 0;

        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) == 0 {
            if enable {
                if let Ok(current_exe) = std::env::current_exe() {
                    let exe_str = format!("\"{}\"", current_exe.display());
                    let exe_w = to_wide_chars(&exe_str);
                    let byte_len = (exe_w.len() * 2) as u32;
                    let _ = RegSetValueExW(
                        hkey,
                        val_name.as_ptr(),
                        0,
                        REG_SZ,
                        exe_w.as_ptr() as *const u8,
                        byte_len,
                    );
                }
            } else {
                let _ = RegDeleteValueW(hkey, val_name.as_ptr());
            }
            RegCloseKey(hkey);
        }
    }
}

unsafe fn load_custom_tray_icon() -> HICON {
    // 1. Try to load from PE resources (if linked via windres with ID 1)
    let hinstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
    let hicon_res = LoadIconW(hinstance, 1 as _);
    if hicon_res != 0 {
        return hicon_res;
    }

    // 2. Parse embedded icon.ico bytes directly
    if ICON_BYTES.len() > 22 {
        let count = u16::from_le_bytes([ICON_BYTES[4], ICON_BYTES[5]]) as usize;
        let mut best_offset = 0;
        let mut best_size = 0;
        for i in 0..count {
            let entry_offset = 6 + i * 16;
            if entry_offset + 16 <= ICON_BYTES.len() {
                let width = ICON_BYTES[entry_offset];
                let size = u32::from_le_bytes([
                    ICON_BYTES[entry_offset + 8],
                    ICON_BYTES[entry_offset + 9],
                    ICON_BYTES[entry_offset + 10],
                    ICON_BYTES[entry_offset + 11],
                ]) as usize;
                let offset = u32::from_le_bytes([
                    ICON_BYTES[entry_offset + 12],
                    ICON_BYTES[entry_offset + 13],
                    ICON_BYTES[entry_offset + 14],
                    ICON_BYTES[entry_offset + 15],
                ]) as usize;

                if width == 32 || width == 0 {
                    best_offset = offset;
                    best_size = size;
                    break;
                } else if best_offset == 0 {
                    best_offset = offset;
                    best_size = size;
                }
            }
        }

        if best_offset > 0 && best_offset + best_size <= ICON_BYTES.len() {
            let hicon = CreateIconFromResourceEx(
                ICON_BYTES[best_offset..].as_ptr(),
                best_size as u32,
                1,
                0x00030000,
                32,
                32,
                LR_DEFAULTCOLOR,
            );
            if hicon != 0 {
                return hicon;
            }
        }
    }

    LoadIconW(0, IDI_APPLICATION)
}

fn to_wide_chars(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn copy_str_to_buf(buf: &mut [u16], s: &str) {
    let wide = to_wide_chars(s);
    let len = wide.len().min(buf.len());
    buf[..len].copy_from_slice(&wide[..len]);
}
