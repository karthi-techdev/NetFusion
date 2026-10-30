use log::{error, info, warn};
use std::net::Ipv4Addr;
use std::process::Command;

#[cfg(target_os = "linux")]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

pub struct TunDevice {
    pub name: String,
    pub ip: Ipv4Addr,
    #[cfg(target_os = "linux")]
    pub async_fd: tokio::io::unix::AsyncFd<std::fs::File>,
    #[cfg(not(target_os = "linux"))]
    pub dev_index: u32,
}

impl TunDevice {
    #[cfg(target_os = "linux")]
    pub fn create(name: &str, ip: Ipv4Addr, mtu: u16) -> Result<Self, String> {
        info!("Creating Linux TUN device: {} with IP: {} (MTU: {})", name, ip, mtu);

        use std::fs::OpenOptions;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|e| format!("Failed to open /dev/net/tun: {}", e))?;

        let raw_fd = file.as_raw_fd();

        // IFF_TUN = 0x0001, IFF_NO_PI = 0x1000
        #[repr(C)]
        struct IfReq {
            ifr_name: [u8; 16],
            ifr_flags: i16,
            _pad: [u8; 22],
        }

        let mut ifr = IfReq {
            ifr_name: [0; 16],
            ifr_flags: 0x0001 | 0x1000,
            _pad: [0; 22],
        };

        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(15);
        ifr.ifr_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // TUNSETIFF ioctl = 0x400454ca
        let ret = unsafe { libc::ioctl(raw_fd, 0x400454ca, &ifr) };
        if ret < 0 {
            return Err(format!("ioctl TUNSETIFF failed: {}", std::io::Error::last_os_error()));
        }

        // Set nonblocking
        unsafe {
            let flags = libc::fcntl(raw_fd, libc::F_GETFL);
            libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // Configure interface IP and bring UP via `ip` command
        let _ = Command::new("ip").args(["addr", "add", &format!("{}/24", ip), "dev", name]).output();
        let _ = Command::new("ip").args(["link", "set", "mtu", &mtu.to_string(), "up", "dev", name]).output();

        let async_fd = tokio::io::unix::AsyncFd::new(file)
            .map_err(|e| format!("Failed to create AsyncFd for TUN: {}", e))?;

        Ok(Self {
            name: name.to_string(),
            ip,
            async_fd,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn create(name: &str, ip: Ipv4Addr, _mtu: u16) -> Result<Self, String> {
        info!("Running on non-Linux host: Initializing simulated TUN device for {}", name);
        Ok(Self {
            name: name.to_string(),
            ip,
            dev_index: 1,
        })
    }

    #[cfg(target_os = "linux")]
    pub async fn read_packet(&self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        loop {
            let mut guard = self.async_fd.readable().await?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let ret = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if ret < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub async fn write_packet(&self, buf: &[u8]) -> Result<usize, std::io::Error> {
        loop {
            let mut guard = self.async_fd.writable().await?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let ret = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
                if ret < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}
