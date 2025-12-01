use colored::*;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

async fn run_latency_probe(host: &str) -> Vec<(String, f32)> {
    // Simple ping latency probe; parse output to get latency jumps
    let output = Command::new("ping")
        .args(&["-c", "4", host])
        .output()
        .expect("Failed to run ping");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        if line.contains("time=") {
            if let Some(pos) = line.find("time=") {
                let time_str = &line[pos + 5..].split_whitespace().next().unwrap_or("0");
                if let Ok(lat) = time_str.parse::<f32>() {
                    results.push((line.to_string(), lat));
                }
            }
        }
    }
    results
}

fn color_latency(latency: f32) -> ColoredString {
    match latency {
        l if l < 50.0 => format!("{:.1} ms", l).green(),
        l if l < 150.0 => format!("{:.1} ms", l).yellow(),
        _ => format!("{:.1} ms", latency).red(),
    }
}

fn fetch_modem_info() -> Vec<(String, String)> {
    // Reads actual modem info using AT commands
    // For simplicity, using system calls; ideally use serial port reading
    let mut data = Vec::new();
    let output = Command::new("sh")
        .arg("-c")
        .arg("echo 'AT+QENG=\"servingcell\"' | socat - /dev/ttyUSB2,crnl")
        .output()
        .unwrap_or_else(|_| panic!("Failed to query modem"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.starts_with("OK") && !line.starts_with("AT") {
            data.push(("Serving Cell".to_string(), line.to_string()));
        }
    }

    data
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let mut show_latency_probe = false;

    loop {
        execute!(
            stdout,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;

        println!("{}", "Modem Monitor (press Q to quit)".bold().blue());
        println!("----------------------------------------");

        let info = fetch_modem_info();
        for (key, value) in info {
            println!("{}: {}", key.yellow(), value);
        }

        if show_latency_probe {
            println!("\n{}", "Latency Probe (ping 1.1.1.1)".bold().cyan());
            let latency_results = run_latency_probe("1.1.1.1").await;
            for (line, lat) in latency_results {
                println!("{} -> {}", line, color_latency(lat));
            }
        }

        println!("\nPress L to toggle latency probe");
        stdout.flush()?;

        // Non-blocking key read
        if event::poll(Duration::from_millis(1000))? {
            if let Event::Key(key_event) = event::read()? {
                match key_event.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break,
                    KeyCode::Char('l') | KeyCode::Char('L') => {
                        show_latency_probe = !show_latency_probe
                    }
                    _ => {}
                }
            }
        }
    }

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    Ok(())
}
