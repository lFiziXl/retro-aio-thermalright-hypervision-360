//! retro_aio — AIO panel driver + hardware telemetry + 2D render engine.
//!
//! `main` opens the AIO panel and runs an infinite render loop capped at
//! ~30 FPS. Each tick reuses a cached telemetry snapshot
//! ([`telemetry::get_stats`]) that is refreshed only once per second, drives
//! a GPU-driven mascot state machine (overload quotes loaded from
//! `assets/quotes.txt` at compile time — no LLM),
//! renders a 480x480 CRT-style frame ([`render::draw_frame`]) and pushes
//! the JPEG to the panel.
//!
//! Mascot state machine:
//! * Overloaded (GPU busy ≥ 80 % **or** RAM ≥ 60 %) → the mascot screams:
//!   `angry_scream` sprite plus a quote from `assets/quotes.txt` for 10 s,
//!   then the silent `angry` sprite (no quote) until the next quote comes
//!   around (every 6 min).
//! * GPU busy from 50 % up to (but not including) 80 % → `smile` sprite.
//! * Otherwise → the mascot idles in `normal` / `smoke` / `squints`,
//!   picking a new pose every 30–240 s via a tiny Xorshift PRNG.

mod render;
mod screen;
mod telemetry;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use screen::{AioError, AioScreen, AIO_PID, AIO_VID, HEIGHT, WIDTH};

/// Interval between frames — 33 ms caps the loop at ~30 FPS, keeping the
/// mascot bounce smooth while halving the render-loop CPU cost.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);


/// A tiny deterministic Xorshift64 PRNG for randomized idle timing — no
/// `rand` dependency, no allocation, no threads.
struct Prng {
    state: u64,
}

impl Prng {
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

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Parse the compile-time-embedded `assets/quotes.txt` into
/// `(ru_quotes, en_quotes)`.
///
/// The file has a `# Russian Quotes` section and a `# English Quotes`
/// section; every non-empty, non-comment line belongs to the most recently
/// seen section header (default before any header: English).
fn load_quotes() -> (Vec<&'static str>, Vec<&'static str>) {
    const ALL_QUOTES: &str = include_str!("../assets/quotes.txt");
    let mut ru_quotes: Vec<&'static str> = Vec::new();
    let mut en_quotes: Vec<&'static str> = Vec::new();
    let mut current_lang = "en"; // Default fallback

    for line in ALL_QUOTES.lines().map(|l| l.trim()) {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("# Russian Quotes") {
            current_lang = "ru";
            continue;
        }
        if line.starts_with("# English Quotes") {
            current_lang = "en";
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        if current_lang == "ru" {
            ru_quotes.push(line);
        } else {
            en_quotes.push(line);
        }
    }

    (ru_quotes, en_quotes)
}

/// Open the panel and run the live telemetry render loop forever.
fn run() -> Result<(), AioError> {
    println!(
        "retro_aio — AIO panel demo ({AIO_VID:04x}:{AIO_PID:04x}, {WIDTH}x{HEIGHT} JPEG)"
    );

    let screen = AioScreen::new(AIO_VID, AIO_PID)?;
    println!("handshake OK — PM={} SUB={}", screen.pm(), screen.sub());

    // Prime the cached `sysinfo::System` and the randomized idle scheduler
    // once, before the render loop starts.
    let mut sys = sysinfo::System::new();
    // Prime with the full CPU refresh (usage + frequency) so the very first
    // snapshot already carries a valid `cpu_freq_mhz`.
    sys.refresh_cpu_all();
    sys.refresh_memory();
    let mut prng = Prng {
        state: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64,
    };
    let mut last_telemetry_time = SystemTime::UNIX_EPOCH;
    let mut cached_stats = telemetry::get_stats(&mut sys);
    let mut idle_timer = 0;
    let mut current_idle_state = "normal";
    // Scream lock: wall-clock (s) at which the current 10 s scream window
    // ends. While `now_secs < quote_end_time` the mascot is locked to the
    // `angry_scream` sprite + quote, even if load dips below the threshold
    // for a tick — this kills the flicker when GPU hovers at the 80 % edge.
    let mut quote_end_time: u64 = 0;
    // Quote index — incremented on every new scream cycle so consecutive
    // overloads walk through the quote pool instead of repeating one quote.
    let mut quote_idx: usize = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as usize;

    // Overload quotes, embedded at compile time from assets/quotes.txt —
    // the system locale decides which pool the mascot speaks.
    let (ru_quotes, en_quotes) = load_quotes();
    let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
    let quotes = if locale.starts_with("ru") { ru_quotes } else { en_quotes };

    if quotes.is_empty() {
        panic!("No quotes found for the selected locale!");
    }

    // Matrix digital rain behind the mascot — a fixed pool of 25 drops
    // (fading pixel trails + one head glyph each), advanced inside
    // `render::draw_frame`.
    let mut matrix_rain = crate::render::MatrixRain::new();

    loop {
        let now = SystemTime::now();
        let now_secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs();

        // 1. Throttle telemetry updates to once per second to save CPU
        if now.duration_since(last_telemetry_time).unwrap().as_secs() >= 1 {
            cached_stats = telemetry::get_stats(&mut sys);
            last_telemetry_time = now;
        }
        let stats = &cached_stats;

        // 2. State Machine Logic
        let mascot_state;
        let quote; // assigned exactly once in every branch below
        let ram_ratio = stats.ram_used_gb / stats.ram_total_gb;

        let is_overloaded = stats.gpu_busy_percent >= 80.0 || ram_ratio >= 0.60;

        // If overloaded and cooldown has passed (360 seconds), trigger a new 10-second scream
        if is_overloaded && now_secs >= quote_end_time + 350 {
            quote_end_time = now_secs + 10;
            quote_idx = quote_idx.wrapping_add(1);
        }

        // 1. Strict 10-second lock for the scream (prevents flashing if load drops for a second)
        if now_secs < quote_end_time {
            mascot_state = "angry_scream";
            quote = quotes[quote_idx % quotes.len()];
        }
        // 2. Silent angry if still overloaded after the scream
        else if is_overloaded {
            mascot_state = "angry";
            quote = "";
        }
        // 3. Smile band (50% to 80%)
        else if stats.gpu_busy_percent >= 50.0 {
            mascot_state = "smile";
            quote = "";
        }
        // 4. Idle state
        else {
            quote = "";
            if now_secs >= idle_timer {
                let states = ["normal", "smoke", "squints"];
                current_idle_state = states[(prng.next() % 3) as usize];
                idle_timer = now_secs + prng.range(30, 240);
            }
            mascot_state = current_idle_state;
        }

        // 3. Render one 480x480 JPEG frame.
        let jpeg = render::draw_frame(stats, mascot_state, quote, &mut matrix_rain);

        // 4. Push the frame to the panel. (No per-frame logging — the daemon
        //    runs silently in the background.)
        screen.send_frame(&jpeg)?;

        // 5. 33 ms per tick — ~30 FPS.
        std::thread::sleep(FRAME_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::load_quotes;

    /// Both quote pools must be populated with the expected number of
    /// quotes, and they must not be mixed up: every Russian quote is
    /// Cyrillic, and no English quote contains a single Cyrillic letter.
    #[test]
    fn test_quotes_parsing() {
        let (ru_quotes, en_quotes) = load_quotes();

        assert_eq!(
            ru_quotes.len(),
            17,
            "expected exactly 17 Russian quotes, got {}",
            ru_quotes.len()
        );
        assert_eq!(
            en_quotes.len(),
            17,
            "expected exactly 17 English quotes, got {}",
            en_quotes.len()
        );

        for q in &ru_quotes {
            assert!(
                q.chars().any(is_cyrillic),
                "Russian pool contains a non-Cyrillic quote: {q:?}"
            );
        }
        for q in &en_quotes {
            assert!(
                !q.chars().any(is_cyrillic),
                "English pool contains a Cyrillic quote: {q:?}"
            );
        }
    }

    /// `char::is_cyrillic` does not exist in std, so check the Cyrillic
    /// Unicode blocks manually (Cyrillic, Supplement, Extended-A/B).
    fn is_cyrillic(c: char) -> bool {
        matches!(c as u32, 0x0400..=0x052F | 0x2C00..=0x2C5F | 0xA640..=0xA69F)
    }
}
