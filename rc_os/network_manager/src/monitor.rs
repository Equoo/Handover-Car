use colored::*;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

#[derive(Debug)]
struct ServingCell {
    technology: String,
    rsrp: i32,
    rsrq: i32,
    sinr: i32,
}

#[derive(Debug)]
struct NeighborCell {
    technology: String,
    earfcn: String,
    pci: String,
    rsrp: i32,
    rsrq: i32,
}

#[derive(Debug)]
enum ConnectionType {
    LTE,
    NR5GNSA, // Non-Standalone
    NR5GSA,  // Standalone
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

fn signal_bars(rsrp: i32) -> String {
    let bars = match rsrp {
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

    let res = tokio::time::timeout(Duration::from_secs(3), async {
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
        if line.starts_with("+QENG: ") && line.contains("servingcell") {
            let parts: Vec<&str> = line.split(',').collect();

            // LTE servingcell format
            if parts.len() > 15 && parts[1].contains("LTE") {
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
            else if parts.len() > 10 && parts[1].contains("NR5G") {
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
        if line.starts_with("+QENG: ") && line.contains("neighbourcell") {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut port = tokio_serial::new("/dev/ttyUSB2", 115200)
        .open_native_async()
        .expect("Failed to open serial port");

    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    loop {
        execute!(
            stdout,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;

        let lines = query_modem(&mut port, "AT+QENG=\"servingcell\"").await;
        let cells = parse_servingcells(&lines);
        let neighbors = parse_neighbors(&lines);
        let conn_type = determine_connection_type(&cells);

        println!(
            "{}",
            "╔══════════════════════════════════════════════╗".blue()
        );
        println!(
            "{}",
            "║         MODEM SIGNAL MONITOR                 ║"
                .blue()
                .bold()
        );
        println!(
            "{}",
            "╚══════════════════════════════════════════════╝".blue()
        );
        println!();
        println!("Connection: {}", display_connection_type(&conn_type));
        println!();

        if cells.is_empty() {
            println!("{}", "No serving cell info available".red());
        } else {
            for (idx, cell) in cells.iter().enumerate() {
                println!("{}", format!("━━━ {} ━━━", cell.technology).cyan().bold());
                println!("Signal: {}", signal_bars(cell.rsrp));
                println!("RSRP:   {} dBm", color_rsrp(cell.rsrp));
                println!("RSRQ:   {} dB", color_rsrq(cell.rsrq));
                println!("SINR:   {} dB", color_sinr(cell.sinr));
                if idx < cells.len() - 1 {
                    println!();
                }
            }
        }

        if !neighbors.is_empty() {
            println!();
            println!("{}", "━━━ Neighbor Cells ━━━".bright_black().bold());
            for (idx, neighbor) in neighbors.iter().enumerate().take(5) {
                println!(
                    "{} | PCI: {} | RSRP: {} | RSRQ: {}",
                    neighbor.technology.bright_black(),
                    neighbor.pci.bright_black(),
                    color_rsrp(neighbor.rsrp),
                    color_rsrq(neighbor.rsrq)
                );
            }
            if neighbors.len() > 5 {
                println!("... and {} more", neighbors.len() - 5);
            }
        }

        println!();
        println!("{}", "Press Q to quit".bright_black());
        std::io::Write::flush(&mut stdout)?;

        let mut quit = false;
        for _ in 0..10 {
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key_event) = event::read()? {
                    if matches!(key_event.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                        quit = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if quit {
            break;
        }
    }

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    Ok(())
}

