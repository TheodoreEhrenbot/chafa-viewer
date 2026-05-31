use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use image::DynamicImage;
use rayon::prelude::*;
use tempfile::NamedTempFile;

const CACHE_KEYS: &[&str] = &[
    "h", "hh", "hhh", "j", "jj", "jjj", "k", "kk", "kkk", "l", "ll", "lll", "i", "ii", "iii",
    "o", "oo", "ooo",
];

fn clamp_pan(zoom: f64, pan_x: f64, pan_y: f64) -> (f64, f64) {
    let half = 0.5 / zoom;
    (pan_x.clamp(half, 1.0 - half), pan_y.clamp(half, 1.0 - half))
}

fn apply_single_key(zoom: f64, pan_x: f64, pan_y: f64, key: char) -> (f64, f64, f64) {
    match key {
        'h' => {
            let step = 0.1 / zoom;
            let (px, py) = clamp_pan(zoom, pan_x - step, pan_y);
            (zoom, px, py)
        }
        'l' => {
            let step = 0.1 / zoom;
            let (px, py) = clamp_pan(zoom, pan_x + step, pan_y);
            (zoom, px, py)
        }
        'k' => {
            let step = 0.1 / zoom;
            let (px, py) = clamp_pan(zoom, pan_x, pan_y - step);
            (zoom, px, py)
        }
        'j' => {
            let step = 0.1 / zoom;
            let (px, py) = clamp_pan(zoom, pan_x, pan_y + step);
            (zoom, px, py)
        }
        'i' => {
            let z = zoom * 1.5;
            let (px, py) = clamp_pan(z, pan_x, pan_y);
            (z, px, py)
        }
        'o' => {
            let z = zoom / 1.5;
            if z < 1.0 {
                (1.0, 0.5, 0.5)
            } else {
                let (px, py) = clamp_pan(z, pan_x, pan_y);
                (z, px, py)
            }
        }
        _ => (zoom, pan_x, pan_y),
    }
}

fn apply_key_sequence(mut zoom: f64, mut pan_x: f64, mut pan_y: f64, keys: &str) -> (f64, f64, f64) {
    for key in keys.chars() {
        let (z, px, py) = apply_single_key(zoom, pan_x, pan_y, key);
        zoom = z;
        pan_x = px;
        pan_y = py;
    }
    (zoom, pan_x, pan_y)
}

fn render_view(image: &DynamicImage, zoom: f64, pan_x: f64, pan_y: f64, cols: u16, rows: u16) -> Result<Vec<u8>> {
    let img_w = image.width();
    let img_h = image.height();

    let vp_w = ((img_w as f64) / zoom).round() as u32;
    let vp_h = ((img_h as f64) / zoom).round() as u32;

    let cx = (pan_x * img_w as f64) as u32;
    let cy = (pan_y * img_h as f64) as u32;
    let x = cx.saturating_sub(vp_w / 2).min(img_w.saturating_sub(vp_w));
    let y = cy.saturating_sub(vp_h / 2).min(img_h.saturating_sub(vp_h));
    let w = vp_w.min(img_w.saturating_sub(x));
    let h = vp_h.min(img_h.saturating_sub(y));

    if w == 0 || h == 0 {
        return Ok(b"(viewport out of bounds)\r\n".to_vec());
    }

    let cropped = image.crop_imm(x, y, w, h);
    let tmp = NamedTempFile::with_suffix(".png")?;
    cropped.save(tmp.path())?;

    let output = Command::new("chafa")
        .args(["--size", &format!("{}x{}", cols, rows)])
        .arg("--animate=off")
        .arg(tmp.path())
        .output()
        .context("Failed to run chafa — is it installed? https://hpjansson.org/chafa/")?;

    let raw = output.stdout;
    let mut normalized = Vec::with_capacity(raw.len() + raw.len() / 20);
    let mut prev = 0u8;
    for &b in &raw {
        if b == b'\n' && prev != b'\r' {
            normalized.push(b'\r');
        }
        normalized.push(b);
        prev = b;
    }

    Ok(normalized)
}

struct App {
    image_path: PathBuf,
    image: Arc<DynamicImage>,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
    cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    cancel: Arc<AtomicBool>,
    chafa_output: Vec<u8>,
    needs_render: bool,
    needs_clear: bool,
}

impl App {
    fn new(image_path: PathBuf) -> Result<Self> {
        let image = image::open(&image_path)
            .with_context(|| format!("Failed to open image: {}", image_path.display()))?;
        Ok(Self {
            image_path,
            image: Arc::new(image),
            zoom: 1.0,
            pan_x: 0.5,
            pan_y: 0.5,
            cache: Arc::new(Mutex::new(HashMap::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            chafa_output: Vec::new(),
            needs_render: true,
            needs_clear: false,
        })
    }

    fn start_prefetch(&mut self, cols: u16, rows: u16) {
        // Signal any in-flight workers to stop
        self.cancel.store(true, Ordering::Relaxed);
        // Fresh token so new workers aren't pre-cancelled
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Arc::clone(&cancel);
        self.cache.lock().unwrap().clear();

        let cache = Arc::clone(&self.cache);
        let image = Arc::clone(&self.image);
        let zoom = self.zoom;
        let pan_x = self.pan_x;
        let pan_y = self.pan_y;

        std::thread::spawn(move || {
            CACHE_KEYS.par_iter().for_each(|&key| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let (z, px, py) = apply_key_sequence(zoom, pan_x, pan_y, key);
                if let Ok(output) = render_view(&image, z, px, py, cols, rows) {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(mut c) = cache.lock() {
                        c.insert(key.to_string(), output);
                    }
                }
            });
        });
    }

    fn handle_key(&mut self, key: KeyEvent, cols: u16, rows: u16) -> bool {
        let single_key = match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('h') | KeyCode::Left => 'h',
            KeyCode::Char('l') | KeyCode::Right => 'l',
            KeyCode::Char('k') | KeyCode::Up => 'k',
            KeyCode::Char('j') | KeyCode::Down => 'j',
            KeyCode::Char('i') | KeyCode::Char('=') | KeyCode::Char('+') => 'i',
            KeyCode::Char('o') | KeyCode::Char('-') => 'o',
            _ => return true,
        };

        let cached = {
            let cache = self.cache.lock().unwrap();
            cache.get(&single_key.to_string()).cloned()
        };

        let (z, px, py) = apply_single_key(self.zoom, self.pan_x, self.pan_y, single_key);
        self.zoom = z;
        self.pan_x = px;
        self.pan_y = py;

        if let Some(output) = cached {
            self.chafa_output = output;
            self.needs_render = false;
            self.needs_clear = true;
            self.start_prefetch(cols, rows);
        } else {
            // Cancel stale prefetch; new one starts after the render
            self.cancel.store(true, Ordering::Relaxed);
            self.cache.lock().unwrap().clear();
            self.needs_render = true;
        }

        true
    }
}

fn draw_frame(stdout: &mut impl Write, app: &App, cols: u16, rows: u16) -> Result<()> {
    queue!(stdout, MoveTo(0, 0))?;
    stdout.write_all(&app.chafa_output)?;

    let filename = app
        .image_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let zoom_pct = (app.zoom * 100.0).round() as u32;
    let status = format!(
        " {filename}  zoom:{zoom_pct}%  i/o/+/-:zoom  hjkl/arrows:pan  q:quit"
    );
    let padded = format!("{:<width$}", status, width = cols as usize);
    let padded = padded.chars().take(cols as usize).collect::<String>();

    queue!(
        stdout,
        MoveTo(0, rows.saturating_sub(1)),
        SetBackgroundColor(Color::DarkGrey),
        SetForegroundColor(Color::White),
        Print(&padded),
        ResetColor,
    )?;

    stdout.flush()?;
    Ok(())
}

fn run_app(app: &mut App) -> Result<()> {
    let mut stdout = io::stdout();

    loop {
        let (cols, rows) = size()?;
        let image_rows = rows.saturating_sub(1);

        if app.needs_render && image_rows > 0 && cols > 0 {
            let output = render_view(&app.image, app.zoom, app.pan_x, app.pan_y, cols, image_rows)?;
            app.chafa_output = output;
            app.needs_render = false;
            app.needs_clear = true;
            app.start_prefetch(cols, image_rows);
        }

        if app.needs_clear {
            queue!(stdout, Clear(ClearType::All))?;
            app.needs_clear = false;
        }

        draw_frame(&mut stdout, app, cols, rows)?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if !app.handle_key(key, cols, image_rows) {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => {
                    app.needs_render = true;
                    app.needs_clear = true;
                    app.cancel.store(true, Ordering::Relaxed);
                    app.cache.lock().unwrap().clear();
                    queue!(stdout, Clear(ClearType::All))?;
                }
                _ => {}
            }
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        bail!("Usage: chafa-viewer <image-file>");
    }
    let image_path = PathBuf::from(&args[1]);

    Command::new("chafa")
        .arg("--version")
        .output()
        .context("chafa not found — install it from https://hpjansson.org/chafa/")?;

    let mut app = App::new(image_path)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;

    let result = run_app(&mut app);

    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen, Show)?;

    result
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
