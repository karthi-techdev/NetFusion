use clap::{Parser, Subcommand};
use colored::*;
use netfusion_core::metrics::EngineMetricsSnapshot;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(author, version, about = "NetFusion Bonding Engine Diagnostics & Benchmark Tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Monitor real-time bonding status and ISP throughput
    Status {
        #[arg(short, long, default_value = "127.0.0.1:53921")]
        ipc_addr: String,
    },
    /// Run single-session TCP download speed benchmark
    Benchmark {
        #[arg(short, long, default_value = "https://speed.cloudflare.com/__down?bytes=50000000")]
        url: String,
    },
    /// Inspect local network adapters and ISP gateway assignments
    Interfaces,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Interfaces => {
            println!("{}", "═══════════════════════════════════════════════════════════".cyan());
            println!("{}", "         NetFusion Network Interface Discovery             ".bold().cyan());
            println!("{}", "═══════════════════════════════════════════════════════════".cyan());

            #[cfg(windows)]
            {
                // Discover interfaces directly using netsh / powershell wrapper or native IP helper
                let output = std::process::Command::new("powershell")
                    .args(["-Command", "Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.InterfaceAlias -ne 'Loopback Pseudo-Interface 1' } | Select-Object InterfaceAlias, IPAddress, InterfaceIndex"])
                    .output()?;
                println!("{}", String::from_utf8_lossy(&output.stdout));
            }
        }
        Commands::Status { ipc_addr } => {
            println!("{}", "Connecting to NetFusion Engine IPC...".yellow());
            match TcpStream::connect(&ipc_addr) {
                Ok(stream) => {
                    println!("{}", "Connected! Streaming live bonding telemetry (Ctrl+C to quit)...".green());
                    let reader = BufReader::new(stream);
                    for line in reader.lines() {
                        if let Ok(l) = line {
                            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&l) {
                                if let Some(status_val) = map.get("Status") {
                                    if let Ok(snapshot) = serde_json::from_value::<EngineMetricsSnapshot>(status_val.clone()) {
                                        print!("\x1B[2J\x1B[1;1H"); // Clear screen
                                        print_dashboard(&snapshot);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}: {}", "Failed to connect to NetFusion engine".red(), e);
                    eprintln!("Ensure netfusion-client service is running on {}", ipc_addr);
                }
            }
        }
        Commands::Benchmark { url } => {
            println!("{}", "═══════════════════════════════════════════════════════════".green());
            println!("{}", "         NetFusion Single-Session Download Benchmark       ".bold().green());
            println!("{}", "═══════════════════════════════════════════════════════════".green());
            println!("Target URL: {}", url.cyan());
            println!("Testing aggregated single TCP session throughput...");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?;

            let start = Instant::now();
            let mut response = client.get(&url).send().await?;

            let mut total_bytes = 0u64;
            while let Some(chunk) = response.chunk().await? {
                total_bytes += chunk.len() as u64;
                let elapsed = start.elapsed().as_secs_f64();
                if elapsed > 0.0 {
                    let mbps = (total_bytes as f64 * 8.0) / (elapsed * 1_000_000.0);
                    print!("\rDownloaded: {:.2} MB | Speed: {:.2} Mbps", total_bytes as f64 / 1_000_000.0, mbps);
                    let _ = std::io::stdout().flush();
                }
            }

            let total_sec = start.elapsed().as_secs_f64();
            let avg_mbps = (total_bytes as f64 * 8.0) / (total_sec * 1_000_000.0);

            println!("\n\n{}", "Benchmark Results:".bold());
            println!("Total Transferred : {:.2} MB", total_bytes as f64 / 1_000_000.0);
            println!("Duration          : {:.2} seconds", total_sec);
            println!("Bonded Throughput : {} Mbps", format!("{:.2}", avg_mbps).bold().green());
        }
    }

    Ok(())
}

fn print_dashboard(s: &EngineMetricsSnapshot) {
    println!("{}", "═══════════════════════════════════════════════════════════".cyan());
    println!("{}", "         NetFusion Bonding Engine - Live Telemetry          ".bold().cyan());
    println!("{}", "═══════════════════════════════════════════════════════════".cyan());

    let status_str = if s.active_bonding { "ACTIVE (BONDED)".green() } else { "IDLE".yellow() };
    println!("Status        : {}", status_str);
    println!("Combined RX   : {} Mbps", format!("{:.2}", s.total_rx_bps / 1_000_000.0).bold().green());
    println!("Combined TX   : {} Mbps", format!("{:.2}", s.total_tx_bps / 1_000_000.0).bold().blue());
    println!("CPU Usage     : {:.1} %", s.cpu_usage_percent);
    println!("Memory (RAM)  : {:.1} MB", s.memory_rss_mb);
    println!("{}", "───────────────────────────────────────────────────────────".cyan());
    println!("Active WAN Uplinks:");

    for p in &s.paths {
        let state = if p.is_alive { "UP".green() } else { "DOWN".red() };
        println!(
            " [{}] {:<18} | IP: {:<15} | RTT: {:>5.1} ms | Loss: {:>4.1}% | Share: {:>4.1}%",
            state, p.name, p.local_ip, p.rtt_ms, p.loss_percent, p.share_percent
        );
    }
    println!("{}", "═══════════════════════════════════════════════════════════".cyan());
}
