use regex::Regex;
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
        "AT+CFUN=1,1",
    ];

    for cmd in &at_commands {
        send_at_command(&mut port, cmd).await?;
    }

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
    // Flush the output (what we write)
    port.flush().await?;

    // Drain input buffer
    let mut buf = [0u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_millis(100), port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                // discard the data
            }
            _ => break, // timeout or no data, done
        }
    }

    Ok(())
}

// Detect the first likely RM500U serial port
async fn detect_module() -> anyhow::Result<String> {
    use glob::glob;

    for entry in glob("/dev/ttyUSB*")? {
        let port = entry?;
        // Quick check: try opening and sending basic AT command
        if let Ok(mut p) = tokio_serial::new(&*port.to_string_lossy(), 115200).open_native_async() {
            if send_at_command(&mut p, "AT").await.is_ok() {
                return Ok(port.to_string_lossy().to_string());
            }
        }
    }

    anyhow::bail!("No RM500U module found on /dev/ttyUSB*");
}

// Send AT command and wait for full response
async fn send_at_command(port: &mut SerialStream, command: &str) -> anyhow::Result<()> {
    let full_command = format!("{}\r", command);
    port.write_all(full_command.as_bytes()).await?;
    port.flush().await?;

    let mut buffer = vec![0u8; 1024];
    let mut response = String::new();

    loop {
        let n = port.read(&mut buffer).await?;
        if n == 0 {
            continue; // wait for more data
        }
        response.push_str(&String::from_utf8_lossy(&buffer[..n]));

        // Check if response contains OK or ERROR line
        if response.contains("\r\nOK\r\n") {
            println!("> {}:\n{}", command, response.trim());
            return Ok(());
        }
        if let Some(error_line) = response.lines().find(|l| l.starts_with("ERROR")) {
            anyhow::bail!("Command '{}' failed: {}", command, error_line);
        }
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
    Command::new("sudo")
        .args(&["dhclient", "-v", interface])
        .status()?;

    println!("Network configured.");
    Ok(())
}
