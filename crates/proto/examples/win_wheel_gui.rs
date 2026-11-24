#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// ============================================================================
//  Xbox 360 Wheel Studio & FFB Feeder  -  "Full FFB Engine" edition
//
//  اللي اتضاف عن النسخة اللي فاتت:
//   * الـ Engine بقى بيفهم كل أنواع الـ Effects اللي بتبعتها اللعبة، مش Constant بس:
//       Constant / Ramp / Square / Sine / Triangle / SawUp / SawDown   (مطبات + اهتزاز الأرض + الكيربات)
//       Spring / Damper / Friction                                       (ثقل الدريكسيون + رجوع للمنتصف)
//       + Envelope (Attack / Fade) + Loop Count + Gain لكل Effect
//   * Mixer بيحسب القوة النهائية كل ~8ms ويبعتها للدريكسيون بسرعة أعلى (قابلة للتعديل)
//   * "Impact Detail" بيكبّر بداية أي خبطة / نزلة من فوق مطب
//   * شريط بيوريك اللعبة بتستخدم أنهي Effects فعلاً (لو SINE/SQR مش منوّرين -> اللعبة مش بتبعتهم)
//   * Detect Motor Direction عشان الـ Spring/Damper يشتغلوا في الاتجاه الصح مهما كان توصيل الموتور
//   * (تجريبي) طبقة Rumble لو الدريكسيون بيستجيب لأوامر الاهتزاز
// ============================================================================

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2};
use nusb::transfer::{Buffer, In, Interrupt, Out};
use nusb::MaybeFuture;
use std::ffi::c_void;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

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

// ============================================================================
//  FFB ENGINE  (كل Effect بيتتبع لوحده + Mixer بيجمعهم)
// ============================================================================
const MAX_EFFECTS: usize = 64;
const COL_DBG: Color32 = Color32::from_rgb(150, 150, 255);

// FFBEType في vJoy
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

/// أقصى سرعة لف (بوحدة: المدى الكامل للدريكسيون في الثانية) بتتحسب عندها قوة الـ Damper 100%
const DAMPER_FULL_VEL: f32 = 3.0;
/// السرعة اللي عندها الـ Friction توصل 100%
const FRICTION_FULL_VEL: f32 = 0.25;

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

#[allow(clippy::declare_interior_mutable_const)]
const ATOMIC_ZERO: AtomicU32 = AtomicU32::new(0);
/// عداد: كام مرة اللعبة بعتت Effect من كل نوع (بيتعرض في الواجهة)
static EFFECT_SEEN: [AtomicU32; 13] = [ATOMIC_ZERO; 13];
/// عداد الباكتات اللي اتبعتت فعلاً للدريكسيون (عشان نعرض pkt/s)
static SENT_PKTS: AtomicU32 = AtomicU32::new(0);

fn note_effect_seen(t: u32) {
    let i = t as usize;
    if i < EFFECT_SEEN.len() && EFFECT_SEEN[i].fetch_add(1, Ordering::Relaxed) == 0 {
        add_log(
            format!("🎮 Game is using effect type: {}", etype_name(t)),
            Color32::from_rgb(0, 230, 118),
        );
    }
}

/// إعدادات بتتعدل من الواجهة وبتتقري في الـ worker كل لفة
#[derive(Clone, Copy)]
struct Tuning {
    vib_gain: f32,      // تكبير الاهتزاز (Periodic effects: مطبات / كيربات / رصيف)
    detail: f32,        // تكبير بداية الخبطات
    cond_on: bool,      // Spring / Damper / Friction
    cond_gain: f32,
    rumble_layer: bool, // تجريبي
    send_ms: u32,       // أقل فاصل بين باكتين للدريكسيون
}

impl Tuning {
    const DEFAULT: Tuning = Tuning {
        vib_gain: 1.5,
        detail: 0.6,
        cond_on: true,
        cond_gain: 0.7,
        rumble_layer: false,
        send_ms: 8,
    };
}

static TUNE: Mutex<Tuning> = Mutex::new(Tuning::DEFAULT);

#[derive(Clone, Copy)]
struct Effect {
    etype: u32,          // FFBEType (0 = لسه مش معروف)
    running: bool,       // اللعبة عملت Start
    started: Option<Instant>,
    duration_ms: u32,    // 0 = للأبد
    loops: u32,          // Loop count (0/1 = مرة، 255 = للأبد)
    gain: f32,           // Gain الخاص بالـ effect (0..1)
    dir_factor: f32,     // إسقاط اتجاه الـ effect على محور الدريكسيون (-1..1)
    // Constant
    magnitude: i32,      // -10000..10000
    // Ramp
    ramp_start: i32,
    ramp_end: i32,
    // Periodic
    p_mag: u32,          // 0..10000
    p_offset: i32,       // -10000..10000
    p_phase: u32,        // درجات
    p_period: u32,       // ms
    // Envelope
    has_env: bool,
    atk_level: u32,
    fade_level: u32,
    atk_time: u32,
    fade_time: u32,
    // Condition (محور X بس)
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

    /// المدة الكلية بالمللي ثانية (None = للأبد)
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

static FFB_DEBUG: AtomicBool = AtomicBool::new(true);
static SEEN_EFOP: AtomicBool = AtomicBool::new(false);
static DBG_LAST: Mutex<[Option<Instant>; 12]> = Mutex::new([None; 12]);

/// Rate-limiter للوج عشان اللوج ما يتملاش بالـ 60 باكت في الثانية
fn dbg_allowed(slot: usize, min_ms: u64) -> bool {
    if !FFB_DEBUG.load(Ordering::Relaxed) {
        return false;
    }
    let mut g = DBG_LAST.lock().unwrap();
    let ok = g[slot].map_or(true, |t| t.elapsed() >= Duration::from_millis(min_ms));
    if ok {
        g[slot] = Some(Instant::now());
    }
    ok
}

/// في الوضع القديم (لسه ما جاش EffectOp من اللعبة) أي effect بيوصله بيانات يشتغل فوراً
fn legacy_autostart(e: &mut Effect) {
    if !SEEN_EFOP.load(Ordering::Relaxed) && !e.running {
        e.running = true;
        e.started = Some(Instant::now());
    }
}

// ---------------------------------------------------------------------------
//  Mixer
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, Default)]
struct FfbMix {
    /// Constant + Ramp + Periodic. بنفس اتجاه DirectInput (بيعدّي على Invert)
    di: f32,
    /// Spring / Damper / Friction. بالاتجاه الفيزيائي (موجب = ناحية +steering)
    phys: f32,
    /// "طاقة" الاهتزاز 0..1 (للـ Rumble layer)
    vib: f32,
    active: bool,
}

#[derive(Clone, Copy)]
struct RenderParams {
    pos: f32, // -1..1
    vel: f32, // وحدات/ثانية (المدى الكامل = 2.0)
    vib_gain: f32,
    cond_on: bool,
    cond_gain: f32,
}

/// شكل الموجة: ph من 0..1
fn wave(t: u32, p: f32) -> f32 {
    match t {
        ET_SQR => {
            if p < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
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

/// Envelope: بيعدّل "حجم" الـ effect (amp >= 0) بحسب Attack / Fade
fn envelope_amp(e: &Effect, amp: f32, t_ms: f64, total_ms: Option<f64>) -> f32 {
    if !e.has_env {
        return amp;
    }
    let mut a = amp;
    if e.atk_time > 0 && t_ms < e.atk_time as f64 {
        let atk = e.atk_level as f32 / 10000.0;
        let k = (t_ms / e.atk_time as f64) as f32;
        a = atk + (amp - atk) * k;
    }
    if let Some(total) = total_ms {
        if e.fade_time > 0 {
            let fade_start = total - e.fade_time as f64;
            if t_ms > fade_start {
                let fade = e.fade_level as f32 / 10000.0;
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
        (s as f32 / 10000.0).min(1.0)
    }
}

/// Spring / Damper / Friction (بالاتجاه الفيزيائي: موجب = ناحية +steering)
fn cond_force(t: u32, e: &Effect, pos: f32, vel: f32) -> f32 {
    match t {
        ET_SPRNG => {
            let cp = e.c_center as f32 / 10000.0;
            let db = (e.c_dead as f32 / 10000.0).abs();
            let x = pos - cp;
            if x > db {
                let s = sat_of(e.c_pos_sat);
                (-(e.c_pos_coeff as f32 / 10000.0) * (x - db)).clamp(-s, s)
            } else if x < -db {
                let s = sat_of(e.c_neg_sat);
                (-(e.c_neg_coeff as f32 / 10000.0) * (x + db)).clamp(-s, s)
            } else {
                0.0
            }
        }
        ET_DMPR | ET_FRCTN => {
            let full = if t == ET_DMPR { DAMPER_FULL_VEL } else { FRICTION_FULL_VEL };
            let v = (vel / full).clamp(-1.0, 1.0);
            let (c, s) = if v >= 0.0 {
                (e.c_pos_coeff, sat_of(e.c_pos_sat))
            } else {
                (e.c_neg_coeff, sat_of(e.c_neg_sat))
            };
            (-(c as f32 / 10000.0) * v).clamp(-s, s)
        }
        _ => 0.0,
    }
}

/// بيجمع كل الـ Effects الشغالة دلوقتي. None = مفيش ولا Effect شغال.
fn ffb_render(rp: &RenderParams) -> Option<FfbMix> {
    let mut s = FFB.lock().unwrap();
    if !s.actuators_on || s.paused {
        return None;
    }
    let dev = s.device_gain as f32 / 255.0;
    let now = Instant::now();
    let mut mix = FfbMix::default();

    for e in s.effects.iter_mut() {
        if !e.running || e.etype == 0 {
            continue;
        }
        let t0 = *e.started.get_or_insert(now);
        let t_ms = now.duration_since(t0).as_secs_f64() * 1000.0;
        let total = e.total_ms();

        // Effect له مدة محددة (زي صدمة الحيطة): الجهاز هو اللي لازم يوقفه لما المدة تخلص
        if let Some(tot) = total {
            if t_ms >= tot {
                e.running = false;
                continue;
            }
        }
        mix.active = true;

        match e.etype {
            ET_CONST => {
                let m = e.magnitude as f32 / 10000.0;
                let a = envelope_amp(e, m.abs(), t_ms, total);
                mix.di += a * m.signum() * e.gain * e.dir_factor;
            }
            ET_RAMP => {
                let k = if e.duration_ms > 0 {
                    let d = e.duration_ms as f64;
                    ((t_ms % d) / d) as f32
                } else {
                    0.0
                };
                let v = e.ramp_start as f32 + (e.ramp_end as f32 - e.ramp_start as f32) * k;
                mix.di += (v / 10000.0) * e.gain * e.dir_factor;
            }
            t if is_periodic(t) => {
                let period = e.p_period.max(1) as f64;
                let ph = ((t_ms / period) + e.p_phase as f64 / 360.0).fract() as f32;
                // الجهاز بيتحدّث ~125 مرة في الثانية، فالترددات العالية جداً بنقلل منها شوية
                let freq = 1000.0 / period as f32;
                let att = if freq > 45.0 { (45.0 / freq).max(0.35) } else { 1.0 };
                let amp = envelope_amp(e, e.p_mag as f32 / 10000.0, t_ms, total);
                let osc = amp * wave(t, ph) * att;
                let off = e.p_offset as f32 / 10000.0;
                let k = e.gain * e.dir_factor;
                mix.di += (off + osc * rp.vib_gain) * k;
                mix.vib += (osc * e.gain).abs();
            }
            t if is_condition(t) => {
                if rp.cond_on && e.has_cond {
                    mix.phys += cond_force(t, e, rp.pos, rp.vel) * e.gain * rp.cond_gain;
                }
            }
            _ => {}
        }
    }

    if !mix.active {
        return None;
    }
    mix.di *= dev;
    mix.phys *= dev;
    mix.vib *= dev;
    Some(mix)
}

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

    // أنواع باكتات الـ FFB (FFBPType = رقم الـ HID PID report)
    const PT_EFFREP: u32 = 0x01; // Set Effect
    const PT_ENVREP: u32 = 0x02; // Envelope
    const PT_CONDREP: u32 = 0x03; // Condition (Spring / Damper / Friction / Inertia)
    const PT_PRIDREP: u32 = 0x04; // Periodic (Sine / Square / ...)
    const PT_CONSTREP: u32 = 0x05; // Constant Force
    const PT_RAMPREP: u32 = 0x06; // Ramp
    const PT_EFOPREP: u32 = 0x0A; // Effect Operation
    const PT_BLKFRREP: u32 = 0x0B; // Block Free
    const PT_CTRLREP: u32 = 0x0C; // Device Control
    const PT_GAINREP: u32 = 0x0D; // Device Gain
    // أنواع معروفة بس مش محتاجينها (Custom data / New effect / Block load / Pool)
    const PT_IGNORED: [u32; 6] = [0x07, 0x08, 0x0E, 0x11, 0x12, 0x13];

    // ملحوظة: الـ structs دي بتطابق vJoy SDK (محاذاة 4 bytes)، وزودنا _tail احتياطي عشان لو الحجم
    // الحقيقي أكبر ما يحصلش كتابة خارج الـ buffer.
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

    // Ffb_h_Type / Ffb_h_EBI / Ffb_h_DevCtrl / Ffb_h_DevGain: بنادي عليهم بـ u32 مصفّر
    // (لو الدالة بتكتب BYTE بس، باقي الـ bytes تفضل صفر)
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

    /// "name\0" كـ &[u8]
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

    /// إسقاط اتجاه الـ effect على محور الدريكسيون.
    /// Polar: 0..255 = 0..360 درجة (0 = فوق، 90 = يمين، 270 = شمال) => الإسقاط على X = sin(زاوية)
    /// Cartesian: DirX/DirY (signed byte)
    /// لو الاتجاه "مش محدد" (قريب من 0 أو 180 درجة) نعتبره قوة كاملة على X.
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
        let ctx = unsafe { &*(p_ctx as *const FfbContext) };
        let f = ctx.fns;

        let ptype = unsafe { call_u32(f.packet_type, p_packet) }.unwrap_or(0);
        let ebi: Option<usize> = unsafe { call_u32(f.ebi, p_packet) }.map(slot_index);

        match ptype {
            // ---------- Set Effect Report: النوع + المدة + الاتجاه + الـ Gain ----------
            PT_EFFREP => {
                let Some(fr) = f.eff_report else { return; };
                let mut rep = FfbEffReport::default();
                if unsafe { fr(p_packet, &mut rep) } != 0 {
                    return;
                }
                let t = rep.effect_type;
                // لو النوع مش منطقي ما نصدقش باقي الحقول (حماية من اختلاف شكل الـ struct)
                if t == 0 || t > ET_CSTM {
                    if dbg_allowed(0, 400) {
                        add_log(
                            format!("FFB EffReport #{}: bad type={} -> ignored", rep.effect_block_index, t),
                            COL_DBG,
                        );
                    }
                    return;
                }
                note_effect_seen(t);
                let idx = slot_index(rep.effect_block_index as u32);
                let polar = rep.polar != 0;
                let factor = dir_factor(polar, rep.direction, rep.dir_y);
                let dur = if rep.duration == 0 || rep.duration >= 32767 { 0 } else { rep.duration as u32 };
                // Gain = 0 غالباً معناها إن الـ struct اتقرا غلط، فبنعتبره 100%
                let gain = if rep.gain == 0 { 1.0 } else { rep.gain as f32 / 255.0 };
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    e.etype = t;
                    e.duration_ms = dur;
                    e.dir_factor = factor;
                    e.gain = gain;
                }
                if dbg_allowed(0, 100) {
                    add_log(
                        format!(
                            "FFB EffReport #{}: {} dur={}ms gain={} polar={} dir={} dirY={} -> X factor {:+.2}",
                            idx, etype_name(t), dur, rep.gain, polar, rep.direction, rep.dir_y, factor
                        ),
                        COL_DBG,
                    );
                }
            }

            // ---------- Envelope: Attack / Fade ----------
            PT_ENVREP => {
                let Some(fe) = f.eff_envlp else { return; };
                let mut en = FfbEffEnvlp::default();
                if unsafe { fe(p_packet, &mut en) } != 0 {
                    return;
                }
                if en.attack_level > 20000 || en.fade_level > 20000 || en.attack_time > 40000 || en.fade_time > 40000 {
                    return;
                }
                let idx = slot_index(en.effect_block_index as u32);
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    e.has_env = true;
                    e.atk_level = en.attack_level;
                    e.fade_level = en.fade_level;
                    e.atk_time = en.attack_time;
                    e.fade_time = en.fade_time;
                }
                if dbg_allowed(10, 300) {
                    add_log(
                        format!(
                            "FFB Envelope #{}: attack {}@{}ms  fade {}@{}ms",
                            idx, en.attack_level, en.attack_time, en.fade_level, en.fade_time
                        ),
                        COL_DBG,
                    );
                }
            }

            // ---------- Set Constant Force: قيمة القوة ----------
            PT_CONSTREP => {
                let Some(fc) = f.eff_const else { return; };
                let mut c = FfbEffConstant::default();
                if unsafe { fc(p_packet, &mut c) } != 0 {
                    return;
                }
                let idx = slot_index(c.effect_block_index as u32);
                if c.magnitude.abs() > 10000 {
                    if dbg_allowed(2, 500) {
                        add_log(format!("FFB Const #{}: out-of-range magnitude {} ignored", idx, c.magnitude), COL_DBG);
                    }
                    return;
                }
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    if e.etype == 0 {
                        e.etype = ET_CONST;
                    }
                    e.magnitude = c.magnitude;
                    legacy_autostart(e);
                }
                if dbg_allowed(1, 200) {
                    add_log(format!("FFB Const #{}: magnitude={:+}", idx, c.magnitude), COL_DBG);
                }
            }

            // ---------- Ramp ----------
            PT_RAMPREP => {
                let Some(fr) = f.eff_ramp else { return; };
                let mut r = FfbEffRamp::default();
                if unsafe { fr(p_packet, &mut r) } != 0 {
                    return;
                }
                if r.start.abs() > 10000 || r.end.abs() > 10000 {
                    return;
                }
                let idx = slot_index(r.effect_block_index as u32);
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    if e.etype == 0 {
                        e.etype = ET_RAMP;
                    }
                    e.ramp_start = r.start;
                    e.ramp_end = r.end;
                    legacy_autostart(e);
                }
                if dbg_allowed(9, 200) {
                    add_log(format!("FFB Ramp #{}: {:+} -> {:+}", idx, r.start, r.end), COL_DBG);
                }
            }

            // ---------- Periodic: Sine / Square / Triangle / Saw (مطبات واهتزازات) ----------
            PT_PRIDREP => {
                let Some(fp) = f.eff_period else { return; };
                let mut p = FfbEffPeriod::default();
                if unsafe { fp(p_packet, &mut p) } != 0 {
                    return;
                }
                if p.magnitude > 20000 || p.offset.abs() > 20000 {
                    return;
                }
                let idx = slot_index(p.effect_block_index as u32);
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    if e.etype == 0 {
                        e.etype = ET_SINE; // لحد ما يجي EffectReport بالنوع الحقيقي
                    }
                    e.p_mag = p.magnitude.min(10000);
                    e.p_offset = p.offset.clamp(-10000, 10000);
                    e.p_phase = p.phase;
                    e.p_period = p.period.clamp(1, 32767);
                    legacy_autostart(e);
                }
                if dbg_allowed(7, 150) {
                    add_log(
                        format!(
                            "FFB Periodic #{}: mag={} offset={:+} period={}ms (~{:.0}Hz) phase={}",
                            idx,
                            p.magnitude,
                            p.offset,
                            p.period,
                            1000.0 / p.period.max(1) as f32,
                            p.phase
                        ),
                        COL_DBG,
                    );
                }
            }

            // ---------- Condition: Spring / Damper / Friction ----------
            PT_CONDREP => {
                let Some(fc) = f.eff_cond else { return; };
                let mut c = FfbEffCond::default();
                if unsafe { fc(p_packet, &mut c) } != 0 {
                    return;
                }
                if c.is_y != 0 {
                    return; // الدريكسيون محور X بس
                }
                if c.pos_coeff.abs() > 20000 || c.neg_coeff.abs() > 20000 || c.center_offset.abs() > 20000 {
                    return;
                }
                let idx = slot_index(c.effect_block_index as u32);
                {
                    let mut s = FFB.lock().unwrap();
                    let e = &mut s.effects[idx];
                    if e.etype == 0 {
                        e.etype = ET_SPRNG;
                    }
                    e.has_cond = true;
                    e.c_center = c.center_offset.clamp(-10000, 10000);
                    e.c_pos_coeff = c.pos_coeff.clamp(-10000, 10000);
                    e.c_neg_coeff = c.neg_coeff.clamp(-10000, 10000);
                    e.c_pos_sat = c.pos_satur.min(10000);
                    e.c_neg_sat = c.neg_satur.min(10000);
                    e.c_dead = c.dead_band.clamp(0, 10000);
                    legacy_autostart(e);
                }
                if dbg_allowed(8, 300) {
                    add_log(
                        format!(
                            "FFB Condition #{}: cp={:+} coeff=+{}/{} sat={}/{} dead={}",
                            idx, c.center_offset, c.pos_coeff, c.neg_coeff, c.pos_satur, c.neg_satur, c.dead_band
                        ),
                        COL_DBG,
                    );
                }
            }

            // ---------- Effect Operation: Start / Solo / Stop ----------
            PT_EFOPREP => {
                let Some(fo) = f.eff_op else { return; };
                let mut op = FfbEffOp::default();
                if unsafe { fo(p_packet, &mut op) } != 0 {
                    return;
                }
                SEEN_EFOP.store(true, Ordering::Relaxed);
                let idx = slot_index(op.effect_block_index as u32);
                {
                    let mut s = FFB.lock().unwrap();
                    match op.effect_op {
                        1 => {
                            let e = &mut s.effects[idx];
                            e.running = true;
                            e.started = Some(Instant::now());
                            e.loops = op.loop_count as u32;
                        }
                        2 => {
                            for (i, e) in s.effects.iter_mut().enumerate() {
                                if i == idx {
                                    e.running = true;
                                    e.started = Some(Instant::now());
                                    e.loops = op.loop_count as u32;
                                } else {
                                    e.running = false;
                                }
                            }
                        }
                        3 => {
                            s.effects[idx].running = false;
                        }
                        _ => {}
                    }
                }
                if dbg_allowed(3, 100) {
                    let name = match op.effect_op {
                        1 => "START",
                        2 => "SOLO",
                        3 => "STOP",
                        _ => "?",
                    };
                    let ty = FFB.lock().unwrap().effects[idx].etype;
                    add_log(
                        format!("FFB EffOp #{} ({}): {} loops={}", idx, etype_name(ty), name, op.loop_count),
                        COL_DBG,
                    );
                }
            }

            // ---------- Block Free: اللعبة مسحت الـ effect ----------
            PT_BLKFRREP => {
                if let Some(idx) = ebi {
                    FFB.lock().unwrap().effects[idx] = Effect::EMPTY;
                    if dbg_allowed(4, 100) {
                        add_log(format!("FFB BlockFree #{}", idx), COL_DBG);
                    }
                }
            }

            // ---------- Device Control: Enable / Disable / StopAll / Reset / Pause / Continue ----------
            PT_CTRLREP => {
                let Some(ctrl) = (unsafe { call_u32(f.dev_ctrl, p_packet) }) else { return; };
                let code = ctrl & 0xFF;
                {
                    let mut s = FFB.lock().unwrap();
                    match code {
                        1 => s.actuators_on = true,
                        2 => s.actuators_on = false,
                        3 => {
                            for e in s.effects.iter_mut() {
                                e.running = false;
                            }
                        }
                        4 => {
                            s.effects = [Effect::EMPTY; MAX_EFFECTS];
                            s.actuators_on = true;
                            s.paused = false;
                        }
                        5 => s.paused = true,
                        6 => s.paused = false,
                        _ => {}
                    }
                }
                if dbg_allowed(5, 50) {
                    let name = match code {
                        1 => "ENABLE actuators",
                        2 => "DISABLE actuators",
                        3 => "STOP ALL",
                        4 => "DEVICE RESET",
                        5 => "PAUSE",
                        6 => "CONTINUE",
                        _ => "?",
                    };
                    add_log(format!("FFB DevCtrl: {}", name), COL_DBG);
                }
            }

            // ---------- Device Gain ----------
            PT_GAINREP => {
                if let Some(g) = unsafe { call_u32(f.dev_gain, p_packet) } {
                    FFB.lock().unwrap().device_gain = (g & 0xFF) as u8;
                    if dbg_allowed(6, 200) {
                        add_log(format!("FFB DeviceGain = {}", g & 0xFF), COL_DBG);
                    }
                }
            }

            t if PT_IGNORED.contains(&t) => {}

            // ---------- نوع مش معروف: نجرب الطريقة القديمة (Constant Force بس) ----------
            other => {
                if let Some(fc) = f.eff_const {
                    let mut c = FfbEffConstant::default();
                    if unsafe { fc(p_packet, &mut c) } == 0 && c.magnitude.abs() <= 10000 {
                        let idx = slot_index(c.effect_block_index as u32);
                        let mut s = FFB.lock().unwrap();
                        let e = &mut s.effects[idx];
                        e.etype = ET_CONST;
                        e.magnitude = c.magnitude;
                        e.running = true;
                    }
                }
                if dbg_allowed(2, 1000) {
                    add_log(format!("FFB packet with unknown type 0x{:02X} (legacy path)", other), COL_DBG);
                }
            }
        }
    }

    pub struct VJoy {
        pub enabled: bool,
        pub acquired: bool,
        pub dev_id: UINT,
        p_status: Option<FnGetVJDStatus>,
        p_acquire: Option<FnAcquireVJD>,
        p_set_axis: Option<FnSetAxis>,
        p_set_btn: Option<FnSetBtn>,
        p_set_disc_pov: Option<FnSetDiscPov>,
        p_relinquish: Option<FnRelinquishVJD>,
        p_ffb_start: Option<FnFfbStart>,
        p_ffb_reg: Option<FnFfbRegisterGenCB>,
        pub ffb_fns: FfbFns,
    }

    impl VJoy {
        pub fn load(dev_id: UINT) -> Self {
            let mut vjoy = Self {
                enabled: false,
                acquired: false,
                dev_id,
                p_status: None,
                p_acquire: None,
                p_set_axis: None,
                p_set_btn: None,
                p_set_disc_pov: None,
                p_relinquish: None,
                p_ffb_start: None,
                p_ffb_reg: None,
                ffb_fns: FfbFns::default(),
            };

            let paths: [&[u8]; 3] = [
                b"vJoyInterface.dll\0",
                b"C:\\Program Files\\vJoy\\x64\\vJoyInterface.dll\0",
                b"C:\\Program Files\\vJoy\\vJoyInterface.dll\0",
            ];

            let mut h_mod = std::ptr::null_mut();
            for p in paths {
                unsafe {
                    h_mod = LoadLibraryA(p.as_ptr());
                    if !h_mod.is_null() {
                        break;
                    }
                }
            }

            if h_mod.is_null() {
                add_log("vJoyInterface.dll not found in standard paths", Color32::from_rgb(255, 171, 0));
                return vjoy;
            }

            unsafe {
                let get_fn = |name: &[u8]| -> *mut c_void { GetProcAddress(h_mod, name.as_ptr()) };
                // بيجرب كذا اسم للدالة (اختلاف بسيط بين نسخ vJoyInterface.dll)
                let get_any = |names: &[&[u8]]| -> *mut c_void {
                    for n in names {
                        let p = GetProcAddress(h_mod, n.as_ptr());
                        if !p.is_null() {
                            return p;
                        }
                    }
                    std::ptr::null_mut()
                };

                let fn_enabled: Option<FnVJoyEnabled> = std::mem::transmute(get_fn(b"vJoyEnabled\0"));
                vjoy.p_status = std::mem::transmute(get_fn(b"GetVJDStatus\0"));
                vjoy.p_acquire = std::mem::transmute(get_fn(b"AcquireVJD\0"));
                vjoy.p_set_axis = std::mem::transmute(get_fn(b"SetAxis\0"));
                vjoy.p_set_btn = std::mem::transmute(get_fn(b"SetBtn\0"));
                vjoy.p_set_disc_pov = std::mem::transmute(get_fn(b"SetDiscPov\0"));
                vjoy.p_relinquish = std::mem::transmute(get_fn(b"RelinquishVJD\0"));

                vjoy.p_ffb_start = std::mem::transmute(get_fn(b"FfbStart\0"));
                vjoy.p_ffb_reg = std::mem::transmute(get_fn(b"FfbRegisterGenCB\0"));

                vjoy.ffb_fns.packet_type = std::mem::transmute(get_fn(b"Ffb_h_Type\0"));
                vjoy.ffb_fns.ebi = std::mem::transmute(get_fn(b"Ffb_h_EBI\0"));
                vjoy.ffb_fns.eff_report = std::mem::transmute(get_fn(b"Ffb_h_Eff_Report\0"));
                vjoy.ffb_fns.eff_const =
                    std::mem::transmute(get_any(&[cs!("Ffb_h_Eff_Constant"), cs!("Ffb_h_Eff_Const")]));
                vjoy.ffb_fns.eff_ramp = std::mem::transmute(get_any(&[cs!("Ffb_h_Eff_Ramp")]));
                vjoy.ffb_fns.eff_period =
                    std::mem::transmute(get_any(&[cs!("Ffb_h_Eff_Period"), cs!("Ffb_h_Eff_Periodic")]));
                vjoy.ffb_fns.eff_cond =
                    std::mem::transmute(get_any(&[cs!("Ffb_h_Eff_Cond"), cs!("Ffb_h_Eff_Condition")]));
                vjoy.ffb_fns.eff_envlp =
                    std::mem::transmute(get_any(&[cs!("Ffb_h_Eff_Envlp"), cs!("Ffb_h_Eff_Envelope")]));
                vjoy.ffb_fns.eff_op = std::mem::transmute(get_fn(b"Ffb_h_EffOp\0"));
                vjoy.ffb_fns.dev_ctrl = std::mem::transmute(get_fn(b"Ffb_h_DevCtrl\0"));
                vjoy.ffb_fns.dev_gain = std::mem::transmute(get_fn(b"Ffb_h_DevGain\0"));

                let f = vjoy.ffb_fns;
                if f.packet_type.is_none()
                    || f.ebi.is_none()
                    || f.eff_report.is_none()
                    || f.eff_op.is_none()
                    || f.dev_ctrl.is_none()
                    || f.dev_gain.is_none()
                {
                    add_log(
                        "vJoyInterface.dll is missing some Ffb_h_* helpers -> basic FFB fallback mode",
                        Color32::from_rgb(255, 171, 0),
                    );
                }

                // لو أي helper للـ effects الجديدة ناقص، نقول للمستخدم بالاسم
                let mut missing: Vec<&str> = Vec::new();
                if f.eff_period.is_none() {
                    missing.push("Periodic (bumps/vibration)");
                }
                if f.eff_cond.is_none() {
                    missing.push("Condition (spring/damper/friction)");
                }
                if f.eff_ramp.is_none() {
                    missing.push("Ramp");
                }
                if f.eff_envlp.is_none() {
                    missing.push("Envelope");
                }
                if missing.is_empty() {
                    add_log("All FFB helpers found (Constant, Ramp, Periodic, Condition, Envelope)", Color32::from_rgb(0, 229, 255));
                } else {
                    add_log(
                        format!("vJoyInterface.dll has no helper for: {}", missing.join(", ")),
                        Color32::from_rgb(255, 171, 0),
                    );
                }

                if let Some(f_en) = fn_enabled {
                    if f_en() != 0 {
                        vjoy.enabled = true;
                        vjoy.try_acquire();
                    }
                }
            }

            vjoy
        }

        pub fn try_acquire(&mut self) -> bool {
            if !self.enabled || self.acquired {
                return self.acquired;
            }
            if let (Some(f_stat), Some(f_acq)) = (self.p_status, self.p_acquire) {
                unsafe {
                    let stat = f_stat(self.dev_id);
                    if (stat == 0 || stat == 1) && f_acq(self.dev_id) != 0 {
                        self.acquired = true;
                        add_log(format!("vJoy Device #{} acquired successfully", self.dev_id), Color32::from_rgb(0, 230, 118));
                        if let Some(f_start) = self.p_ffb_start {
                            f_start(self.dev_id);
                            add_log("vJoy FFB queue started", Color32::from_rgb(0, 229, 255));
                        }
                    }
                }
            }
            self.acquired
        }

        pub fn register_ffb(&self, p_ctx: *mut FfbContext) {
            if let Some(f_reg) = self.p_ffb_reg {
                unsafe {
                    f_reg(ffb_callback, p_ctx as *mut c_void);
                    add_log("Registered DirectInput FFB callback", Color32::from_rgb(0, 229, 255));
                }
            }
        }

        pub fn set_axis(&self, axis: UINT, value: i32) {
            if let Some(f) = self.p_set_axis {
                if self.acquired {
                    unsafe { f(value, self.dev_id, axis); }
                }
            }
        }

        pub fn set_button(&self, btn: u8, pressed: bool) {
            if let Some(f) = self.p_set_btn {
                if self.acquired {
                    unsafe { f(if pressed { 1 } else { 0 }, self.dev_id, btn); }
                }
            }
        }

        pub fn set_pov(&self, value: i32) {
            if let Some(f) = self.p_set_disc_pov {
                if self.acquired {
                    unsafe { f(value, self.dev_id, 0); }
                }
            }
        }
    }

    impl Drop for VJoy {
        fn drop(&mut self) {
            if self.acquired {
                if let Some(f) = self.p_relinquish {
                    unsafe { f(self.dev_id); }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct PedalCalib {
    pub rest_baseline: u8,
    pub max_pressed: u8,
    pub initialized: bool,
}

impl Default for PedalCalib {
    fn default() -> Self {
        Self {
            rest_baseline: 90,
            max_pressed: 255,
            initialized: false,
        }
    }
}

impl PedalCalib {
    fn process(&mut self, raw: u8, is_connected: bool) -> f32 {
        if !is_connected {
            return 0.0;
        }

        if !self.initialized {
            if raw < 135 {
                self.rest_baseline = raw;
                self.initialized = true;
                add_log(format!("Auto-Calibrated Zero baseline to: {}", raw), Color32::YELLOW);
            } else {
                self.rest_baseline = 90;
                self.initialized = true;
                add_log("Pedal pressed on startup; defaulted zero to 90", Color32::from_rgb(255, 171, 0));
            }
        }

        let threshold = self.rest_baseline.saturating_add(16);
        if raw <= threshold {
            return 0.0;
        }

        if raw > self.max_pressed {
            self.max_pressed = raw;
        }

        let range = (self.max_pressed.saturating_sub(threshold)) as f32;
        if range < 12.0 {
            return 0.0;
        }

        ((raw - threshold) as f32 / range).clamp(0.0, 1.0)
    }

    fn reset_to_current(&mut self, current_raw: u8, name: &str) {
        if current_raw < 145 {
            self.rest_baseline = current_raw;
            self.initialized = true;
            add_log(format!("↺ Zero reset for {} -> {}", name, current_raw), Color32::from_rgb(0, 230, 118));
        } else {
            add_log(format!("⚠ Cannot reset {} while pedal is pressed (raw={})", name, current_raw), Color32::from_rgb(255, 59, 48));
        }
    }
}

#[derive(Default, Clone)]
struct WheelState {
    connected: bool,
    vjoy_connected: bool,
    steering: i16,
    brake_raw: u8,
    throttle_raw: u8,
    brake_zero: u8,
    throttle_zero: u8,
    brake_pct: f32,
    throttle_pct: f32,
    buttons: u16,
    game_ffb_force: f32,
    /// 0 = لسه ما اتفحصش (بنفترض الوضع الطبيعي)، 1 = قوة موجبة بتلف ناحية +steering، -1 = العكس
    polarity: i8,
}

#[allow(dead_code)]
enum FfbCommand {
    MotorPower(bool),
    Autocenter(u8),
    ConstantForce(i8),
    Rumble(u8, u8),
    ResetPedals,
    Gain(f32),
    Invert(bool),
    CenterWheel,
    SweepRight,
    SweepLeft,
    SweepEnd,
    DetectPolarity,
    Shutdown,
}

/// باكت القوة الثابتة (force: -127..127)
fn force_pkt(force: i8) -> [u8; 12] {
    [0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, force as u8, 0, 0, 0, 0, 0]
}

/// باكت الاهتزاز (Rumble)
fn rumble_pkt(a: u8, b: u8) -> [u8; 12] {
    [0x00, 0x01, 0x0f, 0xc0, 0x00, a, b, 0, 0, 0, 0, 0]
}

fn main() -> eframe::Result<()> {
    let state = Arc::new(Mutex::new(WheelState::default()));
    let (tx_cmd, rx_cmd) = channel::<FfbCommand>();
    let tx_exit = tx_cmd.clone();

    {
        let state = state.clone();
        thread::spawn(move || {
            usb_and_vjoy_worker(state, rx_cmd);
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Xbox 360 Wheel Studio & FFB Feeder - Pro Edition")
            .with_inner_size([1100.0, 860.0])
            .with_min_inner_size([900.0, 700.0])
            .with_resizable(true),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Xbox 360 Wheel Studio",
        options,
        Box::new(|_cc| Ok(Box::new(DashboardApp::new(state, tx_cmd)))),
    );

    // مهم: القوة عند الدريكسيون "مستمرة للأبد" لحد ما نوقفها، فلازم نصفّرها قبل ما البرنامج يقفل
    let _ = tx_exit.send(FfbCommand::Shutdown);
    thread::sleep(Duration::from_millis(250));
    result
}

struct DashboardApp {
    state: Arc<Mutex<WheelState>>,
    tx: Sender<FfbCommand>,
    motor_on: bool,
    autocenter_val: u8,
    test_force: f32,
    rumble_a: u8,
    rumble_b: u8,
    ffb_gain: f32,
    invert_ffb: bool,
    ffb_debug: bool,
    tune: Tuning,
    rate_t: Instant,
    rate_cnt: u32,
    out_rate: u32,
    sweep_busy: Arc<AtomicBool>,
}

impl DashboardApp {
    fn new(state: Arc<Mutex<WheelState>>, tx: Sender<FfbCommand>) -> Self {
        FFB_DEBUG.store(true, Ordering::Relaxed);
        Self {
            state,
            tx,
            motor_on: true,
            autocenter_val: 31,
            test_force: 0.0,
            rumble_a: 0,
            rumble_b: 0,
            ffb_gain: 1.0,
            invert_ffb: false,
            ffb_debug: true,
            tune: Tuning::DEFAULT,
            rate_t: Instant::now(),
            rate_cnt: 0,
            out_rate: 0,
            sweep_busy: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for DashboardApp {
    fn drop(&mut self) {
        let _ = self.tx.send(FfbCommand::Shutdown);
        thread::sleep(Duration::from_millis(200));
    }
}

impl eframe::App for DashboardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint();

        ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, Color32::from_rgb(18, 20, 26));

        let current = self.state.lock().unwrap().clone();

        // معدل الإرسال الفعلي للدريكسيون (pkt/s)
        if self.rate_t.elapsed() >= Duration::from_secs(1) {
            let c = SENT_PKTS.load(Ordering::Relaxed);
            self.out_rate = c.wrapping_sub(self.rate_cnt);
            self.rate_cnt = c;
            self.rate_t = Instant::now();
        }

        ui.add_space(6.0);

        ui.horizontal(|ui| {
            // الجزء الأيسر
            ui.vertical(|ui| {
                ui.set_width(730.0);

                ui.horizontal(|ui| {
                    ui.heading(
                        egui::RichText::new("🏎️ XBOX 360 WHEEL TO VJOY")
                            .color(Color32::WHITE)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if current.vjoy_connected {
                            ui.label(egui::RichText::new("● vJoy: ACTIVE").color(Color32::from_rgb(0, 230, 118)).strong());
                        } else {
                            ui.label(egui::RichText::new("⚠ vJoy: BUSY / OFF").color(Color32::from_rgb(255, 171, 0)).strong());
                        }
                        ui.add_space(8.0);
                        if current.connected {
                            ui.label(egui::RichText::new("● WHEEL: ONLINE").color(Color32::from_rgb(0, 229, 255)).strong());
                        } else {
                            ui.label(egui::RichText::new("○ WHEEL: OFFLINE").color(Color32::GRAY).strong());
                        }
                    });
                });

                ui.separator();

                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(450.0);
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(440.0, 310.0), egui::Sense::hover());
                        draw_wheel_and_controls(ui.painter(), rect, current.steering, current.buttons);

                        let angle_deg = (current.steering as f32 / 32768.0) * 135.0;
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Angle: {:+5.1}°", angle_deg))
                                    .color(Color32::from_rgb(0, 229, 255))
                                    .size(16.0)
                                    .monospace()
                                    .strong(),
                            );
                        });

                        ui.add_space(4.0);

                        // أزرار المعايرة والتوسيط بعد تفعيل الحركة الفيزيائية
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            if ui.button(egui::RichText::new("🎯 Center Wheel").size(12.0).color(Color32::from_rgb(0, 230, 118))).on_hover_text("سحب الدريكسون للمنتصف فوراً").clicked() {
                                let _ = self.tx.send(FfbCommand::CenterWheel);
                            }

                            let sweep_busy = self.sweep_busy.load(Ordering::Relaxed);
                            if ui.add_enabled(!sweep_busy, egui::Button::new(egui::RichText::new("🔄 Lock-to-Lock Sweep Test").size(12.0).color(Color32::from_rgb(0, 229, 255))))
                                .on_hover_text("فحص حركة الدريكسون 135 يمين ثم 135 شمال ثم التوسيط").clicked() {
                                self.sweep_busy.store(true, Ordering::Relaxed);
                                let tx_clone = self.tx.clone();
                                let busy_flag = self.sweep_busy.clone();
                                thread::spawn(move || {
                                    add_log(">>> Starting Lock-to-Lock Sweep Test...", Color32::YELLOW);
                                    let _ = tx_clone.send(FfbCommand::SweepRight);
                                    thread::sleep(Duration::from_millis(900));
                                    let _ = tx_clone.send(FfbCommand::SweepLeft);
                                    thread::sleep(Duration::from_millis(1600));
                                    let _ = tx_clone.send(FfbCommand::SweepEnd);
                                    add_log("<<< Sweep Test Completed! Centered.", Color32::from_rgb(0, 230, 118));
                                    busy_flag.store(false, Ordering::Relaxed);
                                });
                            }
                        });
                    });

                    ui.add_space(8.0);

                    // الدواسات
                    ui.vertical(|ui| {
                        ui.set_width(240.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("PEDALS").color(Color32::GRAY).size(13.0));
                            if ui.button(egui::RichText::new("↺ Reset Zero").size(11.0)).on_hover_text("اضغط لتثبيت الصفر وأنت سايب الدواسات تماماً").clicked() {
                                let _ = self.tx.send(FfbCommand::ResetPedals);
                            }
                        });
                        ui.add_space(6.0);

                        ui.horizontal(|ui| {
                            draw_pedal(ui, "BRAKE", current.brake_pct, current.brake_raw, current.brake_zero, Color32::from_rgb(255, 59, 48));
                            ui.add_space(20.0);
                            draw_pedal(ui, "THROTTLE", current.throttle_pct, current.throttle_raw, current.throttle_zero, Color32::from_rgb(52, 199, 89));
                        });
                    });
                });

                ui.add_space(6.0);
                ui.separator();

                // لوحة صدمات اللعبة والـ FFB
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("🎮 GAME LIVE FORCE FEEDBACK").color(Color32::from_rgb(0, 229, 255)).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let motor_label = if self.motor_on { "MOTOR: ON" } else { "MOTOR: OFF" };
                            let motor_color = if self.motor_on { Color32::from_rgb(0, 230, 118) } else { Color32::GRAY };
                            if ui.button(egui::RichText::new(motor_label).color(motor_color).strong()).clicked() {
                                self.motor_on = !self.motor_on;
                                let _ = self.tx.send(FfbCommand::MotorPower(self.motor_on));
                            }
                        });
                    });

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label("Live Output:");
                        draw_ffb_meter(ui, current.game_ffb_force);
                        ui.label(format!("{:+4.0}%", current.game_ffb_force * 100.0));
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!("OUT {} pkt/s", self.out_rate))
                                .color(Color32::GRAY)
                                .monospace()
                                .size(11.0),
                        );
                    });

                    // الـ Effects اللي اللعبة استخدمتها فعلاً (أخضر = اللعبة بعتته)
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Game effects:").color(Color32::GRAY).size(11.0))
                            .on_hover_text("لو SINE/SQUARE/TRIANGLE فاضلين رمادي وانت بتعدي على مطبات: يبقى اللعبة مش بتبعتهم -> افتح vJoyConf وفعّل كل الـ Effects");
                        let names: [(u32, &str); 11] = [
                            (ET_CONST, "CONST"),
                            (ET_RAMP, "RAMP"),
                            (ET_SQR, "SQUARE"),
                            (ET_SINE, "SINE"),
                            (ET_TRNGL, "TRIANGLE"),
                            (ET_STUP, "SAW+"),
                            (ET_STDN, "SAW-"),
                            (ET_SPRNG, "SPRING"),
                            (ET_DMPR, "DAMPER"),
                            (ET_FRCTN, "FRICTION"),
                            (ET_INRT, "INERTIA"),
                        ];
                        for (t, nm) in names {
                            let seen = EFFECT_SEEN[t as usize].load(Ordering::Relaxed) > 0;
                            let c = if seen { Color32::from_rgb(0, 230, 118) } else { Color32::from_rgb(90, 95, 105) };
                            ui.label(egui::RichText::new(nm).color(c).monospace().size(11.0).strong());
                        }
                    });

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if ui.add(egui::Slider::new(&mut self.ffb_gain, 0.0..=2.0).text("Master Strength")).changed() {
                            let _ = self.tx.send(FfbCommand::Gain(self.ffb_gain));
                        }
                        ui.add_space(8.0);
                        if ui.checkbox(&mut self.invert_ffb, "Invert Direction").changed() {
                            let _ = self.tx.send(FfbCommand::Invert(self.invert_ffb));
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.add(egui::Slider::new(&mut self.tune.vib_gain, 0.0..=4.0).text("Bump / Vibration"))
                            .on_hover_text("تكبير الاهتزاز (المطبات، الكيربات، الأرض الخشنة). ارفعه لو حاسس إن المطبات خفيفة");
                        ui.add_space(8.0);
                        ui.add(egui::Slider::new(&mut self.tune.detail, 0.0..=3.0).text("Impact Detail"))
                            .on_hover_text("بيكبّر بداية الخبطات (نزلة من فوق مطب، صدمة). 0 = بدون");
                    });

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.tune.cond_on, "Spring / Damper / Friction")
                            .on_hover_text("ثقل الدريكسيون ورجوعه للمنتصف زي ما اللعبة عايزة");
                        ui.add_enabled(
                            self.tune.cond_on,
                            egui::Slider::new(&mut self.tune.cond_gain, 0.0..=1.5).text("Strength"),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Autocenter:");
                        if ui.add(egui::Slider::new(&mut self.autocenter_val, 0..=31).text("Steps")).changed() {
                            let _ = self.tx.send(FfbCommand::Autocenter(self.autocenter_val));
                        }
                        ui.add_space(8.0);
                        ui.label("Manual Push:");
                        let slider = ui.add(egui::Slider::new(&mut self.test_force, -100.0..=100.0).text("%"));
                        if slider.changed() {
                            let force_byte = (self.test_force * 1.27) as i8;
                            let _ = self.tx.send(FfbCommand::ConstantForce(force_byte));
                        }
                        if ui.button("Stop").clicked() {
                            self.test_force = 0.0;
                            let _ = self.tx.send(FfbCommand::ConstantForce(0));
                        }
                    });

                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut self.ffb_debug, "FFB Debug Log (shows what the game sends)").changed() {
                            FFB_DEBUG.store(self.ffb_debug, Ordering::Relaxed);
                        }
                    });

                    egui::CollapsingHeader::new("⚙ Advanced / Experimental")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add(egui::Slider::new(&mut self.tune.send_ms, 4..=20).text("Min send interval").suffix(" ms"))
                                    .on_hover_text("أقل فاصل بين باكتين للدريكسيون. قلله لو الدونجل يستحمل (شوف OUT pkt/s)، زوّده لو فيه أخطاء إرسال في اللوج");
                            });

                            ui.horizontal(|ui| {
                                if ui.button("🧭 Detect Motor Direction")
                                    .on_hover_text("سيب الدريكسيون بإيدك تماماً! هيدفعه ~0.3 ثانية ويقيس الاتجاه، عشان الـ Spring/Damper يشتغلوا صح")
                                    .clicked()
                                {
                                    let _ = self.tx.send(FfbCommand::DetectPolarity);
                                }
                                let pol_txt = match current.polarity {
                                    1 => "+force → +steering (detected)",
                                    -1 => "+force → -steering (detected, auto-compensated)",
                                    _ => "not tested (assuming +force → +steering)",
                                };
                                ui.label(egui::RichText::new(pol_txt).color(Color32::GRAY).size(11.0));
                            });

                            ui.separator();
                            ui.label(egui::RichText::new("Rumble lab (لو الدريكسيون فيه موتورات اهتزاز)").color(Color32::GRAY).size(11.0));
                            ui.horizontal(|ui| {
                                let mut changed = false;
                                changed |= ui.add(egui::Slider::new(&mut self.rumble_a, 0..=255).text("Motor A")).changed();
                                changed |= ui.add(egui::Slider::new(&mut self.rumble_b, 0..=255).text("Motor B")).changed();
                                if changed {
                                    let _ = self.tx.send(FfbCommand::Rumble(self.rumble_a, self.rumble_b));
                                }
                                if ui.button("Stop").clicked() {
                                    self.rumble_a = 0;
                                    self.rumble_b = 0;
                                    let _ = self.tx.send(FfbCommand::Rumble(0, 0));
                                }
                            });
                            ui.checkbox(&mut self.tune.rumble_layer, "Drive rumble from game vibration (experimental)")
                                .on_hover_text("بيبعت اهتزاز الأرض كمان كأوامر Rumble. جرّب الـ lab فوق الأول وشوف الدريكسيون بيستجيب ولا لأ");
                        });
                });
            });

            ui.add_space(8.0);

            // الجزء الأيمن: الـ Log
            ui.vertical(|ui| {
                ui.set_width(ui.available_width().max(280.0));

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("📜 LIVE DIAGNOSTIC LOG").color(Color32::WHITE).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("🗑 Clear").clicked() {
                            LOG_ENTRIES.lock().unwrap().clear();
                        }
                        if ui.button("📋 Copy").clicked() {
                            let text: String = LOG_ENTRIES.lock().unwrap().iter().map(|e| format!("[{:06.2}s] {}\n", e.time_s, e.text)).collect();
                            ui.ctx().copy_text(text);
                        }
                    });
                });

                ui.separator();

                let avail_h = ui.available_height() - 10.0;
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), avail_h), egui::Sense::hover());
                ui.painter().rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(12, 14, 18));
                ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, Color32::from_rgb(40, 44, 52)), StrokeKind::Inside);

                let logs = LOG_ENTRIES.lock().unwrap().clone();
                let mut child_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(6.0)));

                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(&mut child_ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for entry in logs {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(format!("[{:05.1}s]", entry.time_s)).color(Color32::DARK_GRAY).size(10.5).monospace());
                                ui.label(egui::RichText::new(&entry.text).color(entry.color).size(11.0).monospace());
                            });
                        }
                    });
            });
        });

        // نبعت قيم الـ sliders للـ worker
        *TUNE.lock().unwrap() = self.tune;
    }
}

fn draw_ffb_meter(ui: &mut egui::Ui, force: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(260.0, 16.0), egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, CornerRadius::same(4), Color32::from_rgb(28, 32, 40));
    painter.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, Color32::from_rgb(50, 55, 65)), StrokeKind::Inside);

    let mid_x = rect.center().x;
    painter.line_segment([Pos2::new(mid_x, rect.top()), Pos2::new(mid_x, rect.bottom())], Stroke::new(1.5, Color32::GRAY));

    let f = force.clamp(-1.0, 1.0);
    let bar_width = (rect.width() / 2.0) * f.abs();
    let bar_rect = if f >= 0.0 {
        Rect::from_min_size(Pos2::new(mid_x, rect.top() + 2.0), Vec2::new(bar_width, rect.height() - 4.0))
    } else {
        Rect::from_min_size(Pos2::new(mid_x - bar_width, rect.top() + 2.0), Vec2::new(bar_width, rect.height() - 4.0))
    };

    let bar_color = if f.abs() > 0.8 {
        Color32::from_rgb(255, 59, 48)
    } else {
        Color32::from_rgb(0, 229, 255)
    };
    painter.rect_filled(bar_rect, CornerRadius::same(2), bar_color);
}

fn draw_wheel_and_controls(painter: &egui::Painter, rect: Rect, steering: i16, buttons: u16) {
    let center = rect.center() + Vec2::new(0.0, -10.0);
    let radius = 95.0;
    let angle = (steering as f32 / 32768.0) * (135.0 * PI / 180.0);

    let left_paddle_color = if buttons & btn::LB != 0 { Color32::from_rgb(0, 229, 255) } else { Color32::from_rgb(45, 50, 60) };
    let right_paddle_color = if buttons & btn::RB != 0 { Color32::from_rgb(0, 229, 255) } else { Color32::from_rgb(45, 50, 60) };

    painter.rect_filled(
        Rect::from_min_size(center + Vec2::new(-155.0, -42.0), Vec2::new(24.0, 84.0)),
        CornerRadius::same(6),
        left_paddle_color,
    );
    painter.text(center + Vec2::new(-143.0, 0.0), egui::Align2::CENTER_CENTER, "LB", egui::FontId::proportional(11.0), Color32::WHITE);

    painter.rect_filled(
        Rect::from_min_size(center + Vec2::new(131.0, -42.0), Vec2::new(24.0, 84.0)),
        CornerRadius::same(6),
        right_paddle_color,
    );
    painter.text(center + Vec2::new(143.0, 0.0), egui::Align2::CENTER_CENTER, "RB", egui::FontId::proportional(11.0), Color32::WHITE);

    painter.circle_stroke(center, radius, Stroke::new(17.0, Color32::from_rgb(32, 35, 42)));
    painter.circle_stroke(center, radius - 8.5, Stroke::new(1.8, Color32::from_rgb(60, 65, 75)));

    let mark_angle = angle - PI / 2.0;
    let mark_pos = center + Vec2::new(mark_angle.cos(), mark_angle.sin()) * radius;
    painter.circle_filled(mark_pos, 6.5, Color32::from_rgb(255, 45, 85));

    for spoke_offset in [PI, 0.0, PI / 2.0] {
        let a = angle + spoke_offset;
        let spoke_end = center + Vec2::new(a.cos(), a.sin()) * (radius - 12.0);
        painter.line_segment([center, spoke_end], Stroke::new(11.0, Color32::from_rgb(50, 55, 65)));
    }

    painter.circle_filled(center, 42.0, Color32::from_rgb(25, 28, 34));
    let guide_color = if buttons & btn::GUIDE != 0 { Color32::from_rgb(0, 230, 118) } else { Color32::from_rgb(50, 55, 65) };
    painter.circle_filled(center, 17.0, guide_color);
    painter.text(center, egui::Align2::CENTER_CENTER, "X", egui::FontId::proportional(15.0), Color32::WHITE);

    let btn_center = center + Vec2::new(60.0, 0.0);
    draw_button(painter, btn_center + Vec2::new(0.0, 19.0), "A", buttons & btn::A != 0, Color32::from_rgb(0, 200, 83));
    draw_button(painter, btn_center + Vec2::new(19.0, 0.0), "B", buttons & btn::B != 0, Color32::from_rgb(244, 67, 54));
    draw_button(painter, btn_center + Vec2::new(-19.0, 0.0), "X", buttons & btn::X != 0, Color32::from_rgb(33, 150, 243));
    draw_button(painter, btn_center + Vec2::new(0.0, -19.0), "Y", buttons & btn::Y != 0, Color32::from_rgb(255, 235, 59));

    let dpad_center = center + Vec2::new(-60.0, 0.0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(0.0, -17.0), "▲", buttons & btn::DPAD_UP != 0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(0.0, 17.0), "▼", buttons & btn::DPAD_DOWN != 0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(-17.0, 0.0), "◀", buttons & btn::DPAD_LEFT != 0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(17.0, 0.0), "▶", buttons & btn::DPAD_RIGHT != 0);

    draw_button(painter, center + Vec2::new(-23.0, 30.0), "<", buttons & btn::BACK != 0, Color32::LIGHT_GRAY);
    draw_button(painter, center + Vec2::new(23.0, 30.0), ">", buttons & btn::START != 0, Color32::LIGHT_GRAY);
}

fn draw_button(painter: &egui::Painter, pos: Pos2, text: &str, active: bool, color: Color32) {
    let fill = if active { color } else { Color32::from_rgb(38, 42, 50) };
    let text_color = if active { Color32::BLACK } else { color };
    painter.circle_filled(pos, 8.0, fill);
    painter.text(pos, egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(10.5), text_color);
}

fn draw_dpad_arrow(painter: &egui::Painter, pos: Pos2, text: &str, active: bool) {
    let fill = if active { Color32::from_rgb(0, 229, 255) } else { Color32::from_rgb(38, 42, 50) };
    let text_color = if active { Color32::BLACK } else { Color32::GRAY };
    painter.circle_filled(pos, 7.0, fill);
    painter.text(pos, egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(9.5), text_color);
}

fn draw_pedal(ui: &mut egui::Ui, name: &str, pct: f32, raw: u8, zero: u8, color: Color32) {
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(52.0, 230.0), egui::Sense::hover());
        let painter = ui.painter();

        painter.rect(
            rect,
            CornerRadius::same(8),
            Color32::from_rgb(28, 32, 40),
            Stroke::new(1.0, Color32::from_rgb(50, 55, 65)),
            StrokeKind::Inside,
        );

        let fill_height = rect.height() * pct.clamp(0.0, 1.0);
        let fill_rect = Rect::from_min_max(
            Pos2::new(rect.left(), rect.bottom() - fill_height),
            rect.right_bottom(),
        );
        painter.rect_filled(fill_rect, CornerRadius::same(8), color);

        ui.add_space(5.0);
        ui.label(egui::RichText::new(format!("{:.0}%", pct * 100.0)).color(color).strong().size(15.0));
        ui.label(egui::RichText::new(name).color(Color32::WHITE).size(11.0));
        ui.label(egui::RichText::new(format!("Raw:{} Z:{}", raw, zero)).color(Color32::DARK_GRAY).size(9.5));
    });
}

fn usb_and_vjoy_worker(state: Arc<Mutex<WheelState>>, rx_cmd: Receiver<FfbCommand>) {
    let mut vjoy_dev = vjoy::VJoy::load(1);
    let mut brake_calib = PedalCalib::default();
    let mut throttle_calib = PedalCalib::default();

    if vjoy_dev.enabled && vjoy_dev.ffb_fns.eff_const.is_some() {
        let ctx = Box::new(vjoy::FfbContext { fns: vjoy_dev.ffb_fns });
        vjoy_dev.register_ffb(Box::into_raw(ctx));
    }

    let mut user_gain: f32 = 1.0;
    let mut user_invert: bool = false;

    // --- حالة إرسال القوة للدريكسيون ---
    let mut last_sent_ffb: i8 = 0;
    let mut last_ffb_time = Instant::now();
    let mut send_fail_count: u32 = 0;
    let mut last_out_log = Instant::now();
    let mut last_out_byte: i8 = 0;
    let mut manual_force: i8 = 0;
    let mut sweep_active = false;
    let mut sweep_force: i8 = 0;
    let mut shutting_down = false;

    // --- حركة الدريكسيون (موقع + سرعة) للـ Spring / Damper / Friction ---
    let mut cur_pos: f32 = 0.0;
    let mut vel_ref_pos: f32 = 0.0;
    let mut vel_f: f32 = 0.0;
    let mut last_pos_t = Instant::now();
    let mut last_lx: i16 = 0;

    // --- اتجاه الموتور (+force بتلف ناحية +steering ولا العكس) ---
    let mut phys_sign: f32 = 1.0;
    let mut pol_test: Option<(Instant, i16)> = None;
    let mut pol_decided = false;

    // --- فلتر Impact Detail + Rumble layer ---
    let mut detail_lp: f32 = 0.0;
    let mut last_loop = Instant::now();
    let mut last_rumble_b: u8 = 0;
    let mut last_rumble_t = Instant::now();
    let mut rumble_was_on = false;

    let mut last_buttons: u16 = 0;
    let mut last_throttle_active = false;
    let mut last_brake_active = false;
    let mut last_angle_quadrant: i8 = 0;

    loop {
        if !vjoy_dev.acquired {
            vjoy_dev.try_acquire();
        }
        state.lock().unwrap().vjoy_connected = vjoy_dev.acquired;

        let Ok(devices) = nusb::list_devices().wait() else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        let Some(info) = devices.into_iter().find(|d| RECEIVER_IDS.contains(&(d.vendor_id(), d.product_id()))) else {
            state.lock().unwrap().connected = false;
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        let Ok(device) = info.open().wait() else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        let Ok(iface) = device.claim_interface(0).wait() else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        let (Ok(mut ep_in), Ok(mut ep_out)) = (
            iface.endpoint::<Interrupt, In>(0x81),
            iface.endpoint::<Interrupt, Out>(0x01),
        ) else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };

        for _ in 0..4 {
            ep_in.submit(ep_in.allocate(32));
        }

        let _ = send_pkt(&mut ep_out, &[0x08, 0x00, 0x0f, 0xc0]);
        state.lock().unwrap().connected = true;
        add_log("Xbox 360 Receiver & Wheel Linked", Color32::from_rgb(0, 229, 255));

        // الدريكسيون بيبدأ من قوة صفر بعد الاتصال
        last_sent_ffb = 0;
        last_ffb_time = Instant::now();
        send_fail_count = 0;

        let mut motor_inited = false;

        while state.lock().unwrap().connected {
            if !vjoy_dev.acquired {
                vjoy_dev.try_acquire();
            }
            state.lock().unwrap().vjoy_connected = vjoy_dev.acquired;

            while let Ok(cmd) = rx_cmd.try_recv() {
                match cmd {
                    FfbCommand::MotorPower(on) => {
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, if on { 0x04 } else { 0x02 }]);
                        add_log(if on { "Motor Power: ON" } else { "Motor Power: OFF" }, Color32::YELLOW);
                    }
                    FfbCommand::Autocenter(val) => {
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x40 | (val & 0x1f)]);
                    }
                    FfbCommand::ConstantForce(force) => {
                        // القوة اليدوية بتتبعت من نفس مسار الإرسال الموحد تحت
                        manual_force = force;
                    }
                    FfbCommand::Rumble(a, b) => {
                        let _ = send_pkt(&mut ep_out, &rumble_pkt(a, b));
                    }
                    FfbCommand::ResetPedals => {
                        let s = state.lock().unwrap();
                        let b_raw = s.brake_raw;
                        let t_raw = s.throttle_raw;
                        drop(s);
                        brake_calib.reset_to_current(b_raw, "Brake");
                        throttle_calib.reset_to_current(t_raw, "Throttle");
                    }
                    FfbCommand::Gain(g) => {
                        user_gain = g;
                        add_log(format!("FFB Gain set to {:.1}x", g), Color32::YELLOW);
                    }
                    FfbCommand::Invert(inv) => {
                        user_invert = inv;
                        add_log(format!("FFB Invert: {}", inv), Color32::YELLOW);
                    }
                    // توسيط الدريكسيون فوراً
                    FfbCommand::CenterWheel => {
                        sweep_active = false;
                        sweep_force = 0;
                        manual_force = 0;
                        pol_test = None;
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x04]); // motor on
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x20]); // start autocenter
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f]); // full strength
                        add_log("Wheel Centered by Autocenter motor", Color32::from_rgb(0, 230, 118));
                    }
                    // الفحص الشامل لليمين
                    FfbCommand::SweepRight => {
                        sweep_active = true;
                        sweep_force = 88; // لف يمين بقوة +88
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x10]); // إيقاف السبرنج مؤقتاً لإطلاق عزم الموتور
                    }
                    // الفحص الشامل لليسار
                    FfbCommand::SweepLeft => {
                        sweep_active = true;
                        sweep_force = -88; // لف شمال بقوة -88
                    }
                    // إنهاء الفحص والرجوع للمنتصف
                    FfbCommand::SweepEnd => {
                        sweep_active = false;
                        sweep_force = 0;
                        if send_pkt(&mut ep_out, &force_pkt(0)).is_ok() {
                            last_sent_ffb = 0;
                            last_ffb_time = Instant::now();
                        }
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x20]); // إعادة تشغيل السبرنج
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f]);
                    }
                    // فحص اتجاه الموتور: دفعة قصيرة وقياس الحركة، وبعدين دفعة عكسية للرجوع
                    FfbCommand::DetectPolarity => {
                        if sweep_active || pol_test.is_some() {
                            add_log("Motor direction test: busy, try again in a moment", Color32::from_rgb(255, 171, 0));
                        } else {
                            manual_force = 0;
                            pol_test = Some((Instant::now(), last_lx));
                            pol_decided = false;
                            add_log(">>> Motor direction test: keep your hands OFF the wheel", Color32::YELLOW);
                        }
                    }
                    // قفل البرنامج: لازم نفك القوة المستمرة عن الدريكسيون
                    FfbCommand::Shutdown => {
                        shutting_down = true;
                        sweep_active = false;
                        manual_force = 0;
                        pol_test = None;
                        for _ in 0..3 {
                            let _ = send_pkt(&mut ep_out, &force_pkt(0));
                        }
                        let _ = send_pkt(&mut ep_out, &rumble_pkt(0, 0));
                        last_sent_ffb = 0;
                        add_log("Shutdown: wheel force released", Color32::from_rgb(0, 230, 118));
                    }
                }
            }

            let tune = *TUNE.lock().unwrap();
            let loop_dt = last_loop.elapsed().as_secs_f32().clamp(0.0005, 0.05);
            last_loop = Instant::now();

            // ===== فحص اتجاه الموتور (لو شغال بيسيطر على القوة) =====
            let pol_force: Option<i8> = match pol_test {
                Some((t0, lx0)) => {
                    let el = t0.elapsed();
                    if el < Duration::from_millis(320) {
                        Some(70)
                    } else {
                        if !pol_decided {
                            pol_decided = true;
                            let d = last_lx as i32 - lx0 as i32;
                            if d > 1500 {
                                phys_sign = 1.0;
                                state.lock().unwrap().polarity = 1;
                                add_log("Motor direction: +force turns wheel toward +steering (normal)", Color32::from_rgb(0, 230, 118));
                            } else if d < -1500 {
                                phys_sign = -1.0;
                                state.lock().unwrap().polarity = -1;
                                add_log("Motor direction: +force turns wheel toward -steering (reversed) -> auto-compensated", Color32::from_rgb(0, 230, 118));
                            } else {
                                add_log(
                                    format!("Motor direction test inconclusive (moved {}). Release the wheel & check Motor is ON", d),
                                    Color32::from_rgb(255, 171, 0),
                                );
                            }
                        }
                        if el < Duration::from_millis(640) {
                            Some(-70)
                        } else {
                            pol_test = None;
                            Some(0)
                        }
                    }
                }
                None => None,
            };

            // ===== حساب القوة المطلوبة (Shutdown > Test > Sweep > اللعبة + Manual) =====
            let vel_eff = if last_pos_t.elapsed() > Duration::from_millis(80) { 0.0 } else { vel_f };
            let rp = RenderParams {
                pos: cur_pos,
                vel: vel_eff,
                vib_gain: tune.vib_gain,
                cond_on: tune.cond_on,
                cond_gain: tune.cond_gain,
            };
            let mix = if sweep_active || shutting_down || pol_force.is_some() { None } else { ffb_render(&rp) };

            let mut vib_level: f32 = 0.0;
            let target_byte: i8 = if shutting_down {
                0
            } else if let Some(b) = pol_force {
                b
            } else if sweep_active {
                sweep_force
            } else {
                let sign = if user_invert { 1.0 } else { -1.0 };
                let (di, phys, vib) = mix.map_or((0.0, 0.0, 0.0), |m| (m.di, m.phys, m.vib));
                vib_level = vib;

                // Impact Detail: بنكبّر بس "بداية" الخبطة (لما القوة بتزيد بنفس الاتجاه)، من غير ارتداد عند الإفلات
                let a = 1.0 - (-loop_dt / 0.06).exp();
                detail_lp += a * (di - detail_lp);
                let hp = di - detail_lp;
                let hp_onset = if hp * di > 0.0 { hp } else { 0.0 };
                let di_boosted = di + tune.detail * hp_onset;

                let game = (di_boosted * sign + phys * phys_sign) * user_gain;
                state.lock().unwrap().game_ffb_force = game.clamp(-1.0, 1.0);

                let total = game + manual_force as f32 / 127.0;
                (total.clamp(-1.0, 1.0) * 127.0).round() as i8
            };

            if motor_inited {
                // ===== طبقة الـ Rumble التجريبية =====
                if tune.rumble_layer {
                    if last_rumble_t.elapsed() >= Duration::from_millis(30) {
                        let b = ((vib_level * tune.vib_gain).clamp(0.0, 1.0) * 255.0) as u8;
                        if b.abs_diff(last_rumble_b) >= 6 || ((b == 0) != (last_rumble_b == 0)) {
                            let _ = send_pkt(&mut ep_out, &rumble_pkt(b, b));
                            last_rumble_b = b;
                        }
                        last_rumble_t = Instant::now();
                    }
                    rumble_was_on = true;
                } else if rumble_was_on {
                    let _ = send_pkt(&mut ep_out, &rumble_pkt(0, 0));
                    last_rumble_b = 0;
                    rumble_was_on = false;
                }

                // ===== الإرسال: بنحدّث last_sent بس لو الإرسال نجح + تحديث دوري (keep-alive) =====
                // السبب: القوة عند الدريكسيون مستمرة للأبد، فلو باكت "رجّع للصفر" ضاع على الدونجل
                // اللاسلكي كان الدريكسيون يفضل مشدود لحد ما القوة تتغير.
                let since = last_ffb_time.elapsed();
                let min_gap = Duration::from_millis(tune.send_ms.max(2) as u64);
                let refresh = if target_byte != 0 { Duration::from_millis(200) } else { Duration::from_millis(1000) };
                let changed = target_byte != last_sent_ffb;

                if (changed && since >= min_gap) || since >= refresh {
                    let res = send_pkt(&mut ep_out, &force_pkt(target_byte));
                    last_ffb_time = Instant::now();
                    match res {
                        Ok(()) => {
                            last_sent_ffb = target_byte;
                            send_fail_count = 0;
                            SENT_PKTS.fetch_add(1, Ordering::Relaxed);
                            if FFB_DEBUG.load(Ordering::Relaxed)
                                && last_out_log.elapsed() >= Duration::from_millis(250)
                                && (target_byte as i16 - last_out_byte as i16).abs() >= 8
                            {
                                let steer = state.lock().unwrap().steering;
                                add_log(
                                    format!(
                                        "FFB OUT: byte={:+4} (di={:+.2} cond={:+.2} vib={:.2}) steer={:+.0}%",
                                        target_byte,
                                        mix.map_or(0.0, |m| m.di),
                                        mix.map_or(0.0, |m| m.phys),
                                        vib_level,
                                        steer as f32 / 327.68
                                    ),
                                    COL_DBG,
                                );
                                last_out_log = Instant::now();
                                last_out_byte = target_byte;
                            }
                        }
                        Err(e) => {
                            send_fail_count += 1;
                            if send_fail_count == 1 || send_fail_count % 50 == 0 {
                                add_log(
                                    format!("⚠ FFB send failed ({:?}) x{} - will retry", e, send_fail_count),
                                    Color32::from_rgb(255, 59, 48),
                                );
                            }
                        }
                    }
                }
            }

            if let Some(transfer) = ep_in.wait_next_complete(Duration::from_millis(4)) {
                if let Ok(()) = transfer.status {
                    let data = &transfer.buffer[..transfer.actual_len];
                    if data.starts_with(&[0x08, 0x00]) {
                        state.lock().unwrap().connected = false;
                        add_log("Wheel Disconnected", Color32::from_rgb(255, 59, 48));
                    } else if data.starts_with(&[0x08, 0x80]) {
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x08, 0x42]);
                    } else if data.len() >= 18 && data.starts_with(&[0x00, 0x01, 0x00, 0xf0]) {
                        let buttons = u16::from_le_bytes([data[6], data[7]]);
                        let lt = data[8];
                        let rt = data[9];
                        let lx = i16::from_le_bytes([data[10], data[11]]);

                        // موقع + سرعة الدريكسيون (بيستخدمهم الـ Spring / Damper / Friction)
                        {
                            let now = Instant::now();
                            let new_pos = lx as f32 / 32768.0;
                            let dt = now.duration_since(last_pos_t).as_secs_f32();
                            if dt >= 0.002 {
                                let raw_v = ((new_pos - vel_ref_pos) / dt).clamp(-12.0, 12.0);
                                vel_f += 0.35 * (raw_v - vel_f);
                                vel_ref_pos = new_pos;
                                last_pos_t = now;
                            }
                            cur_pos = new_pos;
                            last_lx = lx;
                        }

                        let b_pct = brake_calib.process(lt, true);
                        let t_pct = throttle_calib.process(rt, true);

                        // تسجيل ضغطات الأزرار بالاسم
                        if buttons != last_buttons {
                            for &(bit, name) in BUTTON_NAMES {
                                if (buttons & bit != 0) && (last_buttons & bit == 0) {
                                    add_log(format!("Button [{}] PRESSED", name), Color32::from_rgb(0, 230, 118));
                                } else if (buttons & bit == 0) && (last_buttons & bit != 0) {
                                    add_log(format!("Button [{}] RELEASED", name), Color32::GRAY);
                                }
                            }
                            last_buttons = buttons;
                        }

                        // تسجيل البنزين
                        let t_active = t_pct > 0.03;
                        if t_active && !last_throttle_active {
                            add_log(format!("Throttle Pressed: {:.0}%", t_pct * 100.0), Color32::from_rgb(52, 199, 89));
                            last_throttle_active = true;
                        } else if !t_active && last_throttle_active {
                            add_log("Throttle Released (0%)", Color32::GRAY);
                            last_throttle_active = false;
                        }

                        // تسجيل الفرامل
                        let b_active = b_pct > 0.03;
                        if b_active && !last_brake_active {
                            add_log(format!("Brake Pressed: {:.0}%", b_pct * 100.0), Color32::from_rgb(255, 59, 48));
                            last_brake_active = true;
                        } else if !b_active && last_brake_active {
                            add_log("Brake Released (0%)", Color32::GRAY);
                            last_brake_active = false;
                        }

                        // زوايا أقصى اليمين والشمال والمنتصف
                        let quad = if lx > 24000 { 2 } else if lx > 8000 { 1 } else if lx < -24000 { -2 } else if lx < -8000 { -1 } else { 0 };
                        if quad != last_angle_quadrant {
                            match quad {
                                2 => add_log("Steering: Right Full Lock (135°)", Color32::from_rgb(0, 229, 255)),
                                -2 => add_log("Steering: Left Full Lock (-135°)", Color32::from_rgb(0, 229, 255)),
                                0 if last_angle_quadrant.abs() >= 1 => add_log("Steering: Returned to Center", Color32::GRAY),
                                _ => {}
                            }
                            last_angle_quadrant = quad;
                        }

                        {
                            let mut s = state.lock().unwrap();
                            s.steering = lx;
                            s.brake_raw = lt;
                            s.throttle_raw = rt;
                            s.brake_zero = brake_calib.rest_baseline;
                            s.throttle_zero = throttle_calib.rest_baseline;
                            s.brake_pct = b_pct;
                            s.throttle_pct = t_pct;
                            s.buttons = buttons;
                            s.connected = true;
                        }

                        let vjoy_steering = (((lx as i32 + 32768) as f64 / 65535.0) * 32767.0 + 1.0) as i32;
                        vjoy_dev.set_axis(vjoy::HID_USAGE_X, vjoy_steering.clamp(1, 32768));

                        let vjoy_throttle = (t_pct * 32767.0 + 1.0) as i32;
                        vjoy_dev.set_axis(vjoy::HID_USAGE_Y, vjoy_throttle.clamp(1, 32768));

                        let vjoy_brake = (b_pct * 32767.0 + 1.0) as i32;
                        vjoy_dev.set_axis(vjoy::HID_USAGE_Z, vjoy_brake.clamp(1, 32768));
                        vjoy_dev.set_axis(vjoy::HID_USAGE_RZ, vjoy_brake.clamp(1, 32768));

                        vjoy_dev.set_button(1, buttons & btn::A != 0);
                        vjoy_dev.set_button(2, buttons & btn::B != 0);
                        vjoy_dev.set_button(3, buttons & btn::X != 0);
                        vjoy_dev.set_button(4, buttons & btn::Y != 0);
                        vjoy_dev.set_button(5, buttons & btn::LB != 0);
                        vjoy_dev.set_button(6, buttons & btn::RB != 0);
                        vjoy_dev.set_button(7, buttons & btn::BACK != 0);
                        vjoy_dev.set_button(8, buttons & btn::START != 0);
                        vjoy_dev.set_button(9, buttons & btn::LS != 0);
                        vjoy_dev.set_button(10, buttons & btn::RS != 0);
                        vjoy_dev.set_button(11, buttons & btn::GUIDE != 0);

                        let pov = if buttons & btn::DPAD_UP != 0 {
                            0
                        } else if buttons & btn::DPAD_RIGHT != 0 {
                            1
                        } else if buttons & btn::DPAD_DOWN != 0 {
                            2
                        } else if buttons & btn::DPAD_LEFT != 0 {
                            3
                        } else {
                            -1
                        };
                        vjoy_dev.set_pov(pov);

                        // تهيئة قناة القوة المستمرة الأبدية (Duration = 0x00)
                        if !motor_inited {
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x04]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f]);
                            // بايت رقم 4 = 0x00 يعني القوة مستمرة للأبد حتى يتم إيقافها
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x02, 0x01, 0x05, 0x01, 0x00, 0xff, 0x00, 0x01]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x03, 0x01, 0, 0, 0, 0, 0, 0]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x0b, 0x08, 0x00, 0x01, 0, 0, 0, 0]);
                            motor_inited = true;
                            last_sent_ffb = 0;
                            last_ffb_time = Instant::now();
                            add_log("FFB Engine & Motor Initialized", Color32::from_rgb(0, 230, 118));
                        }
                    }
                }
                let mut buf = transfer.buffer;
                buf.clear();
                ep_in.submit(buf);
            }
        }
    }
}

fn send_pkt(ep: &mut nusb::Endpoint<Interrupt, Out>, packet: &[u8]) -> Result<(), nusb::transfer::TransferError> {
    let mut padded = [0u8; 12];
    let len = packet.len().min(12);
    padded[..len].copy_from_slice(&packet[..len]);
    let mut buf = Buffer::new(12);
    buf.extend_from_slice(&padded);
    ep.transfer_blocking(buf, Duration::from_millis(50)).status
}
