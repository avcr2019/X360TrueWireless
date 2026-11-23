#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};
use nusb::transfer::{Buffer, In, Interrupt, Out};
use nusb::MaybeFuture;
use std::f32::consts::PI;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const RECEIVER_IDS: &[(u16, u16)] = &[
    (0x045e, 0x0719),
    (0x045e, 0x0291),
    (0x045e, 0x02a9),
];

// أزرار دركسون الإكس بوكس 360
mod btn {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LS: u16 = 0x0040;
    pub const RS: u16 = 0x0080;
    pub const LB: u16 = 0x0100; // الشيفتر الشمال
    pub const RB: u16 = 0x0200; // الشيفتر اليمين
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

#[derive(Default, Clone)]
struct WheelState {
    connected: bool,
    steering: i16,
    brake: u8,
    throttle: u8,
    buttons: u16,
}

enum FfbCommand {
    MotorPower(bool),
    Autocenter(u8),
    ConstantForce(i8),
    Rumble(u8, u8),
}

fn main() -> eframe::Result<()> {
    let state = Arc::new(Mutex::new(WheelState::default()));
    let (tx_cmd, rx_cmd) = channel::<FfbCommand>();

    // تشغيل خيط قراءة الـ USB في الخلفية
    {
        let state = state.clone();
        thread::spawn(move || {
            usb_worker(state, rx_cmd);
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Xbox 360 Racing Wheel Dashboard - Pro Studio")
            .with_inner_size([760.0, 680.0])
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "Xbox 360 Wheel Dashboard",
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
        }
    }
}

impl eframe::App for DashboardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint(); // تحديث مستمر 60 فريم في الثانية

        let current = self.state.lock().unwrap().clone();

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(18, 20, 24)))
            .show(ctx, |ui| {
                ui.add_space(10.0);

                // الشريط العلوي (Header)
                ui.horizontal(|ui| {
                    ui.heading(
                        egui::RichText::new("🏎️ XBOX 360 RACING WHEEL")
                            .color(Color32::WHITE)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (txt, color) = if current.connected {
                            ("● CONNECTED (SLOT 1)", Color32::from_rgb(0, 230, 118))
                        } else {
                            ("○ SEARCHING DONGLE...", Color32::from_rgb(255, 171, 0))
                        };
                        ui.label(egui::RichText::new(txt).color(color).strong());
                    });
                });

                ui.separator();
                ui.add_space(10.0);

                // الجزء الأوسط: الدريكسون والدواسات
                ui.horizontal(|ui| {
                    // 1. الدريكسون التفاعلي
                    ui.vertical(|ui| {
                        ui.set_width(450.0);
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(430.0, 320.0), egui::Sense::hover());
                        draw_wheel(ui.painter(), rect, current.steering, current.buttons);

                        // نسبة اللف رقمياً
                        let angle_deg = (current.steering as f32 / 32768.0) * 135.0;
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Steering Angle: {:+5.1}°", angle_deg))
                                    .color(Color32::from_rgb(0, 229, 255))
                                    .size(17.0)
                                    .monospace(),
                            );
                        });
                    });

                    ui.add_space(15.0);

                    // 2. شريط دواسة الفرامل والبنزين
                    ui.vertical(|ui| {
                        ui.set_width(260.0);
                        ui.label(egui::RichText::new("PEDALS TELEMETRY").color(Color32::GRAY).size(13.0));
                        ui.add_space(8.0);

                        ui.horizontal(|ui| {
                            // دواسة الفرامل
                            let brake_pct = current.brake as f32 / 255.0;
                            draw_pedal_bar(ui, "BRAKE", brake_pct, Color32::from_rgb(255, 59, 48));

                            ui.add_space(30.0);

                            // دواسة البنزين
                            let throttle_pct = current.throttle as f32 / 255.0;
                            draw_pedal_bar(ui, "THROTTLE", throttle_pct, Color32::from_rgb(52, 199, 89));
                        });
                    });
                });

                ui.add_space(15.0);
                ui.separator();
                ui.add_space(10.0);

                // لوحة اختبار الـ Force Feedback والموتور
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("⚡ FORCE FEEDBACK REAL-TIME TEST").color(Color32::WHITE).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let motor_label = if self.motor_on { "MOTOR: [ENABLED]" } else { "MOTOR: [DISABLED]" };
                            let motor_color = if self.motor_on { Color32::from_rgb(0, 230, 118) } else { Color32::GRAY };
                            if ui.button(egui::RichText::new(motor_label).color(motor_color)).clicked() {
                                self.motor_on = !self.motor_on;
                                let _ = self.tx.send(FfbCommand::MotorPower(self.motor_on));
                            }
                        });
                    });

                    ui.add_space(6.0);

                    // سلايدر رجوع الدريكسون للمنتصف
                    ui.horizontal(|ui| {
                        ui.label("Autocenter Spring Strength:");
                        if ui.add(egui::Slider::new(&mut self.autocenter_val, 0..=31).text("Steps")).changed() {
                            let _ = self.tx.send(FfbCommand::Autocenter(self.autocenter_val));
                        }
                    });

                    // سلايدر لف الدركسون لليمين واليسار بالقوة
                    ui.horizontal(|ui| {
                        ui.label("Test Force Push (Motor):");
                        let slider = ui.add(egui::Slider::new(&mut self.test_force, -100.0..=100.0).text("% Force"));
                        if slider.changed() {
                            let force_byte = (self.test_force * 1.27) as i8;
                            let _ = self.tx.send(FfbCommand::ConstantForce(force_byte));
                        }
                        if ui.button("Center / Stop Force").clicked() {
                            self.test_force = 0.0;
                            let _ = self.tx.send(FfbCommand::ConstantForce(0));
                        }
                    });

                    // اختبار الهزاز
                    ui.horizontal(|ui| {
                        ui.label("Rumble Motors:");
                        if ui.add(egui::Slider::new(&mut self.rumble_strong, 0..=255).text("Heavy")).changed()
                            || ui.add(egui::Slider::new(&mut self.rumble_weak, 0..=255).text("Light")).changed() {
                            let _ = self.tx.send(FfbCommand::Rumble(self.rumble_strong, self.rumble_weak));
                        }
                        if ui.button("Stop Rumble").clicked() {
                            self.rumble_strong = 0;
                            self.rumble_weak = 0;
                            let _ = self.tx.send(FfbCommand::Rumble(0, 0));
                        }
                    });
                });
            });
    }
}

// رسم الدريكسون مع البدالات والأزرار
fn draw_wheel(painter: &egui::Painter, rect: Rect, steering: i16, buttons: u16) {
    let center = rect.center() + Vec2::new(0.0, 10.0);
    let radius = 110.0;
    let angle = (steering as f32 / 32768.0) * (135.0 * PI
