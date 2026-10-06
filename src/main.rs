use anyhow::{Context, Result};
use chrono::Local;
use colored::Colorize;
use rusqlite::{params, Connection};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use sysinfo::{Disks, System};

use tunjukin_suhu_cpu_windows::CpuTemperature;

/**
**/
struct Sample {
    timestamp: String,
    cpu_temp: Option<f64>,
    cpu_usage: Option<f64>,
    gpu_name: Option<String>,
    gpu_util: Option<f64>,
    ssd_used_gb: Option<f64>,
    ssd_total_gb: Option<f64>,
    ssd_pct: Option<f64>,
    mem_used_gb: Option<f64>,
    mem_total_gb: Option<f64>,
}

// * Text file and db for storing results
const DB_PATH: &str = "sysmon.db";
const TXT_PATH: &str = "sysmon.txt";

// * Main entry point of program
fn main() -> Result<()> {
    
    let mut s = System::new();
    s.refresh_all();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    s.refresh_cpu_usage();

    let sample = collect(s);

    print_report(&sample);
    log_to_database(&sample)?;
    append_to_txt(&sample)?;

    println!(
        "\n{} logged to {} and {}",
        "✔".green(),
        DB_PATH.cyan(),
        TXT_PATH.cyan()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

fn collect(sys: System) -> Sample {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let cpu_usage = {
        let total: f32 = sys.cpus().iter().map(|c| c.cpu_usage()).sum();
        let n = sys.cpus().len() as f64;
        if n > 0.0 { Some(total as f64 / n) } else { None }
    };

    // Overall memory
    let (mem_used_gb, mem_total_gb) = {
        let total = sys.total_memory() as f64 / 1e9;
        let used = sys.used_memory() as f64 / 1e9;
        if total > 0.0 { (Some(used), Some(total)) } else { (None, None) }
    };

    // SSD / disk usage: aggregate all physical disks.
    let (ssd_used_gb, ssd_total_gb, ssd_pct) = {
        let mut used: u64 = 0;
        let mut total: u64 = 0;
        let disks = Disks::new_with_refreshed_list();
        for disk in disks.list() {
                used += disk.total_space() - disk.available_space();
                total += disk.total_space();
            
        }
        if total > 0 {
            let pct = (used as f64 / total as f64) * 100.0;
            (
                Some((used as f64 / 1e9)),
                Some((total as f64 / 1e9)),
                Some(pct),
            )
        } else {
            (None, None, None)
        }
    };

    Sample {
        timestamp,
        cpu_temp: read_cpu_temp(),
        cpu_usage,
        gpu_name: read_gpu().as_ref().map(|g| g.0.clone()),
        gpu_util: read_gpu().map(|g| g.1),
        ssd_used_gb,
        ssd_total_gb,
        ssd_pct,
        mem_used_gb,
        mem_total_gb,
    }
}

// returns CPU temperature if able
fn read_cpu_temp() -> Option<f64> {
    let mut cpu_temp: Option<f64> = None;
    
    match CpuTemperature::get() {
        Ok(temp) => {
            cpu_temp = Some(temp.celsius);
        }
        Err(e) => {
            eprintln!("error getting temp: {}", e)
        }
    }
    cpu_temp
}

/// GPU: try NVIDIA first, then AMD. Returns (name, utilization %).
fn read_gpu() -> Option<(String, f64)> {
    if let Some(g) = read_gpu_nvidia() {
        return Some(g);
    }
    read_gpu_amd()
}

// Get details of NVIDIA GPU: name, utilization
fn read_gpu_nvidia() -> Option<(String, f64)> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() { // return None if status is fail
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    println!("nvidia-smi stdout is: {}", stdout);
    // Take first GPU line.
    let line = stdout.lines().next()?;
    let mut parts = line.splitn(2, ',');
    let name = parts.next()?.trim().to_string();
    let util: f64 = parts.next()?.trim().parse().ok()?;
    Some((name, util))
}

fn read_gpu_amd() -> Option<(String, f64)> {
    // /sys/class/drm/card0/device/gpu_busy_percent
    for card in ["card0", "card1"] {
        let p = format!("/sys/class/drm/{}/device/gpu_busy_percent", card);
        if let Ok(raw) = fs::read_to_string(&p) {
            if let Ok(util) = raw.trim().parse::<f64>() {
                return Some((format!("AMD GPU ({card})"), util));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Pretty (Steam-Overlay-style) output
// ---------------------------------------------------------------------------

/// Draw a small ASCII bar, e.g. "████████░░░░░░░░░░ 42%"
fn bar(pct: f64, width: usize) -> String {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    let (good, warm, hot) = (green(), amber(), red());
    let color = if pct >= 85.0 { hot } else if pct >= 65.0 { warm } else { good };
    let s = "█".repeat(filled) + &"░".repeat(width - filled);
    format!("{} {}{}{}", color(&s), " ", pct.floor() as u32, "%")
}

fn green() -> Box<dyn Fn(&str) -> Box<dyn std::fmt::Display> + 'static> {
    Box::new(|s: &str| Box::new(s.green()))
}
fn amber() -> Box<dyn Fn(&str) -> Box<dyn std::fmt::Display> + 'static> {
    Box::new(|s: &str| Box::new(s.yellow()))
}
fn red() -> Box<dyn Fn(&str) -> Box<dyn std::fmt::Display> + 'static> {
    Box::new(|s: &str| Box::new(s.red()))
}

fn f(v: Option<f64>, unit: &str) -> String {
    match v {
        Some(x) => format!("{x:.1}{unit}"),
        None => "n/a".to_string(),
    }
}

fn print_report(s: &Sample) {
    let w = 46;
    let sep = "─".repeat(w);

    // * Printed report heaading section
    println!("\n{}", "┌".bold());
    println!("{}  {}", "│".bold(), " SYSTEM MONITOR ".bold().cyan());
    println!("{}  {}", "│".bold(), s.timestamp.green());
    println!("{}", "├".bold());

    // * CPU printed report section
    println!(
        "{}  {:<20}{}",
        "│".bold(),
        "CPU Temperature".bold(),
        match s.cpu_temp {
            Some(t) => f(s.cpu_temp, "° Celsius").to_string(),
            None => "n/a".to_string(),
        }
    );
    println!(
        "{}  {:<20}{}",
        "│".bold(),
        "CPU Usage".bold(),
        s.cpu_usage.map(|u| bar(u, 35).to_string()).unwrap_or_else(|| "n/a".into())
    );

    // * GPU printed report section
    let gpu_name = s.gpu_name.clone().unwrap_or_else(|| "n/a".into());
    println!("{}", "├".bold());
    println!("{}  {:<20}{}", "│".bold(), "GPU".bold(), gpu_name.cyan());
    println!(
        "{}  {:<20}{}",
        "│".bold(),
        "GPU Utilization".bold(),
        s.gpu_util.map(|u| bar(u, 35).to_string()).unwrap_or_else(|| "n/a".into())
    );

    // Memory
    println!("{}", "├".bold());
    println!(
        "{}  {:<20}{}",
        "│".bold(),
        "Memory".bold(),
        format!("{} / {}", f(s.mem_used_gb, " GB"), f(s.mem_total_gb, " GB")).dimmed()
    );

    // SSD
    let ssd_pct = s.ssd_pct.unwrap_or(0.0);
    println!("{}", "├".bold());
    println!(
        "{}  {:<20}{}",
        "│".bold(),
        "SSD Usage".bold(),
        format!("{} / {}", f(s.ssd_used_gb, " GB"), f(s.ssd_total_gb, " GB")).dimmed()
    );
    println!("{}  {:<20}{}", "│".bold(), " ".to_string(), bar(ssd_pct, 35).to_string());

    println!("{}", "└".bold());
    let _ = (w, sep);
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

fn log_to_database(s: &Sample) -> Result<()> {
    let conn = Connection::open(DB_PATH)
        .with_context(|| format!("opening sqlite db at {DB_PATH}"))?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS samples (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          TEXT    NOT NULL,
            cpu_temp    REAL,
            cpu_usage   REAL,
            gpu_name    TEXT,
            gpu_util    REAL,
            ssd_used_gb REAL,
            ssd_total_gb REAL,
            ssd_pct     REAL,
            mem_used_gb REAL,
            mem_total_gb REAL
         );",
    )?;

    conn.execute(
        "INSERT INTO samples
            (ts, cpu_temp, cpu_usage, gpu_name, gpu_util,
             ssd_used_gb, ssd_total_gb, ssd_pct, mem_used_gb, mem_total_gb)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            s.timestamp,
            s.cpu_temp,
            s.cpu_usage,
            s.gpu_name,
            s.gpu_util,
            s.ssd_used_gb,
            s.ssd_total_gb,
            s.ssd_pct,
            s.mem_used_gb,
            s.mem_total_gb,
        ],
    )?;

    Ok(())
}

fn append_to_txt(s: &Sample) -> Result<()> {
    let mut line = format!(
        "[{}] cpu_temp={} cpu={} gpu=\"{}\" gpu_util={} ssd={}/{} ({}) mem={}/{}\n",
        s.timestamp,
        s.cpu_temp.map(|v| format!("{v:.1}C")).unwrap_or_else(|| "n/a".into()),
        s.cpu_usage.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "n/a".into()),
        s.gpu_name.clone().unwrap_or_else(|| "n/a".into()),
        s.gpu_util.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "n/a".into()),
        s.ssd_used_gb.map(|v| format!("{v:.1}GB")).unwrap_or_else(|| "n/a".into()),
        s.ssd_total_gb.map(|v| format!("{v:.1}GB")).unwrap_or_else(|| "n/a".into()),
        s.ssd_pct.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "n/a".into()),
        s.mem_used_gb.map(|v| format!("{v:.1}GB")).unwrap_or_else(|| "n/a".into()),
        s.mem_total_gb.map(|v| format!("{v:.1}GB")).unwrap_or_else(|| "n/a".into()),
    );
    // Trailing newline + a blank separator line between snapshots.
    line.push('\n');

    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(TXT_PATH)
        .with_context(|| format!("opening log file {TXT_PATH}"))?;
    f.write_all(line.as_bytes())?;

    Ok(())
}