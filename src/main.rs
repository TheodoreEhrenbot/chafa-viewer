use std::collections::HashMap;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
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
use tempfile::NamedTempFile;

type CropKey = (u32, u32, u32, u32, u16, u16); // (x, y, w, h, cols, rows)

struct App {
    image_path: PathBuf,
    image: DynamicImage,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
    cache: HashMap<CropKey, Vec<u8>>,
    chafa_output: Vec<u8>,
    needs_render: bool,
}

impl App {
    fn new(image_path: PathBuf) -> Result<Self> {
        let image = image::open(&image_path)
            .with_context(|| format!("Failed to open image: {}", image_path.display()))?;
        Ok(Self {
            image_path,
            image,
            zoom: 1.0,
            pan_x: 0.5,
            pan_y: 0.5,
            cache: HashMap::new(),
            chafa_output: Vec::new(),
            needs_render: true,
        })
    }

    fn clamp_pan(&mut self) {
        let half = 0.5 / self.zoom;
        self.pan_x = self.pan_x.clamp(half, 1.0 - half);
        self.pan_y = self.pan_y.clamp(half, 1.0 - half);
    }

    fn zoom_in(&mut self) {
        self.zoom *= 1.5;
        self.clamp_pan();
    }

    fn zoom_out(&mut self) {
        self.zoom /= 1.5;
        if self.zoom < 1.0 {
            self.zoom = 1.0;
            self.pan_x = 0.5;
            self.pan_y = 0.5;
        } else {
            self.clamp_pan();
        }
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        let step = 0.1 / self.zoom;
        self.pan_x += dx * step;
        self.pan_y += dy * step;
        self.clamp_pan();
    }

    fn render_view(&mut self, cols: u16, rows: u16) -> Result<()> {
        let img_w = self.image.width();
        let img_h = self.image.height();

        let vp_w = ((img_w as f64) / self.zoom).round() as u32;
        let vp_h = ((img_h as f64) / self.zoom).round() as u32;

        let cx = (self.pan_x * img_w as f64) as u32;
        let cy = (self.pan_y * img_h as f64) as u32;
        let x = cx.saturating_sub(vp_w / 2).min(img_w.saturating_sub(vp_w));
        let y = cy.saturating_sub(vp_h / 2).min(img_h.saturating_sub(vp_h));
        let w = vp_w.min(img_w.saturating_sub(x));
        let h = vp_h.min(img_h.saturating_sub(y));

        self.needs_render = false;

        if w == 0 || h == 0 {
            self.chafa_output = b"(viewport out of bounds)\r\n".to_vec();
            return Ok(());
        }

        let key: CropKey = (x, y, w, h, cols, rows);
        if let Some(cached) = self.cache.get(&key) {
            self.chafa_output = cached.clone();
            return Ok(());
        }

        let cropped = self.image.crop_imm(x, y, w, h);
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

        self.cache.insert(key, normalized.clone());
        self.chafa_output = normalized;
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('h') | KeyCode::Left => self.pan(-1.0, 0.0),
            KeyCode::Char('l') | KeyCode::Right => self.pan(1.0, 0.0),
            KeyCode::Char('k') | KeyCode::Up => self.pan(0.0, -1.0),
            KeyCode::Char('j') | KeyCode::Down => self.pan(0.0, 1.0),
            KeyCode::Char('i') | KeyCode::Char('=') | KeyCode::Char('+') => self.zoom_in(),
            KeyCode::Char('o') | KeyCode::Char('-') => self.zoom_out(),
            _ => return true,
        }
        self.needs_render = true;
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
            app.render_view(cols, image_rows)?;
            queue!(stdout, Clear(ClearType::All))?;
        }

        draw_frame(&mut stdout, app, cols, rows)?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if !app.handle_key(key) {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => {
                    app.needs_render = true;
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
