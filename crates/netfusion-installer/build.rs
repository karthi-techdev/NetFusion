fn main() {
    #[cfg(windows)]
    {
        let icon_path = std::path::Path::new("../../assets/icon.ico");
        if icon_path.exists() {
            let out_dir = std::env::var("OUT_DIR").unwrap();
            let res_path = format!("{}/app_icon.o", out_dir);
            let rc_file = format!("{}/app_icon.rc", out_dir);
            let rc_content = "1 ICON \"../../assets/icon.ico\"\n";
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
    }
}
