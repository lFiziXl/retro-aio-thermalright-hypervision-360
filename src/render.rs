//! Stage 3 — the 2D render engine (Stage 5: mascot integration).
//!
//! Composes one 480x480 frame per tick: black CRT background, the bouncing
//! mascot sprite (normal / smile / squints / smoke / angry / angry_scream)
//! in the center,
//! a retro white speech bubble with the quote above its head when the
//! quote is non-empty, a cyan terminal-style telemetry readout at the
//! bottom, and a retro "Pip-Boy" HUD pass (`apply_retro_hud`: CRT
//! scanlines plus a cyan targeting frame) over everything. The frame is
//! JPEG-encoded (quality 90) in memory and returned as bytes for
//! [`crate::screen::AioScreen::send_frame`].
//!
//! Text is drawn with `imageproc::drawing::draw_text_mut`, which in
//! imageproc 0.25 is generic over the `Canvas` trait and takes an
//! `ab_glyph` font (the rusttype-based API belongs to imageproc <= 0.23),
//! so the VT323 font is loaded via `ab_glyph::FontRef`.
//!
//! NOTE: imageproc 0.25 no longer ships `draw_image_mut`, so the mascot
//! sprite is composited with `image::imageops::overlay`. The canvas is
//! RGBA from the start, so the sprite's transparent pixels alpha-blend
//! onto the background correctly.

use std::time::{SystemTime, UNIX_EPOCH};

use image::{ImageBuffer, ImageEncoder, Rgb, Rgba, RgbaImage, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::screen::{HEIGHT, WIDTH};
use crate::telemetry::SystemStats;

/// Terminal font embedded at compile time (VT323, SIL Open Font License).
const FONT_DATA: &[u8] = include_bytes!("../assets/font.ttf");
/// Mascot sprites embedded at compile time.
const MASCOT_NORMAL: &[u8] = include_bytes!("../assets/normal.png");
const MASCOT_ANGRY: &[u8] = include_bytes!("../assets/angry.png");
const MASCOT_ANGRY_SCREAM: &[u8] = include_bytes!("../assets/angry_scream.png");
const MASCOT_SMILE: &[u8] = include_bytes!("../assets/smile.png");
const MASCOT_SQUINTS: &[u8] = include_bytes!("../assets/squints.png");
const MASCOT_SMOKE: &[u8] = include_bytes!("../assets/smoke.png");

const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);
const CYAN: Rgba<u8> = Rgba([0, 255, 255, 255]);
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
const BUBBLE_TEXT: Rgba<u8> = Rgba([0, 0, 0, 255]);

/// Telemetry text size, in px.
const FONT_PX: f32 = 40.0;
/// Speech-bubble text size, in px.
const BUBBLE_FONT_PX: f32 = 22.0;
/// Bottom margin for the telemetry line, in px. (12 px base + 5 px so the
/// text clears the cyan HUD frame border on the physical panel, + 7 px so
/// the readout sits higher on the panel than before.)
const BOTTOM_MARGIN: i32 = 24;
/// Padding inside the speech bubble, in px.
const BUBBLE_PAD: i32 = 8;
/// Gap between the bubble's bottom edge and the mascot's head, in px.
const BUBBLE_GAP: i32 = 10;
/// Minimum distance of the bubble from the screen edges, in px.
const BUBBLE_MARGIN: i32 = 8;
/// Matrix rain head-glyph size, in px.
const MATRIX_HEAD_PX: f32 = 16.0;
/// Bright color of a Matrix rain head glyph (bright cyan, matching the
/// HUD palette).
const MATRIX_HEAD: Rgba<u8> = Rgba([100, 255, 255, 255]);

// ── Matrix digital rain ────────────────────────────────────────────────

/// A tiny deterministic Xorshift64 PRNG — the same algorithm as `Prng`
/// in `main.rs`, copied here (and renamed) so this module stays
/// self-contained.
struct RenderPrng {
    state: u64,
}

impl RenderPrng {
    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    /// Uniform value in `[min, max]` (both inclusive).
    fn range(&mut self, min: u64, max: u64) -> u64 {
        min + (self.next() % (max - min + 1))
    }
}

/// Bright glyph pool for the head of a rain drop.
const MATRIX_HEAD_CHARS: [&str; 7] = ["0", "1", "A", "Z", "X", "7", "9"];

/// One falling drop of the Matrix rain.
pub struct MatrixDrop {
    /// Left edge of the 2 px-wide trail column.
    x: i32,
    /// Vertical position of the head, in px (can be above the panel
    /// while the drop is entering).
    y: f32,
    /// Fall speed, in px per frame.
    speed: f32,
    /// Trail length above the head, in px.
    tail_len: f32,
    /// Bright glyph rendered at the head of the drop.
    head_char: &'static str,
}

/// Matrix digital-rain state: a fixed pool of drops advanced once per
/// frame. Painted directly onto the black background inside
/// [`draw_frame`] so it stays behind the mascot, speech bubble,
/// telemetry and HUD.
pub struct MatrixRain {
    drops: Vec<MatrixDrop>,
    prng: RenderPrng,
}

impl MatrixRain {
    /// Create the rain with 25 drops scattered across (and above) the
    /// panel. Seeded from the wall clock so every boot looks different.
    pub fn new() -> Self {
        let mut prng = RenderPrng {
            state: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after the UNIX epoch")
                .as_nanos() as u64,
        };

        let drops = (0..25)
            .map(|_| MatrixDrop {
                x: prng.range(12, 460) as i32,
                y: (prng.range(0, 960) as f32) - 480.0,
                speed: prng.range(40, 120) as f32 / 10.0,
                tail_len: prng.range(40, 150) as f32,
                head_char: MATRIX_HEAD_CHARS[(prng.next() % MATRIX_HEAD_CHARS.len() as u64) as usize],
            })
            .collect();

        Self { drops, prng }
    }

    /// Advance every drop by one frame and paint it onto `frame`:
    ///
    /// * the trail is a 2 px-wide vertical column of fading cyan pixels
    ///   written with direct pixel access — no text rasterization, which
    ///   is what keeps the effect near-0% CPU;
    /// * the head is a single bright glyph via `draw_text_mut` (one
    ///   rasterization per drop, not per trail pixel).
    ///
    /// Out-of-bounds pixels are never written: the trail loop checks the
    /// row against the panel bounds before `put_pixel`, and
    /// `draw_text_mut` clamps glyph pixels to the canvas itself.
    pub fn tick(&mut self, frame: &mut RgbaImage, font: &impl ab_glyph::Font) {
        let head_scale = ab_glyph::PxScale { x: MATRIX_HEAD_PX, y: MATRIX_HEAD_PX };

        for drop in &mut self.drops {
            // 1. Advance; recycle once the trail has fully left the panel.
            drop.y += drop.speed;
            if drop.y - drop.tail_len > 480.0 {
                drop.y = -(50.0 + self.prng.range(0, 200) as f32);
                drop.x = self.prng.range(12, 460) as i32;
                drop.speed = self.prng.range(40, 120) as f32 / 10.0;
                drop.head_char =
                    MATRIX_HEAD_CHARS[(self.prng.next() % MATRIX_HEAD_CHARS.len() as u64) as usize];
            }

            // 2. Trail — direct pixel writes with a linear cyan fade.
            //    `x` is constrained to [12, 460] by construction, so the
            //    second column (x + 1) is inside the 480 px panel and both
            //    columns stay clear of the 10 px-inset HUD frame border.
            if (0..WIDTH as i32).contains(&drop.x) && (0..WIDTH as i32).contains(&(drop.x + 1)) {
                let head_row = drop.y as i32;
                let len = drop.tail_len as i32;
                for dy in 0..len {
                    let py = head_row - dy;
                    // Strictly clip the trail to the inside of the HUD
                    // frame (10 px inset + 1 px border): rows 12..=467.
                    if py < 12 || py > 467 {
                        continue;
                    }
                    let ratio = 1.0 - dy as f32 / drop.tail_len;
                    let fade = (200.0 * ratio).min(200.0) as u8;
                    let color = Rgba([0, fade, fade, 255]);
                    frame.put_pixel(drop.x as u32, py as u32, color);
                    frame.put_pixel(drop.x as u32 + 1, py as u32, color);
                }
            }

            // 3. Head — one bright glyph, the only per-drop rasterization.
            //    Only draw when the head sits inside the frame: the 16 px
            //    glyph must not overlap the top/bottom border (y in
            //    [12, 455] keeps a full 16 px glyph below row 12 and above
            //    row 467).
            if drop.y >= 12.0 && drop.y <= 455.0 {
                draw_text_mut(frame, MATRIX_HEAD, drop.x - 3, drop.y as i32, head_scale, font, drop.head_char);
            }
        }
    }
}

/// Compose one 480x480 frame and return the JPEG-encoded bytes (quality 90).
///
/// * `stats` — telemetry snapshot for the bottom readout.
/// * `mascot_state` — `"normal"`, `"smile"`, `"squints"`, `"smoke"`,
///   `"angry"` (silent, no quote), or `"angry_scream"` (screaming, quote
///   visible); picks the sprite.
/// * `quote` — if non-empty, a white speech bubble with black text is
///   drawn above the mascot's head.
/// * `matrix` — the Matrix digital-rain state; advanced and painted onto
///   the black background right away, so the rain sits behind the mascot,
///   speech bubble, telemetry and HUD.
pub fn draw_frame(
    stats: &SystemStats,
    mascot_state: &str,
    quote: &str,
    matrix: &mut MatrixRain,
) -> Vec<u8> {
    // 1. Load the embedded font (the Matrix rain heads need it too).
    let font = ab_glyph::FontRef::try_from_slice(FONT_DATA)
        .expect("embedded VT323 font must be a valid TrueType");

    // 2. Black canvas at the panel's native geometry (RGBA so the mascot
    //    sprite can be alpha-composited on top).
    let mut img: RgbaImage = ImageBuffer::from_pixel(WIDTH as u32, HEIGHT as u32, BLACK);

    // 3. Matrix digital rain straight onto the black background — trails
    //    are plain pixel writes, one head glyph per drop — so the mascot,
    //    speech bubble, telemetry and HUD all render on top of it.
    matrix.tick(&mut img, &font);

    // 4. Mascot sprite in the center with a slow sine-wave vertical bounce
    //    driven by wall-clock time (~1.26 s per full bounce cycle).
    let mascot_data = match mascot_state {
        "angry" => MASCOT_ANGRY,
        "angry_scream" => MASCOT_ANGRY_SCREAM,
        "smile" => MASCOT_SMILE,
        "squints" => MASCOT_SQUINTS,
        "smoke" => MASCOT_SMOKE,
        _ => MASCOT_NORMAL,
    };
    let mascot = image::load_from_memory(mascot_data)
        .expect("embedded mascot sprite must decode")
        .to_rgba8();

    let bounce = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the UNIX epoch")
        .as_millis() as f64
        / 500.0)
        .sin()
        * 10.0;

    let (mascot_w, mascot_h) = mascot.dimensions();
    let mascot_x = (WIDTH as i64 / 2).saturating_sub(mascot_w as i64 / 2);
    let mascot_y = (HEIGHT as i64 / 2).saturating_sub(mascot_h as i64 / 2) + bounce as i64;
    image::imageops::overlay(&mut img, &mascot, mascot_x, mascot_y);

    // 5. Retro pixelated speech bubble above the mascot's head (only when
    //    the quote is non-empty): white rectangle, black VT323 text.
    if !quote.is_empty() {
        let scale = ab_glyph::PxScale { x: BUBBLE_FONT_PX, y: BUBBLE_FONT_PX };
        let (text_w, text_h) = text_size(scale, &font, quote);
        let bubble_w = text_w as i32 + BUBBLE_PAD * 2;
        let bubble_h = text_h as i32 + BUBBLE_PAD * 2;

        let bubble_x = (WIDTH as i32 / 2)
            .saturating_sub(bubble_w / 2)
            .max(BUBBLE_MARGIN)
            .min(WIDTH as i32 - BUBBLE_MARGIN - bubble_w);
        let bubble_y = (mascot_y as i32 - BUBBLE_GAP - bubble_h).max(BUBBLE_MARGIN);

        draw_filled_rect_mut(
            &mut img,
            Rect::at(bubble_x, bubble_y).of_size(bubble_w as u32, bubble_h as u32),
            WHITE,
        );
        draw_text_mut(
            &mut img,
            BUBBLE_TEXT,
            bubble_x + BUBBLE_PAD,
            bubble_y + BUBBLE_PAD,
            scale,
            &font,
            quote,
        );
    }

    // 6. Telemetry readout at the bottom, retro terminal cyan, centered.
    //    Rotates every 7 s between the CPU, GPU and RAM readouts.
    let scale = ab_glyph::PxScale { x: FONT_PX, y: FONT_PX };
    let state = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 7)
        % 3;
    let line = match state {
        0 => format!("CPU: {:.0}C | {:.0} MHz", stats.cpu_temp, stats.cpu_freq_mhz),
        1 => format!("GPU: {:.0}C | VRAM: {:.1} GB", stats.gpu_temp, stats.gpu_vram_used_gb),
        _ => format!("RAM: {:.1} GB | {:.1} GB", stats.ram_used_gb, stats.ram_total_gb),
    };

    let h = text_size(scale, &font, &line).1;
    let y = HEIGHT as i32 - BOTTOM_MARGIN - h as i32;
    draw_text_mut(
        &mut img,
        CYAN,
        centered_x(&line, scale, &font),
        y,
        scale,
        &font,
        &line,
    );

    // 7. Flatten the RGBA buffer to RGB (the mascot was already
    //    alpha-blended onto the opaque background above) and apply the
    //    retro "Pip-Boy" HUD pass — CRT scanlines + cyan targeting frame —
    //    as the very last step, after the mascot, speech bubble, and
    //    telemetry are already drawn.
    let mut rgb_img = image::DynamicImage::ImageRgba8(img).into_rgb8();
    apply_retro_hud(&mut rgb_img);

    // 8. JPEG-encode in memory (quality 90) for the panel. JPEG has no
    //    alpha channel, so we work on the RGB buffer above.
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .write_image(
            rgb_img.as_raw(),
            WIDTH as u32,
            HEIGHT as u32,
            image::ExtendedColorType::Rgb8,
        )
        .expect("JPEG encoding of a valid 480x480 RGB buffer must not fail");
    jpeg
}

/// Retro "Pip-Boy" HUD pass, applied to the final RGB frame after the
/// mascot, speech bubble, and telemetry are already drawn:
///
/// 1. CRT scanlines — every 3rd row (y % 3 == 0) loses ~30% of its light.
/// 2. Sci-fi frame — a 1px cyan border inset 10px from the edges,
///    3px-thick targeting-HUD corner brackets (~30px long), and a small
///    filled accent block at the top center of the border.
///
/// Pure pixel math + `imageproc::drawing` fills — zero external assets.
fn apply_retro_hud(frame: &mut RgbImage) {
    const CYAN_FRAME: Rgb<u8> = Rgb([0, 255, 255]);
    const INSET: i32 = 10;
    const BRACKET_LEN: i32 = 30;
    const BRACKET_THICK: i32 = 3;
    const ACCENT_W: i32 = 40;
    const ACCENT_H: i32 = 4;

    let w = frame.width() as i32;
    let h = frame.height() as i32;
    let x0 = INSET;
    let y0 = INSET;
    let x1 = w - INSET - 1; // right border column
    let y1 = h - INSET - 1; // bottom border row

    // 1. CRT scanlines: every 3rd row (y % 3 == 0) darkened by ~30%.
    for (_, y, px) in frame.enumerate_pixels_mut() {
        if y % 3 == 0 {
            let p = *px;
            *px = Rgb([
                (p[0] as f32 * 0.7) as u8,
                (p[1] as f32 * 0.7) as u8,
                (p[2] as f32 * 0.7) as u8,
            ]);
        }
    }

    // 2. Thin 1px cyan border, slightly inset from the screen edges.
    draw_hollow_rect_mut(
        frame,
        Rect::at(x0, y0).of_size((w - 2 * INSET) as u32, (h - 2 * INSET) as u32),
        CYAN_FRAME,
    );

    // 3. 3px-thick targeting-HUD corner brackets, ~30px long, hugging the
    //    four corners of the border.
    // Top-left.
    draw_filled_rect_mut(frame, Rect::at(x0, y0).of_size(BRACKET_LEN as u32, BRACKET_THICK as u32), CYAN_FRAME);
    draw_filled_rect_mut(frame, Rect::at(x0, y0).of_size(BRACKET_THICK as u32, BRACKET_LEN as u32), CYAN_FRAME);
    // Top-right.
    draw_filled_rect_mut(frame, Rect::at(x1 - BRACKET_LEN + 1, y0).of_size(BRACKET_LEN as u32, BRACKET_THICK as u32), CYAN_FRAME);
    draw_filled_rect_mut(frame, Rect::at(x1 - BRACKET_THICK + 1, y0).of_size(BRACKET_THICK as u32, BRACKET_LEN as u32), CYAN_FRAME);
    // Bottom-left.
    draw_filled_rect_mut(frame, Rect::at(x0, y1 - BRACKET_THICK + 1).of_size(BRACKET_LEN as u32, BRACKET_THICK as u32), CYAN_FRAME);
    draw_filled_rect_mut(frame, Rect::at(x0, y1 - BRACKET_LEN + 1).of_size(BRACKET_THICK as u32, BRACKET_LEN as u32), CYAN_FRAME);
    // Bottom-right.
    draw_filled_rect_mut(frame, Rect::at(x1 - BRACKET_LEN + 1, y1 - BRACKET_THICK + 1).of_size(BRACKET_LEN as u32, BRACKET_THICK as u32), CYAN_FRAME);
    draw_filled_rect_mut(frame, Rect::at(x1 - BRACKET_THICK + 1, y1 - BRACKET_LEN + 1).of_size(BRACKET_THICK as u32, BRACKET_LEN as u32), CYAN_FRAME);

    // 4. Small filled accent block at the top center of the border.
    draw_filled_rect_mut(
        frame,
        Rect::at(w / 2 - ACCENT_W / 2, y0 - ACCENT_H / 2).of_size(ACCENT_W as u32, ACCENT_H as u32),
        CYAN_FRAME,
    );
}

/// Horizontally center a line within the 480 px panel width.
fn centered_x(text: &str, scale: ab_glyph::PxScale, font: &ab_glyph::FontRef) -> i32 {
    let (w, _) = text_size(scale, font, text);
    (WIDTH as i32 / 2).saturating_sub(w as i32 / 2)
}
