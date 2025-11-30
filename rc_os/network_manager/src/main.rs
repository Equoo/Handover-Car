use anyhow::bail;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let serial_port = detect_module().await?;
    println!("Using serial port: {}", serial_port);

    let mut port = tokio_serial::new(&serial_port, 115200).open_native_async()?;

    flush_serial(&mut port).await?;

    // AT commands to configure ECM
    let at_commands = [
        "AT+CPIN?",
        "AT+QENG=\"servingcell\"",
        "AT+QCFG=\"ethernet\",0",
        "AT+QCFG=\"usbnet\",1",
        "AT+QNETDEVCTL=1,3,1",
        "AT+QNETDEVCTL=1,1,1",
        "AT+QCFG=\"nat\",0",
        // "AT+CFUN=0",
        // "AT+CFUN=1",
    ];

    for cmd in &at_commands {
        send_at_command(&mut port, cmd).await?;
    }

    tokio::time::sleep(Duration::from_secs(3)).await;

    println!("Module configured, detecting RNDIS interface...");

    if let Some(interface) = detect_rndis_interface().await? {
        println!("Found interface: {}", interface);
        configure_network(&interface)?;
    } else {
        println!("No RNDIS interface found. Check module connection.");
    }

    Ok(())
}

async fn flush_serial(port: &mut SerialStream) -> anyhow::Result<()> {
    // Flush outgoing buffer
    port.flush().await?;

    // Drain input buffer for 500ms
    let mut buf = [0u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_millis(200), port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => { /* discard */ }
            _ => break,
        }
    }

    // Now verify module responds
    println!("Checking if module responds to 'AT'...");

    for _ in 0..5 {
        match send_at_quick(port, "AT").await {
            Ok(_) => return Ok(()),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }

    anyhow::bail!("Module does not respond after flush")
}

async fn send_at_quick(port: &mut SerialStream, cmd: &str) -> anyhow::Result<()> {
    let full = format!("{}\r", cmd);
    port.write_all(full.as_bytes()).await?;
    port.flush().await?;

    let mut buf = [0u8; 256];
    let mut resp = String::new();

    let res = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let n = port.read(&mut buf).await?;
            if n > 0 {
                resp.push_str(&String::from_utf8_lossy(&buf[..n]));
                if resp.contains("OK") {
                    return Ok(());
                }
            }
        }
    })
    .await;

    match res {
        Ok(r) => r,
        Err(_) => anyhow::bail!("Timeout waiting for 'OK'"),
    }
}

const AT_IFACE: &str = "if04"; // interface for AT commands

pub async fn detect_module() -> anyhow::Result<String> {
    let dir = Path::new("/dev/serial/by-id");
    if !dir.exists() {
        bail!("/dev/serial/by-id does not exist");
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Pick the interface you want
        if name.contains("RM500U") && name.contains(AT_IFACE) {
            let link_target = fs::read_link(entry.path())?;
            let dev = link_target
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("Invalid symlink target"))?;

            return Ok(format!("/dev/{}", dev));
        }
    }

    bail!("RM500U AT interface not found (if04)");
}

// Send AT command and wait for full response
async fn send_at_command(port: &mut SerialStream, command: &str) -> anyhow::Result<String> {
    let full_command = format!("{}\r", command);
    port.write_all(full_command.as_bytes()).await?;
    port.flush().await?;

    let mut buffer = vec![0u8; 1024];
    let mut response = String::new();

    // Full command timeout (e.g. 3 seconds)
    let res = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let n = port.read(&mut buffer).await?;
            if n > 0 {
                response.push_str(&String::from_utf8_lossy(&buffer[..n]));

                if response.contains("\r\nOK\r\n") {
                    println!("<< {}\n> {}", command, response);
                    return Ok(response);
                }
                if let Some(error) = response.lines().find(|l| l.starts_with("ERROR")) {
                    anyhow::bail!("{} failed: {}", command, error);
                }
            }
        }
    })
    .await;

    match res {
        Ok(r) => r,
        Err(_) => anyhow::bail!("Timeout waiting for response to '{}'", command),
    }
}

// Detect RNDIS interface on Linux
async fn detect_rndis_interface() -> anyhow::Result<Option<String>> {
    use std::fs;

    let entries = fs::read_dir("/sys/class/net")?;
    let re = Regex::new(r"^enx[a-f0-9]+")?;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().into_string().unwrap_or_default();
        if re.is_match(&name) {
            return Ok(Some(name));
        }
    }

    let entries = fs::read_dir("/sys/class/net")?;
    let re = Regex::new(r"^usb[0-9]+")?;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().into_string().unwrap_or_default();
        if re.is_match(&name) {
            return Ok(Some(name));
        }
    }

    Ok(None)
}

// Configure network via DHCP
fn configure_network(interface: &str) -> anyhow::Result<()> {
    println!("Acquiring IP via dhclient...");
    Command::new("pkill")
        .args(&["-f", "dhclient.*$NET_IF"])
        .status()?;
    Command::new("pkill")
        .args(&["-f", "udhcpc.*$NET_IF"])
        .status()?;
    Command::new("dhclient").args(&["-v", interface]).status()?;

    println!("Network configured.");
    Ok(())
}
