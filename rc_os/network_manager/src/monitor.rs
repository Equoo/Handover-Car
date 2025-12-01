use colored::*;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

#[derive(Debug)]
struct ServingCell {
    technology: String,
    rsrp: i32,
    rsrq: i32,
    sinr: i32,
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

async fn query_modem(port: &mut SerialStream, cmd: &str) -> Vec<String> {
    port.write_all(format!("{}\r\n", cmd).as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(port);
    let mut lines = Vec::new();
    loop {
        let mut buf = String::new();
        match reader.read_line(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let line = buf.trim();
                if line == "OK" || line.starts_with("AT") {
                    break;
                }
                lines.push(line.to_string());
            }
            Err(_) => break,
        }
    }
    lines
}

fn parse_servingcell(lines: &[String]) -> Option<ServingCell> {
    for line in lines {
        if line.starts_with("+QENG: \"servingcell\"") {
            // Example line:
            // +QENG: "servingcell","LTE","FDD",208,10,9C91F03,326,1501,3,5,5,C161,-99,-12,-68,6,10,22,28
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() > 15 {
                let tech = parts[1].replace('"', "");
                let rsrp = parts[13].parse::<i32>().unwrap_or(0);
                let rsrq = parts[14].parse::<i32>().unwrap_or(0);
                let sinr = parts[15].parse::<i32>().unwrap_or(0);
                return Some(ServingCell {
                    technology: tech,
                    rsrp,
                    rsrq,
                    sinr,
                });
            }
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Open serial port (replace ttyUSB2 with your modem device)
    let mut port = tokio_serial::new("/dev/ttyUSB2", 115_200)
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
        println!("{}", "Modem Monitor (press Q to quit)".bold().blue());
        println!("----------------------------------------");

        let lines = query_modem(&mut port, "AT+QENG=\"servingcell\"").await;
        if let Some(cell) = parse_servingcell(&lines) {
            println!("Technology: {}", cell.technology.cyan());
            println!("RSRP:       {}", color_rsrp(cell.rsrp));
            println!("RSRQ:       {}", color_rsrq(cell.rsrq));
            println!("SINR:       {}", color_sinr(cell.sinr));
        } else {
            println!("{}", "No serving cell info available".red());
        }

        println!("\nPress Q to quit");
        stdout.flush()?;

        // Wait 1 second or quit
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

