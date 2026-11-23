#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2};
use nusb::transfer::{Buffer, In, Interrupt, Out};
use nusb::MaybeFuture;
use std::ffi::c_void;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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
    if logs.len() > 300 {
        logs.remove(0);
    }
    logs.push(LogEntry {
        time_s,
        text: text.into(),
        color,
    });
}

static GAME_FFB_MAGNITUDE: AtomicI32 = AtomicI32::new(0);
static GAME_FFB_ACTIVE: AtomicBool = AtomicBool::new(false);

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

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FfbEffConstant {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub magnitude: i32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FfbEffOp {
        pub effect_block_index: u8,
        pub _pad: [u8; 3],
        pub effect_op: u32,
        pub loop_count: u8,
        pub _pad2: [u8; 3],
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
    pub type FnFfbEffConstant = unsafe extern "system" fn(*const c_void, *mut FfbEffConstant) -> u32;
    pub type FnFfbEffOp = unsafe extern "system" fn(*const c_void, *mut FfbEffOp) -> u32;

    unsafe extern "system" {
        fn LoadLibraryA(lpLibFileName: *const u8) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
    }

    pub struct FfbContext {
        pub fn_eff_const: FnFfbEffConstant,
        pub fn_eff_op: Option<FnFfbEffOp>,
    }

    pub unsafe extern "system" fn ffb_callback(p_packet: *const c_void, p_ctx: *mut c_void) {
        if p_packet.is_null() || p_ctx.is_null() {
            return;
        }
        let ctx = unsafe { &*(p_ctx as *const FfbContext) };

        if let Some(f_op) = ctx.fn_eff_op {
            let mut op = FfbEffOp::default();
            if unsafe { f_op(p_packet, &mut op) } == 0 && op.effect_op == 3 {
                GAME_FFB_MAGNITUDE.store(0, Ordering::Relaxed);
                GAME_FFB_ACTIVE.store(false, Ordering::Relaxed);
                return;
            }
        }

        let mut eff_const = FfbEffConstant::default();
        if unsafe { (ctx.fn_eff_const)(p_packet, &mut eff_const) } == 0 {
            GAME_FFB_MAGNITUDE.store(eff_const.magnitude, Ordering::Relaxed);
            GAME_FFB_ACTIVE.store(true, Ordering::Relaxed);
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
        pub fn_eff_const: Option<FnFfbEffConstant>,
        pub fn_eff_op: Option<FnFfbEffOp>,
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
                fn_eff_const: None,
                fn_eff_op: None,
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

                let fn_enabled: Option<FnVJoyEnabled> = std::mem::transmute(get_fn(b"vJoyEnabled\0"));
                vjoy.p_status = std::mem::transmute(get_fn(b"GetVJDStatus\0"));
                vjoy.p_acquire = std::mem::transmute(get_fn(b"AcquireVJD\0"));
                vjoy.p_set_axis = std::mem::transmute(get_fn(b"SetAxis\0"));
                vjoy.p_set_btn = std::mem::transmute(get_fn(b"SetBtn\0"));
                vjoy.p_set_disc_pov = std::mem::transmute(get_fn(b"SetDiscPov\0"));
                vjoy.p_relinquish = std::mem::transmute(get_fn(b"RelinquishVJD\0"));

                vjoy.p_ffb_start = std::mem::transmute(get_fn(b"FfbStart\0"));
                vjoy.p_ffb_reg = std::mem::transmute(get_fn(b"FfbRegisterGenCB\0"));
                vjoy.fn_eff_const = std::mem::transmute(get_fn(b"Ffb_h_Eff_Constant\0"));
                vjoy.fn_eff_op = std::mem::transmute(get_fn(b"Ffb_h_EffOp\0"));

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

        let threshold = self.rest_baseline.saturating_add(14);
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
}

enum FfbCommand {
    MotorPower(bool),
    Autocenter(u8),
    ConstantForce(i8),
    Rumble(u8, u8),
    ResetPedals,
    Gain(f32),
    Invert(bool),
    CenterWheel,
}

fn main() -> eframe::Result<()> {
    let state = Arc::new(Mutex::new(WheelState::default()));
    let (tx_cmd, rx_cmd) = channel::<FfbCommand>();

    {
        let state = state.clone();
        thread::spawn(move || {
            usb_and_vjoy_worker(state, rx_cmd);
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Xbox 360 Wheel Studio & FFB Feeder - Pro Edition")
            .with_inner_size([1040.0, 710.0])
            .with_min_inner_size([750.0, 520.0])
            .with_resizable(true), // ميزة تكبير وتصغير النافذة
        ..Default::default()
    };

    eframe::run_native(
        "Xbox 360 Wheel Studio",
        options,
        Box::new(|_cc| Ok(Box::new(DashboardApp::new(state, tx_cmd)))),
    )
}

struct DashboardApp {
    state: Arc<Mutex<WheelState>>,
    tx: Sender<FfbCommand>,
    motor_on: bool,
    autocenter_val: u8,
    test_force: f32,
    rumble_strong: u8,
    rumble_weak: u8,
    ffb_gain: f32,
    invert_ffb: bool,
    is_testing_sweep: bool,
}

impl DashboardApp {
    fn new(state: Arc<Mutex<WheelState>>, tx: Sender<FfbCommand>) -> Self {
        Self {
            state,
            tx,
            motor_on: true,
            autocenter_val: 31,
            test_force: 0.0,
            rumble_strong: 0,
            rumble_weak: 0,
            ffb_gain: 1.0,
            invert_ffb: false,
            is_testing_sweep: false,
        }
    }
}

impl eframe::App for DashboardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint();

        ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, Color32::from_rgb(18, 20, 26));

        let current = self.state.lock().unwrap().clone();

        ui.add_space(6.0);

        // واجهة مقسمة قابلة للتجاوب
        ui.horizontal(|ui| {
            // الجزء الأيسر: التحكم والقياسات
            ui.vertical(|ui| {
                ui.set_width(710.0);

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
                    // الدريكسون والأزرار
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

                        // أزرار المعايرة والتوسيط الذاتية الجديدة!
                        ui.horizontal(|ui| {
                            ui.add_space(20.0);
                            if ui.button(egui::RichText::new("🎯 Center Wheel").size(12.0).color(Color32::from_rgb(0, 230, 118))).on_hover_text("سحب الدريكسون للمنتصف فوراً").clicked() {
                                let _ = self.tx.send(FfbCommand::CenterWheel);
                            }

                            if ui.add_enabled(!self.is_testing_sweep, egui::Button::new(egui::RichText::new("🔄 Lock-to-Lock Sweep Test").size(12.0).color(Color32::from_rgb(0, 229, 255))))
                                .on_hover_text("فحص حركة الدريكسون 135 يمين ثم 135 شمال ثم التوسيط").clicked() {
                                self.is_testing_sweep = true;
                                let tx_clone = self.tx.clone();
                                thread::spawn(move || {
                                    add_log(">>> Starting Lock-to-Lock Sweep Test...", Color32::YELLOW);
                                    let _ = tx_clone.send(FfbCommand::ConstantForce(65));
                                    thread::sleep(Duration::from_millis(900));
                                    let _ = tx_clone.send(FfbCommand::ConstantForce(-65));
                                    thread::sleep(Duration::from_millis(1600));
                                    let _ = tx_clone.send(FfbCommand::ConstantForce(0));
                                    let _ = tx_clone.send(FfbCommand::CenterWheel);
                                    add_log("<<< Sweep Test Completed! Centered.", Color32::from_rgb(0, 230, 118));
                                });
                                self.is_testing_sweep = false;
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
                        ui.label(egui::RichText::new("🎮 GAME LIVE FORCE FEEDBACK (DiRT 2 / FORZA)").color(Color32::from_rgb(0, 229, 255)).strong());
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
                        ui.label("Live Game FFB Signal:");
                        draw_ffb_meter(ui, current.game_ffb_force);
                        ui.label(format!("{:+4.0}%", current.game_ffb_force * 100.0));
                    });

                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label("FFB Multiplier:");
                        if ui.add(egui::Slider::new(&mut self.ffb_gain, 0.0..=2.0).text("Strength")).changed() {
                            let _ = self.tx.send(FfbCommand::Gain(self.ffb_gain));
                        }
                        ui.add_space(8.0);
                        if ui.checkbox(&mut self.invert_ffb, "Invert Direction").changed() {
                            let _ = self.tx.send(FfbCommand::Invert(self.invert_ffb));
                        }
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
                });
            });

            ui.add_space(8.0);

            // الجزء الأيمن: شاشة الـ Live Log الشاملة
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

    if vjoy_dev.enabled && vjoy_dev.fn_eff_const.is_some() {
        let ctx = Box::new(vjoy::FfbContext {
            fn_eff_const: vjoy_dev.fn_eff_const.unwrap(),
            fn_eff_op: vjoy_dev.fn_eff_op,
        });
        vjoy_dev.register_ffb(Box::into_raw(ctx));
    }

    let mut user_gain: f32 = 1.0;
    let mut user_invert: bool = false;
    let mut last_sent_ffb: i8 = 0;
    let mut last_ffb_time = Instant::now();

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
            continue;
        };

        for _ in 0..4 {
            ep_in.submit(ep_in.allocate(32));
        }

        let _ = send_pkt(&mut ep_out, &[0x08, 0x00, 0x0f, 0xc0]);
        state.lock().unwrap().connected = true;
        add_log("Xbox 360 Receiver & Wheel Linked", Color32::from_rgb(0, 229, 255));

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
                        let uforce = force as u8;
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, uforce, 0, 0, 0, 0, 0]);
                    }
                    FfbCommand::Rumble(strong, weak) => {
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x00, weak, strong, 0, 0, 0, 0, 0]);
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
                    FfbCommand::CenterWheel => {
                        // إرسال أمر التوسيط الفوري للدريكسون
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0]);
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x20]);
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f]);
                        add_log("Wheel Centered by Autocenter motor", Color32::from_rgb(0, 230, 118));
                    }
                }
            }

            // استقبال وإرسال قوى DiRT 2 الحية
            if GAME_FFB_ACTIVE.load(Ordering::Relaxed) {
                let raw_mag = GAME_FFB_MAGNITUDE.load(Ordering::Relaxed);
                let sign = if user_invert { 1.0 } else { -1.0 };
                let f_norm = (raw_mag as f32 / 10000.0) * sign;
                let total_f = (f_norm * user_gain).clamp(-1.0, 1.0);
                let force_byte = (total_f * 127.0) as i8;

                state.lock().unwrap().game_ffb_force = f_norm;

                if force_byte != last_sent_ffb && last_ffb_time.elapsed() >= Duration::from_millis(15) {
                    let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, force_byte as u8, 0, 0, 0, 0, 0]);
                    last_sent_ffb = force_byte;
                    last_ffb_time = Instant::now();
                }
            } else if last_sent_ffb != 0 {
                let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0]);
                last_sent_ffb = 0;
                state.lock().unwrap().game_ffb_force = 0.0;
            }

            if let Some(transfer) = ep_in.wait_next_complete(Duration::from_millis(8)) {
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

                        let b_pct = brake_calib.process(lt, true);
                        let t_pct = throttle_calib.process(rt, true);

                        // ----------------------------------------------------
                        // تسجيل كل ضغطة زرار ودواسة بالاسم في الـ Log!
                        // ----------------------------------------------------
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

                        // تسجيل زوايا الدريكسون المهمة (المنتصف / أقصى اليمين / أقصى الشمال)
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

                        if !motor_inited {
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x04]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x02, 0x01, 0x05, 0x01, 0xff, 0xff, 0x00, 0x01]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x03, 0x01, 0, 0, 0, 0, 0, 0]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x0b, 0x08, 0x00, 0x01, 0, 0, 0, 0]);
                            motor_inited = true;
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
