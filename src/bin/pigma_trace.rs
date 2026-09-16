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
use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, Paragraph, Wrap},
    Terminal,
};

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
            // 有人话描述就用描述 (about), 没有才退回命令原文 —— "看懂每段在干什么"
            let label = if !s("about").is_empty() {
                s("about")
            } else {
                format!("{}({})", s("name"), s("arg"))
            };
            format!("{} {}", if ok { "\u{2713}" } else { "\u{2717}" }, label)
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

/// 抽出每条事件的原始命令 (与歌词行一一对应, 供 `r` 键真执行)
fn load_cmds(path: &PathBuf) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    // ⚠️ 必须与 load_lines() 的过滤规则完全一致 (跳过空行 + 解析失败行),
    // 否则索引会错位 —— cmds[cur] 命中空条目 → "r 键没反应"(实测踩坑)。
    raw.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                return None;
            }
            serde_json::from_str::<serde_json::Value>(l).ok().map(|v| {
                if v["kind"] == "tool" {
                    v["arg"].as_str().unwrap_or("").to_string()
                } else {
                    String::new()
                }
            })
        })
        .collect()
}

/// 跨平台执行一条命令并返回 stdout+stderr (Windows 用 cmd /C, 其余 sh -c)
fn run_cmd(cmd: &str) -> String {
    let out = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", cmd]).output()
    } else {
        std::process::Command::new("sh").args(["-c", cmd]).output()
    };
    match out {
        Ok(o) => {
            let mut s = String::new();
            for (name, b) in [("stdout", o.stdout), ("stderr", o.stderr)] {
                let t = String::from_utf8_lossy(&b);
                let t = t.trim();
                if !t.is_empty() {
                    s.push_str(&format!("[{name}]\n{t}\n"));
                }
            }
            if s.is_empty() {
                s = format!("(exit={}, 无输出)", o.status.code().unwrap_or(-1));
            } else {
                s.push_str(&format!("\n(exit={})", o.status.code().unwrap_or(-1)));
            }
            s
        }
        Err(e) => format!("执行失败: {e}"),
    }
}

/// 扫描目录 (类 yazi 面板): 跳过隐藏/target/node_modules, 深度 ≤3, 最多 200 条
fn scan_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(d: &std::path::Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
        if depth > 3 || out.len() >= 200 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut entries: Vec<std::path::PathBuf> =
            rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else {
                out.push(p);
            }
            if out.len() >= 200 {
                break;
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

/// 预览文件: 行号 + 轻量语法着色 (按扩展名; 真 LSP 语义高亮见 P2)
fn preview_file(p: &std::path::Path) -> Vec<Line<'static>> {
    use ratatui::style::Color;
    use ratatui::text::Span;
    let Ok(raw) = std::fs::read_to_string(p) else {
        return vec![Line::from("(无法读取: 非文本或权限不足)")];
    };
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let code = matches!(ext, "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "java");
    raw.lines()
        .take(500)
        .enumerate()
        .map(|(i, l)| {
            let t = l.trim_start();
            let fg = if !code {
                Color::Rgb(180, 180, 180)
            } else if t.starts_with('#') || t.starts_with("//") {
                Color::Rgb(110, 110, 110) // 注释
            } else if t.starts_with('"') || t.starts_with('\'') {
                Color::Rgb(150, 190, 140) // 字符串
            } else if t.starts_with("fn ")
                || t.starts_with("def ")
                || t.starts_with("class ")
                || t.starts_with("pub ")
                || t.starts_with("use ")
                || t.starts_with("import ")
            {
                Color::Rgb(130, 170, 220) // 关键字
            } else {
                Color::Rgb(195, 195, 195)
            };
            Line::from(vec![
                Span::styled(format!("{:>4} ", i + 1), Style::default().fg(Color::Rgb(90, 90, 90))),
                Span::styled(l.to_string(), Style::default().fg(fg)),
            ])
        })
        .collect()
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "用法: pigma-trace <trace.ndjson> [--follow] [--speed 1.0] [--beat 1200] [--real]\n\
             \x20 --beat <ms>  一步一拍 (默认 1200ms, 覆盖事件时间轴 —— 快任务不会一闪而过)\n\
             \x20 --real       用 trace 里的真实 t_ms (适合本身就跨秒的任务)\n\
             键位: 空格 暂停/播放 · j/k 单步 · +/- 调速 · r 真执行当前步(看结果) · R 清空\n\
             \x20     f 文件面板(yazi-like) · n/p 选文件 · o 预览(带语法着色)\n\
             \x20     y 调用真 yazi(--chooser-file) 选文件后预览 · q 退出\n\
             \x20     文件面板根目录可用 LYCO_TRACE_ROOT 指定 (默认当前目录)"
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
    let mut cmds = load_cmds(&path);
    if lines.is_empty() {
        eprintln!("[pigma-trace] {} 无有效事件 (还没开始?)", path.display());
    }
    // `r` 键真执行当前步的输出 (结果面板)
    let mut result: Option<String> = None;
    // `f` 键: 类 yazi 的文件面板 (看生成了什么文件) + `o` 预览
    let mut files: Option<Vec<std::path::PathBuf>> = None;
    let mut file_cur: usize = 0;
    let mut viewing: Option<std::path::PathBuf> = None;

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
                    cmds = load_cmds(&path);
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

        // 当前行序号 (按拍时间轴回算)
        let cur = lines
            .iter()
            .rposition(|l| l.time.as_millis() as f64 <= sim_ms)
            .unwrap_or(0);

        let show = result.clone();
        let show_files = files.clone();
        let show_view = viewing.clone();
        if let Err(e) = terminal.draw(|f| {
            let area: Rect = f.area();
            // 底部面板: 预览 > 文件列表 > 执行结果
            let mut bottom: Option<Vec<Line<'static>>> = None;
            let mut title = String::new();
            if let Some(p) = &show_view {
                let mut v = preview_file(p);
                v.insert(0, Line::from(format!("── {} ──", p.display())));
                title = " 预览 (f 关面板, n/p 换文件) ".to_string();
                bottom = Some(v);
            } else if let Some(list) = &show_files {
                let cwd = std::env::current_dir().unwrap_or_default();
                let mut v = vec![Line::from(format!(
                    "── {} 个文件 · n/p 选择 · o 预览 · f 关闭 ──",
                    list.len()
                ))];
                for (i, p) in list.iter().enumerate() {
                    let rel = p.strip_prefix(&cwd).unwrap_or(p);
                    v.push(Line::from(format!(
                        "{} {}",
                        if i == file_cur { "\u{25B6}" } else { " " },
                        rel.display()
                    )));
                }
                title = " 文件面板 (yazi-like) ".to_string();
                bottom = Some(v);
            } else if let Some(out) = &show {
                bottom = Some(
                    out.lines()
                        .map(|l| Line::from(l.to_string()))
                        .collect::<Vec<_>>(),
                );
                title = " \u{25B6} 执行结果 (r 重跑, R 清空) ".to_string();
            }

            let (ly_area, res_area) = if bottom.is_some() && area.height > 8 {
                let h = area.height * 60 / 100;
                (
                    Rect { height: h, ..area },
                    Rect { y: area.y + h, height: area.height - h, ..area },
                )
            } else {
                (area, Rect::new(area.x, area.y, 0, 0))
            };
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
                ly_area,
            );
            if let Some(lines) = bottom {
                if res_area.height > 0 {
                    let p = Paragraph::new(lines)
                        .wrap(Wrap { trim: false })
                        .style(Style::default().fg(ratatui::style::Color::Rgb(175, 175, 175)))
                        .block(Block::default().title(title));
                    f.render_widget(p, res_area);
                }
            }
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
                    // r: 真执行当前步 (调进程, 看结果) —— 回应"不能 call 进程展示结果"
                    KeyCode::Char('r') => {
                        if let Some(c) = cmds.get(cur) {
                            if !c.is_empty() {
                                result = Some(run_cmd(c));
                            } else {
                                result = Some("(当前行不是可执行步骤)".to_string());
                            }
                        }
                    }
                    KeyCode::Char('R') => result = None,
                    // f: 类 yazi 文件面板 (默认扫当前目录; 可用 LYCO_TRACE_ROOT 指定)
                    KeyCode::Char('f') => {
                        if files.is_some() {
                            files = None;
                            viewing = None;
                        } else {
                            let root = std::env::var("LYCO_TRACE_ROOT")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
                            let list = scan_files(&root);
                            file_cur = 0;
                            viewing = None;
                            files = Some(list);
                        }
                    }
                    KeyCode::Char('n') => {
                        if let Some(ref f) = files {
                            if !f.is_empty() {
                                file_cur = (file_cur + 1).min(f.len() - 1);
                                viewing = None;
                            }
                        }
                    }
                    KeyCode::Char('p') => {
                        if files.is_some() {
                            file_cur = file_cur.saturating_sub(1);
                            viewing = None;
                        }
                    }
                    KeyCode::Char('o') => {
                        if let Some(ref f) = files {
                            viewing = f.get(file_cur).cloned();
                        }
                    }
                    // y: 直接调真 yazi (官方 --chooser-file), 退出后读回选中文件 → 预览
                    KeyCode::Char('y') => {
                        let choose = std::env::temp_dir().join("lyco_yazi_choice");
                        let _ = std::fs::remove_file(&choose);
                        // 先把终端交还给 yazi
                        let _ = disable_raw_mode();
                        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                        let root = std::env::var("LYCO_TRACE_ROOT")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
                        let _ = std::process::Command::new("yazi")
                            .args(["--chooser-file", &choose.to_string_lossy()])
                            .current_dir(&root)
                            .status();
                        let _ = execute!(std::io::stdout(), EnterAlternateScreen);
                        let _ = enable_raw_mode();
                        let _ = terminal.clear();
                        if let Ok(p) = std::fs::read_to_string(&choose) {
                            let p = p.trim();
                            if !p.is_empty() {
                                viewing = Some(std::path::PathBuf::from(p));
                                files = None;
                            }
                        }
                    }
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
