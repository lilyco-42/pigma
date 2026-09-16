//! pigma-trace — 把 agent 的执行 trace (ndjson) 播放成"歌词"
//!
//! 复用 pigma 的歌词渲染器 (`ui::lyrics::draw`)：**一步 = 一行歌词**，
//! 当前行走卡拉OK 渐变，其余行按距离淡出 —— 与音乐场景完全同一套 UI。
//!
//! 用法:
//!   pigma-trace <trace.ndjson> [--follow] [--speed 1.0]
//!
//! 键位: `空格` 播放/暂停 · `j`/`k` 单步前后 · `+`/`-` 调速 · `q` 退出
//!
//! 事件格式见 `lycore/src/trace.rs` (kind: prompt/tool/code/revert/final)

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};

use pigma::{
    config::{BorderConfig, Theme},
    playback::{LyricLine, PlaybackState},
    ui::{block::BlockStyle, lyrics},
    utils::GradientPreset,
};

/// 把一条 trace 事件渲染成"一行歌词"
fn fmt_event(v: &serde_json::Value) -> String {
    let kind = v["kind"].as_str().unwrap_or("?");
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    match kind {
        "prompt" => format!("\u{266A} {}", s("text")),
        "tool" => {
            let ok = v["ok"].as_bool().unwrap_or(false);
            format!(
                "{} {}({})",
                if ok { "\u{2713}" } else { "\u{2717}" },
                s("name"),
                s("arg")
            )
        }
        "code" => format!("\u{25B8} {} {}", s("file"), s("diff")),
        "revert" => format!("\u{21BA} {}", s("why")),
        "final" => format!("\u{2605} {}", s("answer")),
        other => format!("[{other}] {}", v.to_string()),
    }
}

/// 读 trace → 歌词行 (time 用事件自带的 t_ms; 缺省按序号 × 800ms 兜底)
///
/// 注意: trace 的 t_ms 是**真实执行耗时**, 一段快任务整条只有几十毫秒 →
/// 直接按它播会"一闪而过"。所以默认 `--beat` (一步一拍) 覆盖时间轴, `--real` 才用真实时间。
fn load_lines(path: &PathBuf) -> Vec<LyricLine> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let t_ms = v["t_ms"]
            .as_u64()
            .unwrap_or((n as u64 + 1) * 800);
        out.push(LyricLine {
            time: Duration::from_millis(t_ms),
            text: fmt_event(&v),
        });
    }
    out
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "用法: pigma-trace <trace.ndjson> [--follow] [--speed 1.0] [--beat 1200] [--real]\n\
             \x20 --beat <ms>  一步一拍 (默认 1200ms, 覆盖事件时间轴 —— 快任务不会一闪而过)\n\
             \x20 --real       用 trace 里的真实 t_ms (适合本身就跨秒的任务)"
        );
        std::process::exit(2);
    }
    let path = PathBuf::from(&args[0]);
    let follow = args.iter().any(|a| a == "--follow");
    let real = args.iter().any(|a| a == "--real");
    let beat: u64 = args
        .iter()
        .position(|a| a == "--beat")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1200);
    let mut speed: f64 = args
        .iter()
        .position(|a| a == "--speed")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);

    let mut lines = load_lines(&path);
    // 默认"一步一拍": 重写时间轴 (真实 t_ms 常是几十 ms, 按它播会一闪而过)
    let retime = |ls: &mut Vec<LyricLine>| {
        if !real {
            for (i, l) in ls.iter_mut().enumerate() {
                l.time = Duration::from_millis((i as u64 + 1) * beat);
            }
        }
    };
    retime(&mut lines);
    if lines.is_empty() {
        eprintln!("[pigma-trace] {} 无有效事件 (还没开始?)", path.display());
    }

    // 终端: raw + 备用屏
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let theme = Theme::default();
    let border = BorderConfig::default();
    let gradient = GradientPreset::default(); // rainbow

    let mut playing = true;
    let mut sim_ms: f64 = 0.0; // 合成播放头 (与事件 t_ms 同一时间轴)
    let mut last = Instant::now();
    let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();

    let res = loop {
        // --follow: 文件有更新就重载 (tail -f 语义 = "现场")
        if follow {
            if let Ok(mt) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                if Some(mt) != last_mtime {
                    lines = load_lines(&path);
                    retime(&mut lines); // 直播时新事件同样按拍重排, 不会一闪而过
                    last_mtime = Some(mt);
                }
            }
        }

        let total_ms = lines
            .last()
            .map(|l| l.time.as_millis() as f64 + 3000.0)
            .unwrap_or(1.0);

        if playing {
            let dt = last.elapsed().as_secs_f64() * 1000.0 * speed;
            sim_ms += dt;
        }
        last = Instant::now();

        let mut state = PlaybackState {
            lyrics: Some(lines.clone()),
            progress: (sim_ms / total_ms).clamp(0.0, 1.0),
            playing,
            ..Default::default()
        };

        if let Err(e) = terminal.draw(|f| {
            let area: Rect = f.area();
            lyrics::draw(
                f,
                &state,
                &BlockStyle {
                    colors: &theme,
                    border: &border,
                    tick: (sim_ms / 80.0) as u64,
                },
                gradient,
                "\u{25B6} AGENT TRACE",
                area,
            );
        }) {
            break Err(e.into());
        }

        state.lyrics = None; // 释放副本

        // 键盘
        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char(' ') => playing = !playing,
                    KeyCode::Char('+') => speed = (speed * 1.5).min(8.0),
                    KeyCode::Char('-') => speed = (speed / 1.5).max(0.1),
                    KeyCode::Char('j') => sim_ms += 800.0, // 单步前进
                    KeyCode::Char('k') => sim_ms = (sim_ms - 800.0).max(0.0),
                    _ => {}
                }
            }
        }

        // 播完就停在末尾 (除非 follow 等新事件)
        if !follow && sim_ms > total_ms {
            sim_ms = total_ms;
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}
