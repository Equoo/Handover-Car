use colored::*;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

#[derive(Debug, Clone)]
struct ServingCell {
    technology: String,
    rsrp: i32,
    rsrq: i32,
    sinr: i32,
}

#[derive(Debug, Clone)]
struct NeighborCell {
    technology: String,
    earfcn: String,
    pci: String,
    rsrp: i32,
    rsrq: i32,
}

#[derive(Debug, Clone)]
enum ConnectionType {
    LTE,
    NR5GNSA, // Non-Standalone
    NR5GSA,  // Standalone
}

#[derive(Debug, Clone)]
struct PingStats {
    min: f32,
    avg: f32,
    max: f32,
    packet_loss: f32,
}

fn color_rsrp(rsrp: i32) -> ColoredString {
    match rsrp {
        r if r > -80 => r.to_string().green(),
        r if r > -100 => r.to_string().yellow(),
        _ => rsrp.to_string().red(),
    }
}

fn color_rsrq(rsrq: i32) -> ColoredString {
    match rsrq {
        r if r > -10 => r.to_string().green(),
        r if r > -15 => r.to_string().yellow(),
        _ => rsrq.to_string().red(),
    }
}

fn color_sinr(sinr: i32) -> ColoredString {
    match sinr {
        r if r > 20 => r.to_string().green(),
        r if r > 10 => r.to_string().yellow(),
        _ => sinr.to_string().red(),
    }
}

fn color_ping(ms: f32) -> ColoredString {
    match ms as i32 {
        p if p < 50 => format!("{:.1}", ms).green(),
        p if p < 100 => format!("{:.1}", ms).yellow(),
        _ => format!("{:.1}", ms).red(),
    }
}

fn signal_bars(rsrp: i32) -> String {
    let bars = match rsrp {
        r if r == 0 => 0,
        r if r > -80 => 5,
        r if r > -90 => 4,
        r if r > -100 => 3,
        r if r > -110 => 2,
        r if r > -120 => 1,
        _ => 0,
    };

    let filled = "▮".repeat(bars);
    let empty = "▯".repeat(5 - bars);

    match bars {
        5 | 4 => format!("{}{}", filled.green(), empty),
        3 | 2 => format!("{}{}", filled.yellow(), empty),
        _ => format!("{}{}", filled.red(), empty),
    }
}

async fn query_modem(port: &mut SerialStream, command: &str) -> Vec<String> {
    let full_command = format!("{}\r", command);
    port.write_all(full_command.as_bytes()).await.unwrap();
    port.flush().await.unwrap();

    let mut buffer = vec![0u8; 4096];
    let mut response = String::new();
    let mut lines = Vec::new();

    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let n = port.read(&mut buffer).await.unwrap_or(0);
            if n > 0 {
                response.push_str(&String::from_utf8_lossy(&buffer[..n]));

                if response.contains("\r\nOK\r\n") {
                    lines = response.lines().map(String::from).collect();
                    break;
                }
                if response.lines().any(|l| l.starts_with("ERROR")) {
                    break;
                }
            }
        }
    })
    .await;

    lines
}

fn parse_servingcells(lines: &[String]) -> Vec<ServingCell> {
    let mut cells = Vec::new();

    for line in lines {
        if line.starts_with("+QENG: ") {
            let parts: Vec<&str> = line.split(',').collect();

            // LTE servingcell format
            if parts.len() > 15 && parts[0].contains("LTE") {
                let tech = parts[1].replace('"', "");
                let rsrp = parts[13].parse::<i32>().unwrap_or(0);
                let rsrq = parts[14].parse::<i32>().unwrap_or(0);
                let sinr = parts[15].parse::<i32>().unwrap_or(0);
                cells.push(ServingCell {
                    technology: tech,
                    rsrp,
                    rsrq,
                    sinr,
                });
            }
            // NR5G-NSA servingcell format
            else if parts.len() > 10 && parts[0].contains("NR5G") {
                let tech = parts[1].replace('"', "");
                let rsrp = parts[8].parse::<i32>().unwrap_or(0);
                let rsrq = parts[9].parse::<i32>().unwrap_or(0);
                let sinr = parts[10].parse::<i32>().unwrap_or(0);
                cells.push(ServingCell {
                    technology: tech,
                    rsrp,
                    rsrq,
                    sinr,
                });
            }
        }
    }

    cells
}

fn parse_neighbors(lines: &[String]) -> Vec<NeighborCell> {
    let mut neighbors = Vec::new();

    for line in lines {
        if line.starts_with("+QENG: ") {
            let parts: Vec<&str> = line.split(',').collect();

            // LTE neighbor format
            if parts.len() > 6 && parts[1].contains("LTE") {
                let tech = parts[1].replace('"', "");
                let earfcn = parts[2].to_string();
                let pci = parts[3].to_string();
                let rsrp = parts[5].parse::<i32>().unwrap_or(-999);
                let rsrq = parts[6].parse::<i32>().unwrap_or(-999);
                neighbors.push(NeighborCell {
                    technology: tech,
                    earfcn,
                    pci,
                    rsrp,
                    rsrq,
                });
            }
            // NR5G neighbor format
            else if parts.len() > 6 && parts[1].contains("NR5G") {
                let tech = parts[1].replace('"', "");
                let earfcn = parts[2].to_string();
                let pci = parts[3].to_string();
                let rsrp = parts[5].parse::<i32>().unwrap_or(-999);
                let rsrq = parts[6].parse::<i32>().unwrap_or(-999);
                neighbors.push(NeighborCell {
                    technology: tech,
                    earfcn,
                    pci,
                    rsrp,
                    rsrq,
                });
            }
        }
    }

    neighbors
}

fn determine_connection_type(cells: &[ServingCell]) -> ConnectionType {
    let has_lte = cells.iter().any(|c| c.technology.contains("LTE"));
    let has_5g = cells.iter().any(|c| c.technology.contains("NR5G"));

    if has_lte && has_5g {
        ConnectionType::NR5GNSA
    } else if has_5g {
        ConnectionType::NR5GSA
    } else {
        ConnectionType::LTE
    }
}

fn display_connection_type(conn_type: &ConnectionType) -> ColoredString {
    match conn_type {
        ConnectionType::LTE => "LTE".yellow(),
        ConnectionType::NR5GNSA => "5G NSA".bright_magenta(),
        ConnectionType::NR5GSA => "5G SA".bright_green(),
    }
}

async fn ping_host(hostname: &str) -> Option<PingStats> {
    let output = tokio::task::spawn_blocking({
        let hostname = hostname.to_string();
        move || {
            Command::new("ping")
                .arg("-c")
                .arg("5")
                .arg("-W")
                .arg("1")
                .arg(&hostname)
                .output()
        }
    })
    .await
    .ok()?
    .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse packet loss
    let packet_loss = stdout
        .lines()
        .find(|l| l.contains("packet loss"))?
        .split_whitespace()
        .find(|s| s.contains('%'))?
        .trim_end_matches('%')
        .parse::<f32>()
        .ok()?;

    // Parse rtt min/avg/max
    let rtt_line = stdout.lines().find(|l| l.contains("rtt min/avg/max"))?;
    let stats_part = rtt_line.split('=').nth(1)?.trim();
    let values: Vec<&str> = stats_part.split('/').collect();

    if values.len() >= 3 {
        Some(PingStats {
            min: values[0].parse().ok()?,
            avg: values[1].parse().ok()?,
            max: values[2].parse().ok()?,
            packet_loss,
        })
    } else {
        None
    }
}

fn render_display(
    cells: &[ServingCell],
    neighbors: &[NeighborCell],
    conn_type: &ConnectionType,
    ping_stats: &Option<PingStats>,
) -> String {
    let mut output = String::new();

    output.push_str("╔══════════════════════════════════════════════╗\n");
    output.push_str("║         MODEM SIGNAL MONITOR                 ║\n");
    output.push_str("╚══════════════════════════════════════════════╝\n");
    output.push('\n');
    output.push_str(&format!(
        "Connection: {}\n",
        display_connection_type(conn_type)
    ));
    output.push('\n');

    if cells.is_empty() {
        output.push_str(&format!("{}\n", "No serving cell info available".red()));
    } else {
        for (idx, cell) in cells.iter().enumerate() {
            output.push_str(&format!(
                "{}\n",
                format!("━━━ {} ━━━", cell.technology).cyan().bold()
            ));
            output.push_str(&format!("Signal: {}\n", signal_bars(cell.rsrp)));
            output.push_str(&format!("RSRP:   {} dBm\n", color_rsrp(cell.rsrp)));
            output.push_str(&format!("RSRQ:   {} dB\n", color_rsrq(cell.rsrq)));
            output.push_str(&format!("SINR:   {} dB\n", color_sinr(cell.sinr)));
            if idx < cells.len() - 1 {
                output.push('\n');
            }
        }
    }

    if let Some(stats) = ping_stats {
        output.push('\n');
        output.push_str(&format!(
            "{}\n",
            "━━━ Network Test (server) ━━━".cyan().bold()
        ));
        output.push_str(&format!("Min:         {} ms\n", color_ping(stats.min)));
        output.push_str(&format!("Avg:         {} ms\n", color_ping(stats.avg)));
        output.push_str(&format!("Max:         {} ms\n", color_ping(stats.max)));
        let loss_str = format!("{:.0}%", stats.packet_loss);
        let loss_colored = if stats.packet_loss > 5.0 {
            loss_str.red()
        } else if stats.packet_loss > 0.0 {
            loss_str.yellow()
        } else {
            loss_str.green()
        };
        output.push_str(&format!("Packet Loss: {}\n", loss_colored));
    }

    if !neighbors.is_empty() {
        output.push('\n');
        output.push_str(&format!(
            "{}\n",
            "━━━ Neighbor Cells ━━━".bright_black().bold()
        ));
        for neighbor in neighbors.iter().take(5) {
            output.push_str(&format!(
                "{} | PCI: {} | RSRP: {} | RSRQ: {}\n",
                neighbor.technology.bright_black(),
                neighbor.pci.bright_black(),
                color_rsrp(neighbor.rsrp),
                color_rsrq(neighbor.rsrq)
            ));
        }
        if neighbors.len() > 5 {
            output.push_str(&format!("... and {} more\n", neighbors.len() - 5));
        }
    }

    output.push('\n');
    output.push_str(&format!("{}\n", "Press Q to quit".bright_black()));

    output
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut port = tokio_serial::new("/dev/ttyUSB2", 115200)
        .open_native_async()
        .expect("Failed to open serial port");

    let mut stdout = std::io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let mut last_cells: Vec<ServingCell> = Vec::new();
    let mut last_neighbors: Vec<NeighborCell> = Vec::new();
    let mut last_ping: Option<PingStats> = None;
    let mut last_update = Instant::now();

    loop {
        // Update data every second
        if last_update.elapsed() >= Duration::from_millis(500) {
            last_cells =
                parse_servingcells(&query_modem(&mut port, "AT+QENG=\"servingcell\"").await);
            last_neighbors =
                parse_neighbors(&query_modem(&mut port, "AT+QENG=\"neighbourcell\"").await);
            last_ping = ping_host("server").await;
            last_update = Instant::now();
        }

        let conn_type = determine_connection_type(&last_cells);
        let display = render_display(&last_cells, &last_neighbors, &conn_type, &last_ping);

        execute!(
            stdout,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;
        print!("{}", display);
        std::io::Write::flush(&mut stdout)?;

        // Check for quit key
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key_event) = event::read()? {
                if matches!(key_event.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}
