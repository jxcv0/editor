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
    buffer::{Buffer, Pos},
    config::Config,
    editor::{BufferSlot, Editor, PickerKind},
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
    let mut frames = ui::FrameBuilder::new();
    for index in 0..500 {
        let started = Instant::now();
        editor.handle_key(Key::char('i'));
        editor.handle_key(Key::char(if index % 2 == 0 { 'x' } else { 'y' }));
        editor.handle_key(Key::plain(KeyCode::Esc));
        black_box(frames.draw_editor(&mut editor, 120, 40));
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

    // Exercise the workload shapes that a short-line insertion misses. These
    // remain component timings: process launch, Runtime maintenance, and the
    // terminal consumer/flush are outside this benchmark.
    let mut all_ok = startup_ok && input_ok;
    let mut words = scratch(&format!("{} tail", "a".repeat(4_000)));
    let mut samples = Vec::new();
    for _ in 0..100 {
        words.active_pane_mut().cursor = Pos::ZERO;
        let started = Instant::now();
        words.handle_key(Key::char('w'));
        black_box(frames.draw_editor(&mut words, 120, 40));
        samples.push(started.elapsed());
    }
    all_ok &= check_input("4 KB word motion + canvas", &mut samples);

    let indentation = "line of text to indent\n".repeat(4096);
    samples.clear();
    for _ in 0..50 {
        let mut editor = scratch(&indentation);
        editor.handle_key(Key::char('>'));
        let started = Instant::now();
        editor.handle_key(Key::char('G'));
        black_box(frames.draw_editor(&mut editor, 120, 40));
        samples.push(started.elapsed());
    }
    all_ok &= check_input("94 KB indent-all + canvas", &mut samples);

    let long_line = "let x = 1; ".repeat((1024 * 1024 - 7) / 11);
    fs::write(&path, format!("short\n{long_line}\n"))?;
    let mut long = scratch("");
    long.buffers[0].buffer = Buffer::open(&path)?;
    samples.clear();
    for _ in 0..50 {
        let mut cold = ui::FrameBuilder::new();
        let started = Instant::now();
        black_box(cold.draw_editor(&mut long, 120, 40));
        samples.push(started.elapsed());
    }
    all_ok &= check_input("1 MiB long Rust line, cold canvas", &mut samples);
    long.active_pane_mut().cursor = Pos::new(1, long_line.len() - 2);
    samples.clear();
    for _ in 0..100 {
        let started = Instant::now();
        black_box(frames.draw_editor(&mut long, 120, 40));
        samples.push(started.elapsed());
    }
    all_ok &= check_input("1 MiB line end, cached canvas", &mut samples);

    let mut finder = scratch("");
    finder.set_project_files(
        (0..10_000)
            .map(|index| {
                PathBuf::from(format!(
                    "/work/crates/service_{:03}/src/handler_{index:06}.rs",
                    index % 400
                ))
            })
            .collect(),
    );
    finder.show_picker_items(PickerKind::Files, Vec::new());
    samples.clear();
    for _ in 0..100 {
        finder.picker.as_mut().unwrap().query = "hand".into();
        let started = Instant::now();
        finder.handle_key(Key::char('l'));
        black_box(frames.draw_editor(&mut finder, 120, 40));
        samples.push(started.elapsed());
    }
    all_ok &= check_input("10k-file finder input + canvas", &mut samples);
    println!("Component checks only; excludes process launch, maintenance, and terminal flush.");

    let _ = fs::remove_file(&path);
    if !all_ok {
        return Err("one or more editor smoke budgets regressed".into());
    }
    Ok(())
}

fn scratch(text: &str) -> Editor {
    let mut editor = Editor::new(Config::default(), PathBuf::from("."));
    editor.buffers[0].buffer = Buffer::from_text(text);
    editor
}

fn check_input(label: &str, samples: &mut [Duration]) -> bool {
    let p95 = percentile(samples, 95);
    let passed = p95 <= Duration::from_millis(8);
    println!(
        "{label} p95: {:.3} ms [{} 8 ms budget]",
        millis(p95),
        verdict(passed)
    );
    passed
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
