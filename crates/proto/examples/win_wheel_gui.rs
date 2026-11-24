#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2};
use nusb::transfer::{Buffer, In, Interrupt, Out};
use nusb::MaybeFuture;
use std::ffi::c_void;
use std::f32::consts::PI;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const CONFIG_FILE: &str = "wheel_studio.cfg";

const RECEIVER_IDS: &[(u16, u16)] = &[
    (0x045e, 0x0719),
    (0x045e, 0x0291),
    (0x045e, 0x02a9),
];

static START_TIME: OnceLock<Instant> = OnceLock::new();
static LOG_ENTRIES: Mutex<Vec<LogEntry>> = Mutex::new(Vec::new());

#[derive(Clone)]
struct LogEntry {
    time_s: f32,
    text: String,
    color: Color32,
}

fn add_log(text: impl Into<String>, color: Color32) {
    let start = START_TIME.get_or_init(Instant::now);
    let time_s = start.elapsed().as_secs_f32();
    let mut logs = LOG_ENTRIES.lock().unwrap();
    if logs.len() > 600 {
        logs.remove(0);
    }
    logs.push(LogEntry {
        time_s,
        text: text.into(),
        color,
    });
}

const COL_DBG: Color32 = Color32::from_rgb(150, 150, 255);
const COL_OK: Color32 = Color32::from_rgb(0, 230, 118);
const COL_WARN: Color32 = Color32::from_rgb(255, 171, 0);
const COL_ERR: Color32 = Color32::from_rgb(255, 59, 48);
const COL_INFO: Color32 = Color32::from_rgb(0, 229, 255);

// ============================================================================
//  FFB ENGINE
// ============================================================================
const MAX_EFFECTS: usize = 64;

const ET_CONST: u32 = 1;
const ET_RAMP: u32 = 2;
const ET_SQR: u32 = 3;
const ET_SINE: u32 = 4;
const ET_TRNGL: u32 = 5;
const ET_STUP: u32 = 6;
const ET_STDN: u32 = 7;
const ET_SPRNG: u32 = 8;
const ET_DMPR: u32 = 9;
const ET_INRT: u32 = 10;
const ET_FRCTN: u32 = 11;
const ET_CSTM: u32 = 12;

const DAMPER_FULL_VEL: f32 = 3.0;
const FRICTION_FULL_VEL: f32 = 0.25;
const INERTIA_FULL_ACC: f32 = 40.0;

const AUTO_TARGET: f32 = 0.70;
const AUTO_FLOOR: f32 = 0.25;
const AUTO_MAX: f32 = 1.80;

fn etype_name(t: u32) -> &'static str {
    match t {
        ET_CONST => "CONSTANT",
        ET_RAMP => "RAMP",
        ET_SQR => "SQUARE",
        ET_SINE => "SINE",
        ET_TRNGL => "TRIANGLE",
        ET_STUP => "SAW-UP",
        ET_STDN => "SAW-DOWN",
        ET_SPRNG => "SPRING",
        ET_DMPR => "DAMPER",
        ET_INRT => "INERTIA",
        ET_FRCTN => "FRICTION",
        ET_CSTM => "CUSTOM",
        _ => "?",
    }
}

fn is_periodic(t: u32) -> bool {
    (ET_SQR..=ET_STDN).contains(&t)
}

fn is_condition(t: u32) -> bool {
    (ET_SPRNG..=ET_FRCTN).contains(&t)
}

#[inline]
fn s16(v: i32) -> i32 {
    (v as u16) as i16 as i32
}

#[allow(clippy::declare_interior_mutable_const)]
const ATOMIC_ZERO: AtomicU32 = AtomicU32::new(0);
static EFFECT_SEEN: [AtomicU32; 13] = [ATOMIC_ZERO; 13];
static ACTIVE_MASK: AtomicU32 = AtomicU32::new(0);
static SENT_PKTS: AtomicU32 = AtomicU32::new(0);
static RX_TOTAL: AtomicU32 = AtomicU32::new(0);
static RX_ERRORS: AtomicU32 = AtomicU32::new(0);
static LAST_CONST_RAW: AtomicI32 = AtomicI32::new(0);
static TEST_PULSE_UNTIL_MS: AtomicU32 = AtomicU32::new(0);

fn note_effect_seen(t: u32) {
    let i = t as usize;
    if i < EFFECT_SEEN.len() && EFFECT_SEEN[i].fetch_add(1, Ordering::Relaxed) == 0 {
        add_log(format!("🎮 Game is using effect type: {}", etype_name(t)), COL_OK);
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Tuning {
    vib_gain: f32,
    detail: f32,
    texture: f32,
    cond_on: bool,
    cond_gain: f32,
    invert_motor: bool,
    rumble_layer: bool,
    send_ms: u32,
    min_force: f32,
    low_boost: f32,
    max_out: f32,
    soft_limit: bool,
    auto_gain: bool,
}

impl Tuning {
    // قيم متوازنة افتراضية تمنع الصدمات العنيفة والشد المستمر لـ 135 درجة
    const DEFAULT: Tuning = Tuning {
        vib_gain: 1.0,
        detail: 0.35,
        texture: 0.35,
        cond_on: true,
        cond_gain: 0.45,
        invert_motor: false,
        rumble_layer: false,
        send_ms: 8,
        min_force: 0.025,
        low_boost: 0.15,
        max_out: 0.78,
        soft_limit: true,
        auto_gain: false,
    };

    const RALLY: Tuning = Tuning {
        vib_gain: 1.45,
        detail: 0.65,
        texture: 0.70,
        cond_on: true,
        cond_gain: 0.55,
        invert_motor: false,
        rumble_layer: false,
        send_ms: 8,
        min_force: 0.035,
        low_boost: 0.28,
        max_out: 0.85,
        soft_limit: true,
        auto_gain: false,
    };

    const ARCADE: Tuning = Tuning {
        vib_gain: 1.35,
        detail: 0.50,
        texture: 0.55,
        cond_on: true,
        cond_gain: 0.40,
        invert_motor: false,
        rumble_layer: false,
        send_ms: 8,
        min_force: 0.04,
        low_boost: 0.32,
        max_out: 0.82,
        soft_limit: true,
        auto_gain: true,
    };

    const SIM: Tuning = Tuning {
        vib_gain: 0.90,
        detail: 0.25,
        texture: 0.25,
        cond_on: true,
        cond_gain: 0.55,
        invert_motor: false,
        rumble_layer: false,
        send_ms: 8,
        min_force: 0.02,
        low_boost: 0.10,
        max_out: 0.75,
        soft_limit: true,
        auto_gain: false,
    };

    fn apply_preset(&mut self, i: usize) {
        let keep_rumble = self.rumble_layer;
        let keep_send = self.send_ms;
        let keep_inv = self.invert_motor;
        *self = match i {
            1 => Tuning::RALLY,
            2 => Tuning::ARCADE,
            3 => Tuning::SIM,
            _ => Tuning::DEFAULT,
        };
        self.rumble_layer = keep_rumble;
        self.send_ms = keep_send;
        self.invert_motor = keep_inv;
    }

    fn save(&self) {
        let data = format!(
            "vib_gain={}\ndetail={}\ntexture={}\ncond_on={}\ncond_gain={}\ninvert_motor={}\nrumble_layer={}\nsend_ms={}\nmin_force={}\nlow_boost={}\nmax_out={}\nsoft_limit={}\nauto_gain={}\n",
            self.vib_gain,
            self.detail,
            self.texture,
            self.cond_on,
            self.cond_gain,
            self.invert_motor,
            self.rumble_layer,
            self.send_ms,
            self.min_force,
            self.low_boost,
            self.max_out,
            self.soft_limit,
            self.auto_gain,
        );
        let _ = fs::write(CONFIG_FILE, data);
    }

    fn load() -> Self {
        let Ok(content) = fs::read_to_string(CONFIG_FILE) else {
            return Self::DEFAULT;
        };
        let mut t = Self::DEFAULT;
        for line in content.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "vib_gain" => if let Ok(x) = v.trim().parse::<f32>() { t.vib_gain = x.clamp(0.0, 3.0); },
                "detail" => if let Ok(x) = v.trim().parse::<f32>() { t.detail = x.clamp(0.0, 2.0); },
                "texture" => if let Ok(x) = v.trim().parse::<f32>() { t.texture = x.clamp(0.0, 2.0); },
                "cond_on" => if let Ok(x) = v.trim().parse::<bool>() { t.cond_on = x; },
                "cond_gain" => if let Ok(x) = v.trim().parse::<f32>() { t.cond_gain = x.clamp(0.0, 1.5); },
                "invert_motor" => if let Ok(x) = v.trim().parse::<bool>() { t.invert_motor = x; },
                "rumble_layer" => if let Ok(x) = v.trim().parse::<bool>() { t.rumble_layer = x; },
                "send_ms" => if let Ok(x) = v.trim().parse::<u32>() { t.send_ms = x.clamp(4, 30); },
                "min_force" => if let Ok(x) = v.trim().parse::<f32>() { t.min_force = x.clamp(0.0, 0.15); },
                "low_boost" => if let Ok(x) = v.trim().parse::<f32>() { t.low_boost = x.clamp(0.0, 0.8); },
                "max_out" => if let Ok(x) = v.trim().parse::<f32>() { t.max_out = x.clamp(0.15, 1.0); },
                "soft_limit" => if let Ok(x) = v.trim().parse::<bool>() { t.soft_limit = x; },
                "auto_gain" => if let Ok(x) = v.trim().parse::<bool>() { t.auto_gain = x; },
                _ => {}
            }
        }
        t
    }
}

static TUNE: Mutex<Tuning> = Mutex::new(Tuning::DEFAULT);

#[derive(Clone, Copy)]
struct Effect {
    etype: u32,
    running: bool,
    started: Option<Instant>,
    duration_ms: u32,
    loops: u32,
    gain: f32,
    dir_factor: f32,
    magnitude: i32,
    prev_magnitude: i32,
    last_mag_instant: Option<Instant>,
    ramp_start: i32,
    ramp_end: i32,
    p_mag: u32,
    p_offset: i32,
    p_phase: u32,
    p_period: u32,
    has_env: bool,
    atk_level: u32,
    fade_level: u32,
    atk_time: u32,
    fade_time: u32,
    has_cond: bool,
    c_center: i32,
    c_pos_coeff: i32,
    c_neg_coeff: i32,
    c_pos_sat: u32,
    c_neg_sat: u32,
    c_dead: i32,
}

impl Effect {
    const EMPTY: Effect = Effect {
        etype: 0,
        running: false,
        started: None,
        duration_ms: 0,
        loops: 1,
        gain: 1.0,
        dir_factor: 1.0,
        magnitude: 0,
        prev_magnitude: 0,
        last_mag_instant: None,
        ramp_start: 0,
        ramp_end: 0,
        p_mag: 0,
        p_offset: 0,
        p_phase: 0,
        p_period: 100,
        has_env: false,
        atk_level: 0,
        fade_level: 0,
        atk_time: 0,
        fade_time: 0,
        has_cond: false,
        c_center: 0,
        c_pos_coeff: 0,
        c_neg_coeff: 0,
        c_pos_sat: 10000,
        c_neg_sat: 10000,
        c_dead: 0,
    };

    fn total_ms(&self) -> Option<f64> {
        if self.duration_ms == 0 || self.loops >= 255 {
            return None;
        }
        Some(self.duration_ms as f64 * self.loops.max(1) as f64)
    }
}

struct FfbState {
    effects: [Effect; MAX_EFFECTS],
    device_gain: u8,
    actuators_on: bool,
    paused: bool,
}

static FFB: Mutex<FfbState> = Mutex::new(FfbState {
    effects: [Effect::EMPTY; MAX_EFFECTS],
    device_gain: 255,
    actuators_on: true,
    paused: false,
});

fn clear_all_effects_and_stop(reason: &str) {
    let mut s = FFB.lock().unwrap();
    for e in s.effects.iter_mut() {
        e.running = false;
        e.magnitude = 0;
        e.prev_magnitude = 0;
    }
    ACTIVE_MASK.store(0, Ordering::Relaxed);
    add_log(format!("🛑 {}", reason), COL_WARN);
}

static FFB_DEBUG: AtomicBool = AtomicBool::new(false);
static SEEN_EFOP: AtomicBool = AtomicBool::new(false);
static DBG_LAST: Mutex<[Option<Instant>; 12]> = Mutex::new([None; 12]);

fn rate_ok(slot: usize, min_ms: u64) -> bool {
    let mut g = DBG_LAST.lock().unwrap();
    let ok = g[slot].map_or(true, |t| t.elapsed() >= Duration::from_millis(min_ms));
    if ok {
        g[slot] = Some(Instant::now());
    }
    ok
}

fn dbg_allowed(slot: usize, min_ms: u64) -> bool {
    FFB_DEBUG.load(Ordering::Relaxed) && rate_ok(slot, min_ms)
}

fn rx_error(what: &str, detail: String) {
    let n = RX_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || rate_ok(11, 3000) {
        add_log(format!("⚠ FFB {} packet problem: {} (total {})", what, detail, n), COL_WARN);
    }
}

fn legacy_autostart(e: &mut Effect) {
    if !SEEN_EFOP.load(Ordering::Relaxed) && !e.running {
        e.running = true;
        e.started = Some(Instant::now());
    }
}

// ============================================================================
//  MIXER & SHAPING
// ============================================================================
#[derive(Clone, Copy, Default)]
struct FfbMix {
    di: f32,
    phys: f32,
    vib: f32,
    active: bool,
}

#[derive(Clone, Copy)]
struct RenderParams {
    pos: f32,
    vel: f32,
    acc: f32,
    vib_gain: f32,
    detail: f32,
    texture: f32,
    cond_on: bool,
    cond_gain: f32,
}

fn wave(t: u32, p: f32) -> f32 {
    match t {
        ET_SQR => if p < 0.5 { 1.0 } else { -1.0 },
        ET_SINE => (p * 2.0 * PI).sin(),
        ET_TRNGL => {
            if p < 0.25 {
                4.0 * p
            } else if p < 0.75 {
                2.0 - 4.0 * p
            } else {
                4.0 * p - 4.0
            }
        }
        ET_STUP => 2.0 * p - 1.0,
        ET_STDN => 1.0 - 2.0 * p,
        _ => 0.0,
    }
}

fn envelope_amp(e: &Effect, amp: f32, t_ms: f64, total_ms: Option<f64>) -> f32 {
    if !e.has_env {
        return amp;
    }
    let mut a = amp;
    if e.atk_time > 0 && t_ms < e.atk_time as f64 {
        let atk = (e.atk_level as f32 / 10000.0).clamp(0.0, 1.5);
        let k = (t_ms / e.atk_time as f64) as f32;
        a = atk + (amp - atk) * k;
    }
    if let Some(total) = total_ms {
        if e.fade_time > 0 {
            let fade_start = total - e.fade_time as f64;
            if t_ms > fade_start {
                let fade = (e.fade_level as f32 / 10000.0).clamp(0.0, 1.5);
                let k = (((t_ms - fade_start) / e.fade_time as f64) as f32).clamp(0.0, 1.0);
                a += (fade - a) * k;
            }
        }
    }
    a.max(0.0)
}

fn sat_of(s: u32) -> f32 {
    if s == 0 {
        1.0
    } else {
        (s as f32 / 10000.0).clamp(0.0, 1.0)
    }
}

fn cond_force(t: u32, e: &Effect, pos: f32, vel: f32, acc: f32) -> f32 {
    match t {
        ET_SPRNG => {
            let cp = (s16(e.c_center) as f32 / 10000.0).clamp(-1.0, 1.0);
            let db = (s16(e.c_dead).abs() as f32 / 10000.0).clamp(0.0, 0.5);
            let x = pos - cp;
            let coeff = if x >= 0.0 {
                (s16(e.c_pos_coeff) as f32 / 10000.0).clamp(-1.5, 1.5)
            } else {
                (s16(e.c_neg_coeff) as f32 / 10000.0).clamp(-1.5, 1.5)
            };
            if x > db {
                let s = sat_of(e.c_pos_sat);
                (-coeff * (x - db)).clamp(-s, s)
            } else if x < -db {
                let s = sat_of(e.c_neg_sat);
                (-coeff * (x + db)).clamp(-s, s)
            } else {
                0.0
            }
        }
        ET_DMPR | ET_FRCTN => {
            let full = if t == ET_DMPR { DAMPER_FULL_VEL } else { FRICTION_FULL_VEL };
            let v = (vel / full).clamp(-1.0, 1.0);
            let (c, s) = if v >= 0.0 {
                ((s16(e.c_pos_coeff) as f32 / 10000.0).clamp(-1.5, 1.5), sat_of(e.c_pos_sat))
            } else {
                ((s16(e.c_neg_coeff) as f32 / 10000.0).clamp(-1.5, 1.5), sat_of(e.c_neg_sat))
            };
            (-c * v).clamp(-s, s)
        }
        ET_INRT => {
            let a = (acc / INERTIA_FULL_ACC).clamp(-1.0, 1.0);
            let (c, s) = if a >= 0.0 {
                ((s16(e.c_pos_coeff) as f32 / 10000.0).clamp(-1.5, 1.5), sat_of(e.c_pos_sat))
            } else {
                ((s16(e.c_neg_coeff) as f32 / 10000.0).clamp(-1.5, 1.5), sat_of(e.c_neg_sat))
            };
            (-c * a).clamp(-s, s)
        }
        _ => 0.0,
    }
}

fn ffb_render(rp: &RenderParams) -> Option<FfbMix> {
    let mut s = FFB.lock().unwrap();
    if !s.actuators_on || s.paused {
        ACTIVE_MASK.store(0, Ordering::Relaxed);
        return None;
    }
    let dev = s.device_gain as f32 / 255.0;
    let now = Instant::now();
    let mut mix = FfbMix::default();
    let mut mask: u32 = 0;

    for e in s.effects.iter_mut() {
        if !e.running || e.etype == 0 {
            continue;
        }
        let t0 = *e.started.get_or_insert(now);
        let t_ms = now.duration_since(t0).as_secs_f64() * 1000.0;
        let total = e.total_ms();

        if let Some(tot) = total {
            if t_ms >= tot {
                e.running = false;
                continue;
            }
        }
        mix.active = true;
        mask |= 1u32 << (e.etype & 31);

        match e.etype {
            ET_CONST => {
                let m = (e.magnitude as f32 / 10000.0).clamp(-1.0, 1.0);
                let prev_m = (e.prev_magnitude as f32 / 10000.0).clamp(-1.0, 1.0);
                let delta = (m - prev_m).abs();

                // لو التغير سريع ومتذبذب (مطبات/حشيش داخل Constant) نضيف لمسة texture ناعمة
                let mut road_buzz = 0.0f32;
                if let Some(last_t) = e.last_mag_instant {
                    let dt = now.duration_since(last_t).as_secs_f32().max(0.001);
                    if dt < 0.08 && delta > 0.03 {
                        road_buzz = (delta * rp.texture * 0.45).clamp(0.0, 0.25)
                            * (m - prev_m).signum();
                        mix.vib += (delta * rp.texture).clamp(0.0, 0.6);
                    }
                }

                // Impact detail خفيف في بداية التغير الحاد
                let impact = if delta > 0.25 {
                    ((delta - 0.25) * rp.detail * 0.25).clamp(0.0, 0.20) * (m - prev_m).signum()
                } else {
                    0.0
                };

                let a = envelope_amp(e, m.abs(), t_ms, total);
                let base = a * m.signum();
                mix.di += (base + road_buzz + impact).clamp(-1.0, 1.0) * e.gain * e.dir_factor;
            }
            ET_RAMP => {
                let k = if e.duration_ms > 0 {
                    let d = e.duration_ms as f64;
                    ((t_ms % d) / d) as f32
                } else {
                    0.0
                };
                let v = e.ramp_start as f32 + (e.ramp_end as f32 - e.ramp_start as f32) * k;
                mix.di += (v / 10000.0).clamp(-1.0, 1.0) * e.gain * e.dir_factor;
            }
            t if is_periodic(t) => {
                let period = e.p_period.max(1) as f64;
                let ph_deg = if e.p_phase > 360 {
                    e.p_phase as f64 / 100.0
                } else {
                    e.p_phase as f64
                };
                let ph = ((t_ms / period) + ph_deg / 360.0).fract() as f32;
                let freq = 1000.0 / period as f32;
                let att = if freq > 45.0 { (45.0 / freq).max(0.35) } else { 1.0 };
                let amp = envelope_amp(e, (e.p_mag as f32 / 10000.0).clamp(0.0, 1.0), t_ms, total);
                let osc = amp * wave(t, ph) * att;
                let off = (e.p_offset as f32 / 10000.0).clamp(-1.0, 1.0);
                let k = e.gain * e.dir_factor;
                mix.di += (off + osc * rp.vib_gain).clamp(-1.2, 1.2) * k;
                mix.vib += (osc * e.gain).abs();
            }
            t if is_condition(t) => {
                if rp.cond_on && e.has_cond {
                    mix.phys += cond_force(t, e, rp.pos, rp.vel, rp.acc) * e.gain * rp.cond_gain;
                }
            }
            _ => {}
        }
    }

    ACTIVE_MASK.store(mask, Ordering::Relaxed);

    if !mix.active {
        return None;
    }
    mix.di *= dev;
    mix.phys *= dev;
    mix.vib *= dev;
    Some(mix)
}

fn limit(a: f32, ceil: f32, soft: bool) -> f32 {
    if ceil <= 0.0 {
        return 0.0;
    }
    if !soft {
        return a.min(ceil);
    }
    let knee = ceil * 0.72;
    if a <= knee {
        a
    } else {
        knee + (ceil - knee) * ((a - knee) / (ceil - knee)).tanh()
    }
}

fn shape_force(x: f32, t: &Tuning) -> f32 {
    let mut a = x.abs();
    if a < 0.002 {
        return 0.0;
    }
    if a <= 1.0 {
        let exp = (1.0 - 0.35 * t.low_boost).clamp(0.55, 1.0);
        a = a.powf(exp);
    }
    let m = t.min_force;
    let ramp = (a / 0.06).min(1.0);
    a = a * (1.0 - m) + m * ramp;
    x.signum() * limit(a, t.max_out, t.soft_limit)
}

// ============================================================================
//  CONTROLLER BUTTONS & VJOY BRIDGE
// ============================================================================
mod btn {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LS: u16 = 0x0040;
    pub const RS: u16 = 0x0080;
    pub const LB: u16 = 0x0100;
    pub const RB: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

const BUTTON_NAMES: &[(u16, &str)] = &[
    (btn::A, "A"),
    (btn::B, "B"),
    (btn::X, "X"),
    (btn::Y, "Y"),
    (btn::LB, "Paddle Left (LB)"),
    (btn::RB, "Paddle Right (RB)"),
    (btn::DPAD_UP, "D-Pad Up"),
    (btn::DPAD_DOWN, "D-Pad Down"),
    (btn::DPAD_LEFT, "D-Pad Left"),
    (btn::DPAD_RIGHT, "D-Pad Right"),
    (btn::START, "Start"),
    (btn::BACK, "Back"),
    (btn::GUIDE, "Guide"),
];

mod vjoy {
    use super::*;

    type BOOL = i32;
    type UINT = u32;
    type LONG = i32;
    type BYTE = u8;

    pub const HID_USAGE_X: UINT = 0x30;
    pub const HID_USAGE_Y: UINT = 0x31;
    pub const HID_USAGE_Z: UINT = 0x32;
    pub const HID_USAGE_RZ: UINT = 0x35;

    const PT_EFFREP: u32 = 0x01;
    const PT_ENVREP: u32 = 0x02;
    const PT_CONDREP: u32 = 0x03;
    const PT_PRIDREP: u32 = 0x04;
    const PT_CONSTREP: u32 = 0x05;
    const PT_RAMPREP: u32 = 0x06;
    const PT_EFOPREP: u32 = 0x0A;
    const PT_BLKFRREP: u32 = 0x0B;
    const PT_CTRLREP: u32 = 0x0C;
    const PT_GAINREP: u32 = 0x0D;
    const PT_IGNORED: [u32; 6] = [0x07, 0x08, 0x0E, 0x11, 0x12, 0x13];

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffConstant {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub magnitude: i32,
        pub _tail: [u8; 8],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffRamp {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub start: i32,
        pub end: i32,
        pub _tail: [u8; 8],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffPeriod {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub magnitude: u32,
        pub offset: i32,
        pub phase: u32,
        pub period: u32,
        pub _tail: [u8; 8],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffCond {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub is_y: i32,
        pub center_offset: i32,
        pub pos_coeff: i32,
        pub neg_coeff: i32,
        pub pos_satur: u32,
        pub neg_satur: u32,
        pub dead_band: i32,
        pub _tail: [u8; 8],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffEnvlp {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub attack_level: u32,
        pub fade_level: u32,
        pub attack_time: u32,
        pub fade_time: u32,
        pub _tail: [u8; 8],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffOp {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub effect_op: u32,
        pub loop_count: u8,
        pub _pad2: [u8; 3],
        pub _tail: [u8; 8],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    #[allow(dead_code)]
    pub struct FfbEffReport {
        pub effect_block_index: u8,
        pub _pad0: [u8; 3],
        pub effect_type: u32,
        pub duration: u16,
        pub trigger_rpt: u16,
        pub sample_prd: u16,
        pub gain: u8,
        pub trigger_btn: u8,
        pub polar: i32,
        pub direction: u8,
        pub dir_y: u8,
        pub _tail: [u8; 10],
    }

    type FnVJoyEnabled = unsafe extern "system" fn() -> BOOL;
    type FnGetVJDStatus = unsafe extern "system" fn(UINT) -> i32;
    type FnAcquireVJD = unsafe extern "system" fn(UINT) -> BOOL;
    type FnRelinquishVJD = unsafe extern "system" fn(UINT);
    type FnSetAxis = unsafe extern "system" fn(LONG, UINT, UINT) -> BOOL;
    type FnSetBtn = unsafe extern "system" fn(BOOL, UINT, BYTE) -> BOOL;
    type FnSetDiscPov = unsafe extern "system" fn(i32, UINT, BYTE) -> BOOL;

    type FfbGenCB = unsafe extern "system" fn(*const c_void, *mut c_void);
    type FnFfbStart = unsafe extern "system" fn(UINT) -> BOOL;
    type FnFfbRegisterGenCB = unsafe extern "system" fn(FfbGenCB, *mut c_void);

    pub type FnFfbU32 = unsafe extern "system" fn(*const c_void, *mut u32) -> u32;
    pub type FnFfbEffReport = unsafe extern "system" fn(*const c_void, *mut FfbEffReport) -> u32;
    pub type FnFfbEffConstant = unsafe extern "system" fn(*const c_void, *mut FfbEffConstant) -> u32;
    pub type FnFfbEffRamp = unsafe extern "system" fn(*const c_void, *mut FfbEffRamp) -> u32;
    pub type FnFfbEffPeriod = unsafe extern "system" fn(*const c_void, *mut FfbEffPeriod) -> u32;
    pub type FnFfbEffCond = unsafe extern "system" fn(*const c_void, *mut FfbEffCond) -> u32;
    pub type FnFfbEffEnvlp = unsafe extern "system" fn(*const c_void, *mut FfbEffEnvlp) -> u32;
    pub type FnFfbEffOp = unsafe extern "system" fn(*const c_void, *mut FfbEffOp) -> u32;

    unsafe extern "system" {
        fn LoadLibraryA(lpLibFileName: *const u8) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    }

    macro_rules! cs {
        ($s:expr) => {
            concat!($s, "\0").as_bytes()
        };
    }

    #[derive(Clone, Copy, Default)]
    pub struct FfbFns {
        pub packet_type: Option<FnFfbU32>,
        pub ebi: Option<FnFfbU32>,
        pub eff_report: Option<FnFfbEffReport>,
        pub eff_const: Option<FnFfbEffConstant>,
        pub eff_ramp: Option<FnFfbEffRamp>,
        pub eff_period: Option<FnFfbEffPeriod>,
        pub eff_cond: Option<FnFfbEffCond>,
        pub eff_envlp: Option<FnFfbEffEnvlp>,
        pub eff_op: Option<FnFfbEffOp>,
        pub dev_ctrl: Option<FnFfbU32>,
        pub dev_gain: Option<FnFfbU32>,
    }

    pub struct FfbContext {
        pub fns: FfbFns,
    }

    fn slot_index(ebi: u32) -> usize {
        ((ebi & 0xFF) as usize).min(MAX_EFFECTS - 1)
    }

    fn dir_factor(polar: bool, dir: u8, dir_y: u8) -> f32 {
        let f = if polar {
            (dir as f32 * 2.0 * PI / 256.0).sin()
        } else {
            let x = dir as i8 as f32;
            let y = dir_y as i8 as f32;
            let n = (x * x + y * y).sqrt();
            if n < 1.0 { 1.0 } else { x / n }
        };
        if f.abs() < 0.1 { 1.0 } else { f }
    }

    unsafe fn call_u32(f: Option<FnFfbU32>, p: *const c_void) -> Option<u32> {
        let f = f?;
        let mut v: u32 = 0;
        if unsafe { f(p, &mut v) } == 0 { Some(v) } else { None }
    }

    pub unsafe extern "system" fn ffb_callback(p_packet: *const c_void, p_ctx: *mut c_void) {
        if p_packet.is_null() || p_ctx.is_null() {
            return;
        }
        RX_TOTAL.fetch_add(1, Ordering::Relaxed);
        let ctx = unsafe { &*(p_ctx as *const FfbContext) };
        let f = ctx.fns;

        let ptype = unsafe { call_u32(f.packet_type, p_packet) }.unwrap_or(0);
        let ebi: Option<usize> = unsafe { call_u32(f.ebi, p_packet) }.map(slot_index);

        match ptype {
            PT_EFFREP => {
                let Some(fr) = f.eff_report else { return; };
                let mut rep = FfbEffReport::default();
                if unsafe { fr(p_packet, &mut rep) } != 0 {
                    rx_error("EffReport", "helper returned an error".to_string());
                    return;
                }
                let t = rep.effect_type;
                if t == 0 || t > ET_CSTM {
                    rx_error("EffReport", format!("bad effect type {} (block #{})", t, rep.effect_block_index));
                    return;
                }
                note_effect_seen(t);
                let idx = slot_index(rep.effect_block_index as u32);
                let polar = rep.polar != 0;
                let factor = dir_factor(polar, rep.direction, rep.dir_y);
                let dur = if rep.duration == 0 || rep.duration >= 32767 { 0 } else { rep.duration as u32 };
                let gain = if rep.gain == 0 { 1.0 } else { rep.gain as f32 / 255.0 };
                let changed = {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    let c = e.etype != t
                        || e.duration_ms != dur
                        || (e.gain - gain).abs() > 0.004
                        || (e.dir_factor - factor).abs() > 0.01;
                    e.etype = t;
                    e.duration_ms = dur;
                    e.dir_factor = factor;
                    e.gain = gain;
                    c
                };
                let show = if FFB_DEBUG.load(Ordering::Relaxed) {
                    rate_ok(0, 120)
                } else {
                    changed && rate_ok(0, 300)
                };
                if show {
                    add_log(
                        format!(
                            "FFB Effect #{}: {} dur={}ms gain={} -> X {:+.2}",
                            idx, etype_name(t), dur, rep.gain, factor
                        ),
                        COL_DBG,
                    );
                }
            }

            PT_ENVREP => {
                let Some(fe) = f.eff_envlp else { return; };
                let mut en = FfbEffEnvlp::default();
                if unsafe { fe(p_packet, &mut en) } != 0 {
                    rx_error("Envelope", "helper returned an error".to_string());
                    return;
                }
                let idx = slot_index(en.effect_block_index as u32);
                let mut s = FFB.lock().unwrap();
                let e = &mut s.effects[idx];
                e.has_env = true;
                e.atk_level = en.attack_level;
                e.fade_level = en.fade_level;
                e.atk_time = en.attack_time;
                e.fade_time = en.fade_time;
            }

            PT_CONSTREP => {
                let Some(fc) = f.eff_const else { return; };
                let mut c = FfbEffConstant::default();
                if unsafe { fc(p_packet, &mut c) } != 0 {
                    rx_error("Constant", "helper returned an error".to_string());
                    return;
                }
                let mag = s16(c.magnitude).clamp(-10000, 10000);
                LAST_CONST_RAW.store(mag, Ordering::Relaxed);
                let idx = slot_index(c.effect_block_index as u32);
                let now = Instant::now();
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    if e.etype == 0 {
                        e.etype = ET_CONST;
                    }
                    e.prev_magnitude = e.magnitude;
                    e.magnitude = mag;
                    e.last_mag_instant = Some(now);
                    legacy_autostart(e);
                }
                if dbg_allowed(1, 200) {
                    add_log(format!("FFB Const #{}: mag={:+}", idx, mag), COL_DBG);
                }
            }

            PT_RAMPREP => {
                let Some(fr) = f.eff_ramp else { return; };
                let mut r = FfbEffRamp::default();
                if unsafe { fr(p_packet, &mut r) } != 0 {
                    rx_error("Ramp", "helper returned an error".to_string());
                    return;
                }
                let idx = slot_index(r.effect_block_index as u32);
                let mut s = FFB.lock().unwrap();
                let e = &mut s.effects[idx];
                if e.etype == 0 {
                    e.etype = ET_RAMP;
                }
                e.ramp_start = s16(r.start).clamp(-10000, 10000);
                e.ramp_end = s16(r.end).clamp(-10000, 10000);
                legacy_autostart(e);
            }

            PT_PRIDREP => {
                let Some(fp) = f.eff_period else { return; };
                let mut p = FfbEffPeriod::default();
                if unsafe { fp(p_packet, &mut p) } != 0 {
                    rx_error("Periodic", "helper returned an error".to_string());
                    return;
                }
                let idx = slot_index(p.effect_block_index as u32);
                let mut s = FFB.lock().unwrap();
                let e = &mut s.effects[idx];
                if is_periodic(e.etype) == false {
                    e.etype = ET_SINE;
                }
                e.p_mag = (p.magnitude as u16).min(10000) as u32;
                e.p_offset = s16(p.offset).clamp(-10000, 10000);
                e.p_phase = p.phase;
                e.p_period = p.period.max(1);
                legacy_autostart(e);
            }

            PT_CONDREP => {
                let Some(fc) = f.eff_cond else { return; };
                let mut cd = FfbEffCond::default();
                if unsafe { fc(p_packet, &mut cd) } != 0 {
                    rx_error("Condition", "helper returned an error".to_string());
                    return;
                }
                if cd.is_y != 0 {
                    return;
                }
                let idx = slot_index(cd.effect_block_index as u32);
                let mut s = FFB.lock().unwrap();
                let e = &mut s.effects[idx];
                if is_condition(e.etype) == false {
                    e.etype = ET_SPRNG;
                }
                e.has_cond = true;
                e.c_center = s16(cd.center_offset);
                e.c_pos_coeff = s16(cd.pos_coeff);
                e.c_neg_coeff = s16(cd.neg_coeff);
                e.c_pos_sat = (cd.pos_satur as u16).min(10000) as u32;
                e.c_neg_sat = (cd.neg_satur as u16).min(10000) as u32;
                e.c_dead = s16(cd.dead_band);
                legacy_autostart(e);
            }

            PT_EFOPREP => {
                let Some(fo) = f.eff_op else { return; };
                let mut op = FfbEffOp::default();
                if unsafe { fo(p_packet, &mut op) } != 0 {
                    rx_error("EffectOp", "helper returned an error".to_string());
                    return;
                }
                SEEN_EFOP.store(true, Ordering::Relaxed);
                let idx = slot_index(op.effect_block_index as u32);
                let mut s = FFB.lock().unwrap();
                let e = &mut s.effects[idx];
                if op.loop_count > 0 {
                    e.loops = op.loop_count as u32;
                }
                match op.effect_op {
                    1 | 2 => {
                        // لو كان شغال بالفعل من قريب (<250ms) لا نصفر started عشان الموجة متتقطعش
                        let already_fresh = e.running
                            && e.started
                                .map_or(false, |t| t.elapsed() < Duration::from_millis(250));
                        e.running = true;
                        if !already_fresh {
                            e.started = Some(Instant::now());
                        }
                    }
                    3 => {
                        e.running = false;
                    }
                    _ => {}
                }
            }

            PT_BLKFRREP => {
                if let Some(idx) = ebi {
                    let mut s = FFB.lock().unwrap();
                    s.effects[idx] = Effect::EMPTY;
                }
            }

            PT_CTRLREP => {
                if let Some(v) = unsafe { call_u32(f.dev_ctrl, p_packet) } {
                    let mut s = FFB.lock().unwrap();
                    match v {
                        1 => s.actuators_on = true,
                        2 => s.actuators_on = false,
                        3 => {
                            for e in s.effects.iter_mut() {
                                e.running = false;
                            }
                        }
                        4 => s.paused = false,
                        5 => s.paused = true,
                        _ => {}
                    }
                }
            }

            PT_GAINREP => {
                if let Some(v) = unsafe { call_u32(f.dev_gain, p_packet) } {
                    let g = (v & 0xFF) as u8;
                    FFB.lock().unwrap().device_gain = g;
                }
            }

            _ => {
                if !PT_IGNORED.contains(&ptype) && dbg_allowed(5, 1000) {
                    add_log(format!("FFB unknown packet type 0x{:02X}", ptype), COL_DBG);
                }
            }
        }
    }

    pub struct VJoyBridge {
        _dll: *mut c_void,
        set_axis: FnSetAxis,
        set_btn: FnSetBtn,
        set_pov: FnSetDiscPov,
        relinquish: FnRelinquishVJD,
        vjd_id: UINT,
        _ctx: Box<FfbContext>,
    }

    unsafe impl Send for VJoyBridge {}

    impl VJoyBridge {
        pub fn connect(vjd_id: u32) -> Result<Self, String> {
            unsafe {
                let dll = LoadLibraryA(cs!("vJoyInterface.dll"));
                if dll.is_null() {
                    return Err("vJoyInterface.dll not found".into());
                }
                macro_rules! sym {
                    ($name:expr, $ty:ty) => {{
                        let p = GetProcAddress(dll, cs!($name));
                        if p.is_null() {
                            return Err(format!("Missing symbol {}", $name));
                        }
                        std::mem::transmute::<*mut c_void, $ty>(p)
                    }};
                }
                macro_rules! opt_sym {
                    ($name:expr, $ty:ty) => {{
                        let p = GetProcAddress(dll, cs!($name));
                        if p.is_null() { None } else { Some(std::mem::transmute::<*mut c_void, $ty>(p)) }
                    }};
                }

                let enabled = sym!("vJoyEnabled", FnVJoyEnabled);
                if enabled() == 0 {
                    return Err("vJoy driver is not enabled".into());
                }
                let status = sym!("GetVJDStatus", FnGetVJDStatus);
                let st = status(vjd_id);
                if st != 1 {
                    return Err(format!("vJoy device {} status = {}", vjd_id, st));
                }
                let acquire = sym!("AcquireVJD", FnAcquireVJD);
                if acquire(vjd_id) == 0 {
                    return Err(format!("Failed to acquire vJoy device {}", vjd_id));
                }

                let set_axis = sym!("SetAxis", FnSetAxis);
                let set_btn = sym!("SetBtn", FnSetBtn);
                let set_pov = sym!("SetDiscPov", FnSetDiscPov);
                let relinquish = sym!("RelinquishVJD", FnRelinquishVJD);

                let fns = FfbFns {
                    packet_type: opt_sym!("Ffb_h_Type", FnFfbU32),
                    ebi: opt_sym!("Ffb_h_EBI", FnFfbU32),
                    eff_report: opt_sym!("Ffb_h_Eff_Report", FnFfbEffReport),
                    eff_const: opt_sym!("Ffb_h_Eff_Constant", FnFfbEffConstant),
                    eff_ramp: opt_sym!("Ffb_h_Eff_Ramp", FnFfbEffRamp),
                    eff_period: opt_sym!("Ffb_h_Eff_Period", FnFfbEffPeriod),
                    eff_cond: opt_sym!("Ffb_h_Eff_Cond", FnFfbEffCond),
                    eff_envlp: opt_sym!("Ffb_h_Eff_Envlp", FnFfbEffEnvlp),
                    eff_op: opt_sym!("Ffb_h_Eff_Op", FnFfbEffOp),
                    dev_ctrl: opt_sym!("Ffb_h_DevCtrl", FnFfbU32),
                    dev_gain: opt_sym!("Ffb_h_DevGain", FnFfbU32),
                };

                let mut ctx = Box::new(FfbContext { fns });
                let ctx_ptr = (&mut *ctx) as *mut FfbContext as *mut c_void;

                if let Some(reg_cb) = opt_sym!("FfbRegisterGenCB", FnFfbRegisterGenCB) {
                    reg_cb(ffb_callback, ctx_ptr);
                }
                if let Some(ffb_start) = opt_sym!("FfbStart", FnFfbStart) {
                    let _ = ffb_start(vjd_id);
                }

                Ok(Self {
                    _dll: dll,
                    set_axis,
                    set_btn,
                    set_pov,
                    relinquish,
                    vjd_id,
                    _ctx: ctx,
                })
            }
        }

        pub fn send_axes(&self, steer_norm: f32, throttle: f32, brake: f32) {
            let s_val = (((steer_norm.clamp(-1.0, 1.0) + 1.0) * 0.5) * 32767.0) as LONG;
            let t_val = (throttle.clamp(0.0, 1.0) * 32767.0) as LONG;
            let b_val = (brake.clamp(0.0, 1.0) * 32767.0) as LONG;
            unsafe {
                let _ = (self.set_axis)(s_val, self.vjd_id, HID_USAGE_X);
                let _ = (self.set_axis)(t_val, self.vjd_id, HID_USAGE_Y);
                let _ = (self.set_axis)(b_val, self.vjd_id, HID_USAGE_Z);
                let _ = (self.set_axis)(16384, self.vjd_id, HID_USAGE_RZ);
            }
        }

        pub fn send_buttons_and_pov(&self, buttons: u16, pov: u8) {
            for (i, (mask, _)) in BUTTON_NAMES.iter().enumerate() {
                let pressed = if (buttons & *mask) != 0 { 1 } else { 0 };
                unsafe {
                    let _ = (self.set_btn)(pressed, self.vjd_id, (i + 1) as BYTE);
                }
            }
            let pov_v: i32 = match pov {
                1 => 0,
                2 => 9000,
                4 => 18000,
                8 => 27000,
                _ => -1,
            };
            unsafe {
                let _ = (self.set_pov)(pov_v, self.vjd_id, 1);
            }
        }
    }

    impl Drop for VJoyBridge {
        fn drop(&mut self) {
            unsafe {
                (self.relinquish)(self.vjd_id);
            }
        }
    }
}

// ============================================================================
//  TELEMETRY & BACKGROUND WORKER
// ============================================================================
#[derive(Clone, Default)]
struct Telemetry {
    receiver_connected: bool,
    receiver_desc: String,
    vjoy_connected: bool,
    vjoy_status: String,
    steer_norm: f32,
    throttle: f32,
    brake: f32,
    buttons: u16,
    pov: u8,
    ffb_byte: i8,
    di_force: f32,
    phys_force: f32,
    vib_level: f32,
    final_force: f32,
    auto_gain_factor: f32,
    pkt_per_sec: u32,
    rx_per_sec: u32,
    rx_errors: u32,
    last_const_raw: i32,
    active_mask: u32,
    seen_counts: [u32; 13],
}

enum UiCmd {
    SetVjoyId(u32),
    ReconnectAll,
    PanicStop,
    TriggerTestPulse,
}

fn decode_xbox_packet(data: &[u8]) -> Option<(f32, f32, f32, u16, u8)> {
    if data.len() < 14 {
        return None;
    }
    if data[0] != 0x00 || data[1] != 0x01 {
        return None;
    }
    let buttons = u16::from_le_bytes([data[2], data[3]]);
    let lt = data[4] as f32 / 255.0;
    let rt = data[5] as f32 / 255.0;
    let lx = i16::from_le_bytes([data[6], data[7]]) as f32 / 32767.0;
    let steer = lx.clamp(-1.0, 1.0);
    let pov = (buttons & 0x0F) as u8;
    Some((steer, rt, lt, buttons, pov))
}

fn spawn_worker(tel_tx: Sender<Telemetry>, cmd_rx: Receiver<UiCmd>) {
    thread::spawn(move || {
        let mut vjd_id: u32 = 1;
        let mut vjoy_bridge: Option<vjoy::VJoyBridge> = None;
        let mut vjoy_status = String::from("Connecting...");

        let mut in_ep: Option<nusb::Endpoint<In, Interrupt>> = None;
        let mut out_ep: Option<nusb::Endpoint<Out, Interrupt>> = None;
        let mut receiver_connected = false;
        let mut receiver_desc = String::from("Searching...");

        let try_connect_vjoy = |id: u32, bridge: &mut Option<vjoy::VJoyBridge>, status: &mut String| {
            *bridge = None;
            match vjoy::VJoyBridge::connect(id) {
                Ok(b) => {
                    *status = format!("vJoy #{} Ready", id);
                    add_log(format!("✅ Connected to vJoy device #{}", id), COL_OK);
                    *bridge = Some(b);
                }
                Err(e) => {
                    *status = format!("vJoy #{}: {}", id, e);
                    add_log(format!("⚠ vJoy #{}: {}", id, e), COL_WARN);
                }
            }
        };

        let try_connect_receiver = |
            in_ep: &mut Option<nusb::Endpoint<In, Interrupt>>,
            out_ep: &mut Option<nusb::Endpoint<Out, Interrupt>>,
            conn: &mut bool,
            desc: &mut String,
        | {
            *in_ep = None;
            *out_ep = None;
            *conn = false;

            let Ok(devs) = nusb::list_devices().wait() else {
                *desc = "USB scan failed".into();
                return;
            };

            for d in devs {
                let matched = RECEIVER_IDS
                    .iter()
                    .any(|(vid, pid)| d.vendor_id() == *vid && d.product_id() == *pid);
                if !matched {
                    continue;
                }
                if let Ok(dev) = d.open().wait() {
                    if let Ok(iface) = dev.claim_interface(0).wait() {
                        let mut ep_in = None;
                        let mut ep_out = None;
                        for ep in iface.endpoints() {
                            match (ep.direction(), ep.transfer_type()) {
                                (nusb::transfer::Direction::In, nusb::transfer::TransferType::Interrupt) => {
                                    if ep_in.is_none() {
                                        ep_in = iface.endpoint::<In, Interrupt>(ep.address()).ok();
                                    }
                                }
                                (nusb::transfer::Direction::Out, nusb::transfer::TransferType::Interrupt) => {
                                    if ep_out.is_none() {
                                        ep_out = iface.endpoint::<Out, Interrupt>(ep.address()).ok();
                                    }
                                }
                                _ => {}
                            }
                        }
                        if ep_in.is_some() {
                            *in_ep = ep_in;
                            *out_ep = ep_out;
                            *conn = true;
                            *desc = format!("{:04X}:{:04X}", d.vendor_id(), d.product_id());
                            add_log(format!("✅ Xbox Receiver connected ({})", *desc), COL_OK);
                            return;
                        }
                    }
                }
            }
            *desc = "Not found".into();
        };

        try_connect_vjoy(vjd_id, &mut vjoy_bridge, &mut vjoy_status);
        try_connect_receiver(&mut in_ep, &mut out_ep, &mut receiver_connected, &mut receiver_desc);

        let mut steer = 0.0f32;
        let mut throttle = 0.0f32;
        let mut brake = 0.0f32;
        let mut buttons = 0u16;
        let mut pov = 0u8;
        let mut prev_buttons = 0u16;

        let mut prev_pos = 0.0f32;
        let mut prev_vel = 0.0f32;
        let mut last_kin_t = Instant::now();
        let mut last_send_t = Instant::now();
        let mut last_ui_t = Instant::now();
        let mut last_rate_t = Instant::now();
        let mut last_sent_cnt = 0u32;
        let mut last_rx_cnt = 0u32;
        let mut pkt_per_sec = 0u32;
        let mut rx_per_sec = 0u32;
        let mut peak_seen = 0.25f32;
        let mut auto_gain = 1.0f32;

        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    UiCmd::SetVjoyId(id) => {
                        vjd_id = id;
                        try_connect_vjoy(vjd_id, &mut vjoy_bridge, &mut vjoy_status);
                    }
                    UiCmd::ReconnectAll => {
                        try_connect_vjoy(vjd_id, &mut vjoy_bridge, &mut vjoy_status);
                        try_connect_receiver(&mut in_ep, &mut out_ep, &mut receiver_connected, &mut receiver_desc);
                    }
                    UiCmd::PanicStop => {
                        clear_all_effects_and_stop("PANIC STOP: All FFB forces zeroed");
                        if let Some(ref b) = vjoy_bridge {
                            b.send_axes(steer, throttle, brake);
                        }
                        if let Some(ref mut oep) = out_ep {
                            let buf = Buffer::from(vec![0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
                            let _ = oep.submit(buf);
                        }
                    }
                    UiCmd::TriggerTestPulse => {
                        let start = START_TIME.get_or_init(Instant::now);
                        let now_ms = start.elapsed().as_millis() as u32;
                        TEST_PULSE_UNTIL_MS.store(now_ms.saturating_add(450), Ordering::Relaxed);
                        add_log("🧪 Running safe test pulse (450ms)", COL_INFO);
                    }
                }
            }

            if let Some(ref mut iep) = in_ep {
                while let Ok(buf) = iep.next_complete().now_or_never().flatten() {
                    if let Some((s_n, rt, lt, btns, p)) = decode_xbox_packet(&buf) {
                        steer = s_n;
                        throttle = rt;
                        brake = lt;
                        buttons = btns;
                        pov = p;

                        if buttons != prev_buttons {
                            for (mask, name) in BUTTON_NAMES {
                                let was = (prev_buttons & mask) != 0;
                                let now_p = (buttons & mask) != 0;
                                if was != now_p {
                                    add_log(
                                        format!("Button [{}] {}", name, if now_p { "PRESSED" } else { "RELEASED" }),
                                        COL_INFO,
                                    );
                                }
                            }
                            prev_buttons = buttons;
                        }
                    }
                }
            }

            let now = Instant::now();
            let dt = now.duration_since(last_kin_t).as_secs_f32().max(0.0005);
            last_kin_t = now;
            let vel = (steer - prev_pos) / dt;
            let acc = (vel - prev_vel) / dt;
            prev_pos = steer;
            prev_vel = vel;

            let tune = *TUNE.lock().unwrap();
            let send_interval = Duration::from_millis(tune.send_ms.max(4) as u64);

            let mut di_f = 0.0f32;
            let mut phys_f = 0.0f32;
            let mut vib_f = 0.0f32;

            if let Some(mix) = ffb_render(&RenderParams {
                pos: steer,
                vel,
                acc,
                vib_gain: tune.vib_gain,
                detail: tune.detail,
                texture: tune.texture,
                cond_on: tune.cond_on,
                cond_gain: tune.cond_gain,
            }) {
                di_f = mix.di;
                phys_f = mix.phys;
                vib_f = mix.vib;
            }

            // اختبار اهتزاز تجريبي خفيف
            let start_ms = START_TIME.get_or_init(Instant::now).elapsed().as_millis() as u32;
            let test_until = TEST_PULSE_UNTIL_MS.load(Ordering::Relaxed);
            if start_ms < test_until {
                let rem = (test_until - start_ms) as f32 / 1000.0;
                let osc = (rem * 36.0 * 2.0 * PI).sin() * 0.28;
                di_f += osc;
                vib_f = (vib_f + osc.abs()).min(1.0);
            }

            // عكس اتجاه الموتور موحد على القوة الفيزيائية والـ DI
            let dir_sign = if tune.invert_motor { -1.0 } else { 1.0 };
            let raw_combined = (di_f + phys_f) * dir_sign;

            if tune.auto_gain {
                let mag = raw_combined.abs();
                if mag > peak_seen {
                    peak_seen = peak_seen * 0.85 + mag * 0.15;
                } else {
                    peak_seen = (peak_seen * 0.998).max(AUTO_FLOOR);
                }
                let desired = (AUTO_TARGET / peak_seen.max(AUTO_FLOOR)).clamp(0.4, AUTO_MAX);
                auto_gain = auto_gain * 0.92 + desired * 0.08;
            } else {
                auto_gain = 1.0;
                peak_seen = peak_seen.max(AUTO_FLOOR);
            }

            let shaped = shape_force(raw_combined * auto_gain, &tune);
            let out_byte = (shaped.clamp(-1.0, 1.0) * 127.0) as i8;

            if now.duration_since(last_send_t) >= send_interval {
                last_send_t = now;
                if let Some(ref b) = vjoy_bridge {
                    b.send_axes(steer, throttle, brake);
                    b.send_buttons_and_pov(buttons, pov);
                }
                if let Some(ref mut oep) = out_ep {
                    let rumble_val = if tune.rumble_layer {
                        (vib_f.clamp(0.0, 1.0) * 255.0) as u8
                    } else {
                        0
                    };
                    let pkt = vec![0x00, 0x08, 0x00, rumble_val, rumble_val, 0x00, 0x00, 0x00];
                    let _ = oep.submit(Buffer::from(pkt));
                }
                SENT_PKTS.fetch_add(1, Ordering::Relaxed);
            }

            if now.duration_since(last_rate_t) >= Duration::from_secs(1) {
                let s_now = SENT_PKTS.load(Ordering::Relaxed);
                let r_now = RX_TOTAL.load(Ordering::Relaxed);
                pkt_per_sec = s_now.saturating_sub(last_sent_cnt);
                rx_per_sec = r_now.saturating_sub(last_rx_cnt);
                last_sent_cnt = s_now;
                last_rx_cnt = r_now;
                last_rate_t = now;
            }

            if now.duration_since(last_ui_t) >= Duration::from_millis(33) {
                last_ui_t = now;
                let mut seen = [0u32; 13];
                for i in 0..13 {
                    seen[i] = EFFECT_SEEN[i].load(Ordering::Relaxed);
                }
                let _ = tel_tx.send(Telemetry {
                    receiver_connected,
                    receiver_desc: receiver_desc.clone(),
                    vjoy_connected: vjoy_bridge.is_some(),
                    vjoy_status: vjoy_status.clone(),
                    steer_norm: steer,
                    throttle,
                    brake,
                    buttons,
                    pov,
                    ffb_byte: out_byte,
                    di_force: di_f,
                    phys_force: phys_f,
                    vib_level: vib_f,
                    final_force: shaped,
                    auto_gain_factor: auto_gain,
                    pkt_per_sec,
                    rx_per_sec,
                    rx_errors: RX_ERRORS.load(Ordering::Relaxed),
                    last_const_raw: LAST_CONST_RAW.load(Ordering::Relaxed),
                    active_mask: ACTIVE_MASK.load(Ordering::Relaxed),
                    seen_counts: seen,
                });
            }

            thread::sleep(Duration::from_millis(2));
        }
    });
}

// ============================================================================
//  EGUI UI (منظومة واجهة مفتوحة وقابلة للتمرير بدون تراكب أو ستارة)
// ============================================================================
struct WheelStudioApp {
    tel_rx: Receiver<Telemetry>,
    cmd_tx: Sender<UiCmd>,
    tel: Telemetry,
    tune: Tuning,
    preset_idx: usize,
    vjoy_id: u32,
}

impl WheelStudioApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.window_fill = Color32::from_rgb(16, 18, 24);
        style.visuals.panel_fill = Color32::from_rgb(20, 23, 31);
        cc.egui_ctx.set_style(style);

        let loaded_tune = Tuning::load();
        *TUNE.lock().unwrap() = loaded_tune;

        let (tel_tx, tel_rx) = channel();
        let (cmd_tx, cmd_rx) = channel();
        spawn_worker(tel_tx, cmd_rx);

        add_log("🚀 Xbox 360 Wheel Studio & FFB Feeder initialized", COL_OK);

        Self {
            tel_rx,
            cmd_tx,
            tel: Telemetry::default(),
            tune: loaded_tune,
            preset_idx: 0,
            vjoy_id: 1,
        }
    }

    fn sync_tune(&self) {
        *TUNE.lock().unwrap() = self.tune;
        self.tune.save();
    }

    fn draw_wheel_gauge(&self, ui: &mut egui::Ui) {
        let desired_size = Vec2::new(ui.available_width().min(360.0), 170.0);
        let (rect, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
        let painter = ui.painter_at(rect);

        painter.rect_filled(rect, CornerRadius::same(10), Color32::from_rgb(26, 30, 40));
        painter.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, Color32::from_rgb(52, 60, 78)),
            StrokeKind::Inside,
        );

        let center = Pos2::new(rect.center().x, rect.center().y + 6.0);
        let radius = 56.0;
        let angle_rad = self.tel.steer_norm * (135.0 * PI / 180.0);

        painter.circle_stroke(center, radius, Stroke::new(6.0, Color32::from_rgb(75, 86, 110)));

        let spoke_end = Pos2::new(
            center.x + angle_rad.sin() * radius,
            center.y - angle_rad.cos() * radius,
        );
        painter.line_segment([center, spoke_end], Stroke::new(4.0, COL_INFO));
        painter.circle_filled(center, 8.0, COL_OK);

        let force_bar_w = (rect.width() - 40.0) * 0.5;
        let bar_y = rect.bottom() - 22.0;
        let mid_x = rect.center().x;
        painter.line_segment(
            [Pos2::new(mid_x - force_bar_w, bar_y), Pos2::new(mid_x + force_bar_w, bar_y)],
            Stroke::new(6.0, Color32::from_rgb(42, 48, 62)),
        );
        let f_offset = self.tel.final_force.clamp(-1.0, 1.0) * force_bar_w;
        let f_col = if self.tel.final_force.abs() > 0.7 {
            COL_WARN
        } else {
            COL_OK
        };
        painter.line_segment(
            [Pos2::new(mid_x, bar_y), Pos2::new(mid_x + f_offset, bar_y)],
            Stroke::new(6.0, f_col),
        );

        let deg = self.tel.steer_norm * 135.0;
        painter.text(
            Pos2::new(rect.left() + 12.0, rect.top() + 12.0),
            egui::Align2::LEFT_TOP,
            format!("Steer: {:+.1}°", deg),
            egui::FontId::monospace(13.0),
            Color32::WHITE,
        );
        painter.text(
            Pos2::new(rect.right() - 12.0, rect.top() + 12.0),
            egui::Align2::RIGHT_TOP,
            format!("FFB: {:+} ({:+.2})", self.tel.ffb_byte, self.tel.final_force),
            egui::FontId::monospace(13.0),
            f_col,
        );
    }
}

impl eframe::App for WheelStudioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(t) = self.tel_rx.try_recv() {
            self.tel = t;
        }

        // شريط علوي ثابت بأزرار التحكم السريع والـ PANIC
        egui::TopBottomPanel::top("top_bar")
            .exact_height(46.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.heading("🎮 Xbox 360 Wheel Studio");
                    ui.separator();

                    let r_col = if self.tel.receiver_connected { COL_OK } else { COL_ERR };
                    ui.colored_label(r_col, format!("RX: {}", self.tel.receiver_desc));

                    let v_col = if self.tel.vjoy_connected { COL_OK } else { COL_WARN };
                    ui.colored_label(v_col, &self.tel.vjoy_status);

                    ui.separator();
                    ui.label("vJoy ID:");
                    if ui.add(egui::DragValue::new(&mut self.vjoy_id).range(1..=16)).changed() {
                        let _ = self.cmd_tx.send(UiCmd::SetVjoyId(self.vjoy_id));
                    }

                    if ui.button("🔄 Reconnect").clicked() {
                        let _ = self.cmd_tx.send(UiCmd::ReconnectAll);
                    }

                    if ui.button("🧪 Test Pulse").clicked() {
                        let _ = self.cmd_tx.send(UiCmd::TriggerTestPulse);
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let panic_btn = egui::Button::new(
                            egui::RichText::new("⛔ PANIC STOP").color(Color32::WHITE).strong(),
                        )
                        .fill(COL_ERR);
                        if ui.add(panic_btn).clicked() {
                            let _ = self.cmd_tx.send(UiCmd::PanicStop);
                        }
                    });
                });
            });

        // مساحة وسطى قابلة للتمرير بالكامل بدون أي عنصر يحجب باقي الواجهة
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.columns(2, |cols| {
                        // العمود الأيسر: المعاينة + الإشارات + اللوج
                        cols[0].group(|ui| {
                            ui.set_min_width(cols[0].available_width() - 12.0);
                            ui.strong("Live Wheel & Telemetry");
                            ui.add_space(4.0);
                            self.draw_wheel_gauge(ui);

                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.label(format!("Throttle: {:.0}%", self.tel.throttle * 100.0));
                                ui.separator();
                                ui.label(format!("Brake: {:.0}%", self.tel.brake * 100.0));
                                ui.separator();
                                ui.label(format!("Const Raw: {:+}", self.tel.last_const_raw));
                            });

                            ui.horizontal(|ui| {
                                ui.label(format!("OUT/s: {}", self.tel.pkt_per_sec));
                                ui.separator();
                                ui.label(format!("FFB RX/s: {}", self.tel.rx_per_sec));
                                ui.separator();
                                ui.label(format!("Errors: {}", self.tel.rx_errors));
                                if self.tune.auto_gain {
                                    ui.separator();
                                    ui.label(format!("AutoGain: {:.2}x", self.tel.auto_gain_factor));
                                }
                            });

                            ui.add_space(6.0);
                            ui.strong("Detected FFB Effect Types");
                            ui.horizontal_wrapped(|ui| {
                                for t in 1..=11u32 {
                                    let active = (self.tel.active_mask & (1u32 << (t & 31))) != 0;
                                    let seen = self.tel.seen_counts[t as usize] > 0;
                                    let col = if active {
                                        COL_OK
                                    } else if seen {
                                        COL_INFO
                                    } else {
                                        Color32::GRAY
                                    };
                                    ui.colored_label(col, format!("[{}]", etype_name(t)));
                                }
                            });

                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.strong("Event Log");
                                if ui.small_button("Clear").clicked() {
                                    LOG_ENTRIES.lock().unwrap().clear();
                                }
                                let mut dbg = FFB_DEBUG.load(Ordering::Relaxed);
                                if ui.checkbox(&mut dbg, "Verbose FFB").changed() {
                                    FFB_DEBUG.store(dbg, Ordering::Relaxed);
                                }
                            });

                            egui::ScrollArea::vertical()
                                .id_salt("log_scroll")
                                .max_height(220.0)
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    let logs = LOG_ENTRIES.lock().unwrap();
                                    for entry in logs.iter() {
                                        ui.colored_label(
                                            entry.color,
                                            format!("[{:6.2}s] {}", entry.time_s, entry.text),
                                        );
                                    }
                                });
                        });

                        // العمود الأيمن: الإعدادات والـ Presets والفيزياء
                        cols[1].group(|ui| {
                            ui.set_min_width(cols[1].available_width() - 12.0);
                            ui.strong("Presets & Balance");
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                let presets = ["Balanced (Default)", "Rally", "Arcade", "Sim"];
                                for (i, label) in presets.iter().enumerate() {
                                    if ui.selectable_label(self.preset_idx == i, *label).clicked() {
                                        self.preset_idx = i;
                                        self.tune.apply_preset(i);
                                        self.sync_tune();
                                    }
                                }
                            });

                            ui.add_space(8.0);
                            ui.strong("Road & Force Feel");
                            let mut changed = false;

                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.vib_gain, 0.0..=2.5).text("Periodic Vib"))
                                .changed();
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.texture, 0.0..=1.5).text("Road Texture"))
                                .changed();
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.detail, 0.0..=1.5).text("Impact Detail"))
                                .changed();

                            ui.add_space(6.0);
                            ui.strong("Centering & Damping (Conditions)");
                            changed |= ui.checkbox(&mut self.tune.cond_on, "Enable Spring / Damper / Friction").changed();
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.cond_gain, 0.0..=1.2).text("Condition Gain"))
                                .changed();

                            ui.add_space(6.0);
                            ui.strong("Motor Curve & Safety Limits");
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.min_force, 0.0..=0.12).text("Min Force (Deadzone)"))
                                .changed();
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.low_boost, 0.0..=0.7).text("Low-Force Boost"))
                                .changed();
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.max_out, 0.20..=1.0).text("Max Output"))
                                .changed();
                            changed |= ui
                                .add(egui::Slider::new(&mut self.tune.send_ms, 4..=24).text("Send Interval (ms)"))
                                .changed();

                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                changed |= ui.checkbox(&mut self.tune.soft_limit, "Soft Limiter").changed();
                                changed |= ui.checkbox(&mut self.tune.auto_gain, "Auto Gain").changed();
                                changed |= ui.checkbox(&mut self.tune.invert_motor, "Invert Motor").changed();
                                changed |= ui.checkbox(&mut self.tune.rumble_layer, "Rumble Layer").changed();
                            });

                            if changed {
                                self.sync_tune();
                            }
                        });
                    });
                });
        });

        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 680.0])
            .with_min_inner_size([720.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Xbox 360 Wheel Studio & FFB Feeder",
        options,
        Box::new(|cc| Ok(Box::new(WheelStudioApp::new(cc)))),
    )
}
