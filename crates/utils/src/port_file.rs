use std::{env, path::PathBuf, sync::OnceLock};

use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PortInfo {
    pub main_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_proxy_port: Option<u16>,
}

/// The ports of the server running in this process, once it has bound them.
///
/// In-process readers use this rather than the file: the file lives in a
/// shared temp dir, so a temp cleaner can delete it and a second server on the
/// same machine (a dev stack) overwrites it with its own ports.
static OWN_PORTS: OnceLock<PortInfo> = OnceLock::new();

pub async fn write_port_file_with_proxy(
    main_port: u16,
    preview_proxy_port: Option<u16>,
) -> std::io::Result<PathBuf> {
    let _ = OWN_PORTS.set(PortInfo {
        main_port,
        preview_proxy_port,
    });
    let dir = env::temp_dir().join("bettercoding");
    let path = dir.join("bettercoding.port");
    let port_info = PortInfo {
        main_port,
        preview_proxy_port,
    };
    let content = serde_json::to_string(&port_info)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    tracing::debug!("Writing ports {:?} to {:?}", port_info, path);
    fs::create_dir_all(&dir).await?;
    fs::write(&path, content).await?;
    Ok(path)
}

pub async fn read_port_file(app_name: &str) -> std::io::Result<u16> {
    read_port_info(app_name).await.map(|info| info.main_port)
}

pub async fn read_port_info(app_name: &str) -> std::io::Result<PortInfo> {
    if let Some(info) = OWN_PORTS.get() {
        return Ok(*info);
    }
    let dir = env::temp_dir().join(app_name);
    let path = dir.join(format!("{app_name}.port"));
    tracing::debug!("Reading port from {:?}", path);

    let content = fs::read_to_string(&path).await?;

    if let Ok(port_info) = serde_json::from_str::<PortInfo>(&content) {
        return Ok(port_info);
    }

    let port: u16 = content
        .trim()
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    Ok(PortInfo {
        main_port: port,
        preview_proxy_port: None,
    })
}
