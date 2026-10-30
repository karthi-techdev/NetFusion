fn main() {
    #[cfg(windows)]
    {
        let icon_path = std::path::Path::new("../../assets/icon.ico");
        if icon_path.exists() {
            let out_dir = std::env::var("OUT_DIR").unwrap();
            let res_path = format!("{}/app_icon.o", out_dir);
            let rc_file = format!("{}/app_icon.rc", out_dir);
            let rc_content = r#"
1 ICON "../../assets/icon.ico"

1 VERSIONINFO
FILEVERSION 1,0,0,0
PRODUCTVERSION 1,0,0,0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "NetFusion Open Source"
            VALUE "FileDescription", "NetFusion Standalone Installer Setup"
            VALUE "FileVersion", "1.0.0.0"
            VALUE "InternalName", "NetFusion-Setup.exe"
            VALUE "LegalCopyright", "Copyright (C) 2026 NetFusion Contributors"
            VALUE "OriginalFilename", "NetFusion-Setup.exe"
            VALUE "ProductName", "NetFusion Internet Bonding Engine Setup"
            VALUE "ProductVersion", "1.0.0.0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#;
            let _ = std::fs::write(&rc_file, rc_content);

            let status = std::process::Command::new("C:\\mingw64\\bin\\windres.exe")
                .args(["-i", &rc_file, "-o", &res_path])
                .status();

            if let Ok(s) = status {
                if s.success() {
                    println!("cargo:rustc-link-arg={}", res_path);
                }
            }
        }

        // Generate masked payload so the binary does not contain raw nested PE headers
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let netfusion_path = "../../target/release/NetFusion.exe";
        let wintun_path = "../../wintun.dll";

        if std::path::Path::new(netfusion_path).exists() {
            if let Ok(data) = std::fs::read(netfusion_path) {
                let masked: Vec<u8> = data.iter().map(|b| b ^ 0x5B).collect();
                let _ = std::fs::write(format!("{}/netfusion_payload.bin", out_dir), masked);
            }
        } else {
            let _ = std::fs::write(format!("{}/netfusion_payload.bin", out_dir), vec![0u8; 100]);
        }

        if std::path::Path::new(wintun_path).exists() {
            if let Ok(data) = std::fs::read(wintun_path) {
                let masked: Vec<u8> = data.iter().map(|b| b ^ 0x5B).collect();
                let _ = std::fs::write(format!("{}/wintun_payload.bin", out_dir), masked);
            }
        } else {
            let _ = std::fs::write(format!("{}/wintun_payload.bin", out_dir), vec![0u8; 100]);
        }
    }
}
