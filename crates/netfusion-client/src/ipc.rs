use log::{info, warn};
use netfusion_core::metrics::EngineMetricsSnapshot;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcCommand {
    StartBonding,
    StopBonding,
    SetBondingMode(String),
    SetInterfaceWeight { name: String, weight: f32 },
    GetStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    Status(EngineMetricsSnapshot),
    CommandAck { success: bool, message: String },
}

pub struct IpcServer {
    port: u16,
    telemetry_tx: broadcast::Sender<EngineMetricsSnapshot>,
    command_tx: mpsc::Sender<IpcCommand>,
}

impl IpcServer {
    pub fn new(
        port: u16,
        telemetry_tx: broadcast::Sender<EngineMetricsSnapshot>,
        command_tx: mpsc::Sender<IpcCommand>,
    ) -> Self {
        Self {
            port,
            telemetry_tx,
            command_tx,
        }
    }

    pub async fn run(self) -> Result<(), String> {
        let addr = format!("127.0.0.1:{}", self.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("Failed to bind IPC listener to {}: {}", addr, e))?;

        info!("NetFusion IPC Server listening on {}", addr);

        loop {
            match listener.accept().await {
                Ok((stream, _peer_addr)) => {
                    let mut rx_telemetry = self.telemetry_tx.subscribe();
                    let cmd_tx = self.command_tx.clone();

                    tokio::spawn(async move {
                        let (read_half, mut write_half) = stream.into_split();
                        let mut reader = BufReader::new(read_half);
                        let mut line = String::new();

                        loop {
                            tokio::select! {
                                // Send telemetry updates to connected GUI client
                                Ok(snapshot) = rx_telemetry.recv() => {
                                    if let Ok(json) = serde_json::to_string(&IpcResponse::Status(snapshot)) {
                                        if write_half.write_all(json.as_bytes()).await.is_err() ||
                                           write_half.write_all(b"\n").await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                // Read incoming commands from GUI client
                                result = reader.read_line(&mut line) => {
                                    match result {
                                        Ok(0) => break, // EOF
                                        Ok(_) => {
                                            if let Ok(cmd) = serde_json::from_str::<IpcCommand>(line.trim()) {
                                                let _ = cmd_tx.send(cmd).await;
                                            }
                                            line.clear();
                                        }
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!("IPC accept error: {}", e);
                }
            }
        }
    }
}
