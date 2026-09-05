//! Repeatable local smoke benchmark for the budgets in DESIGN.md.
//!
//! Run with: cargo run --release --bin editor-bench

use std::{
    fs,
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

use editor::{
    buffer::Buffer,
    config::Config,
    editor::{BufferSlot, Editor},
    input::{Key, KeyCode},
    ui,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = benchmark_file();
    let mut source = String::with_capacity(1024 * 1024);
    while source.len() < 1024 * 1024 {
        source.push_str("pub fn measured(value: usize) -> usize { value + 1 }\n");
    }
    source.truncate(1024 * 1024);
    while !source.is_char_boundary(source.len()) {
        source.pop();
    }
    fs::write(&path, source.as_bytes())?;

    // Warm the filesystem and allocator paths before collecting samples.
    drop(Buffer::open(&path)?);
    let mut startup = Vec::with_capacity(100);
    for _ in 0..100 {
        let started = Instant::now();
        black_box(Buffer::open(&path)?);
        startup.push(started.elapsed());
    }

    let buffer = Buffer::open(&path)?;
    let mut editor = Editor::new(Config::default(), PathBuf::from("."));
    editor.buffers[0] = BufferSlot {
        buffer,
        display_name: "benchmark.rs".into(),
        large_file: false,
    };
    let mut input = Vec::with_capacity(500);
    for index in 0..500 {
        let started = Instant::now();
        editor.handle_key(Key::char('i'));
        editor.handle_key(Key::char(if index % 2 == 0 { 'x' } else { 'y' }));
        editor.handle_key(Key::plain(KeyCode::Esc));
        black_box(ui::draw_editor(&mut editor, 120, 40));
        input.push(started.elapsed());
        editor.handle_key(Key::char('u'));
    }

    let startup_p95 = percentile(&mut startup, 95);
    let input_p95 = percentile(&mut input, 95);
    let startup_ok = startup_p95 <= Duration::from_millis(50);
    let input_ok = input_p95 <= Duration::from_millis(8);
    println!(
        "warm 1 MiB buffer open p95: {:>8.3} ms  [{} 50 ms budget]",
        millis(startup_p95),
        verdict(startup_ok)
    );
    println!(
        "edit + resulting frame p95: {:>6.3} ms  [{} 8 ms budget]",
        millis(input_p95),
        verdict(input_ok)
    );

    let _ = fs::remove_file(&path);
    if !startup_ok || !input_ok {
        return Err("one or more editor smoke budgets regressed".into());
    }
    Ok(())
}

fn benchmark_file() -> PathBuf {
    std::env::temp_dir().join(format!("editor-benchmark-{}.rs", std::process::id()))
}

fn percentile(samples: &mut [Duration], percentile: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() * percentile / 100).min(samples.len() - 1)]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn verdict(passed: bool) -> &'static str {
    if passed { "PASS" } else { "FAIL" }
}
