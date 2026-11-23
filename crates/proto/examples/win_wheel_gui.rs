#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2};
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

    {
        let state = state.clone();
        thread::spawn(move || {
            usb_worker(state, rx_cmd);
        });
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Xbox 360 Wireless Wheel - Dashboard Studio")
            .with_inner_size([780.0, 640.0])
            .with_resizable(false),
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint();

        // خلفية داكنة
        ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, Color32::from_rgb(18, 20, 26));

        let current = self.state.lock().unwrap().clone();

        ui.add_space(10.0);

        // الشريط العلوي
        ui.horizontal(|ui| {
            ui.heading(
                egui::RichText::new("🏎️ XBOX 360 RACING WHEEL STUDIO")
                    .color(Color32::WHITE)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (txt, color) = if current.connected {
                    ("● CONNECTED (ONLINE)", Color32::from_rgb(0, 230, 118))
                } else {
                    ("○ SEARCHING DONGLE...", Color32::from_rgb(255, 171, 0))
                };
                ui.label(egui::RichText::new(txt).color(color).strong());
            });
        });

        ui.separator();
        ui.add_space(8.0);

        // منطقة العرض الرئيسية
        ui.horizontal(|ui| {
            // الدريكسون والأزرار
            ui.vertical(|ui| {
                ui.set_width(480.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(470.0, 340.0), egui::Sense::hover());
                draw_wheel_and_controls(ui.painter(), rect, current.steering, current.buttons);

                let angle_deg = (current.steering as f32 / 32768.0) * 135.0;
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Steering Angle: {:+5.1}°", angle_deg))
                            .color(Color32::from_rgb(0, 229, 255))
                            .size(18.0)
                            .monospace()
                            .strong(),
                    );
                });
            });

            ui.add_space(15.0);

            // الدواسات
            ui.vertical(|ui| {
                ui.set_width(240.0);
                ui.label(egui::RichText::new("PEDALS TELEMETRY").color(Color32::GRAY).size(13.0));
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    let brake_pct = current.brake as f32 / 255.0;
                    draw_pedal(ui, "BRAKE", brake_pct, Color32::from_rgb(255, 59, 48));

                    ui.add_space(30.0);

                    let throttle_pct = current.throttle as f32 / 255.0;
                    draw_pedal(ui, "THROTTLE", throttle_pct, Color32::from_rgb(52, 199, 89));
                });
            });
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        // لوحة تحكم الـ Force Feedback
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("⚡ REAL-TIME FORCE FEEDBACK CONTROL").color(Color32::WHITE).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let motor_label = if self.motor_on { "MOTOR: ENABLED" } else { "MOTOR: DISABLED" };
                    let motor_color = if self.motor_on { Color32::from_rgb(0, 230, 118) } else { Color32::GRAY };
                    if ui.button(egui::RichText::new(motor_label).color(motor_color).strong()).clicked() {
                        self.motor_on = !self.motor_on;
                        let _ = self.tx.send(FfbCommand::MotorPower(self.motor_on));
                    }
                });
            });

            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("Autocenter Return Strength:");
                if ui.add(egui::Slider::new(&mut self.autocenter_val, 0..=31).text("Steps")).changed() {
                    let _ = self.tx.send(FfbCommand::Autocenter(self.autocenter_val));
                }
            });

            ui.horizontal(|ui| {
                ui.label("Test Push Force (Motor):");
                let slider = ui.add(egui::Slider::new(&mut self.test_force, -100.0..=100.0).text("% Force"));
                if slider.changed() {
                    let force_byte = (self.test_force * 1.27) as i8;
                    let _ = self.tx.send(FfbCommand::ConstantForce(force_byte));
                }
                if ui.button("Center / Stop").clicked() {
                    self.test_force = 0.0;
                    let _ = self.tx.send(FfbCommand::ConstantForce(0));
                }
            });

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
    }
}

fn draw_wheel_and_controls(painter: &egui::Painter, rect: Rect, steering: i16, buttons: u16) {
    let center = rect.center() + Vec2::new(0.0, -10.0);
    let radius = 100.0;
    let angle = (steering as f32 / 32768.0) * (135.0 * PI / 180.0);

    // البدالات (Paddles LB / RB)
    let left_paddle_color = if buttons & btn::LB != 0 { Color32::from_rgb(0, 229, 255) } else { Color32::from_rgb(45, 50, 60) };
    let right_paddle_color = if buttons & btn::RB != 0 { Color32::from_rgb(0, 229, 255) } else { Color32::from_rgb(45, 50, 60) };

    painter.rect_filled(
        Rect::from_min_size(center + Vec2::new(-165.0, -45.0), Vec2::new(26.0, 90.0)),
        CornerRadius::same(6),
        left_paddle_color,
    );
    painter.text(center + Vec2::new(-152.0, 0.0), egui::Align2::CENTER_CENTER, "LB", egui::FontId::proportional(12.0), Color32::WHITE);

    painter.rect_filled(
        Rect::from_min_size(center + Vec2::new(139.0, -45.0), Vec2::new(26.0, 90.0)),
        CornerRadius::same(6),
        right_paddle_color,
    );
    painter.text(center + Vec2::new(152.0, 0.0), egui::Align2::CENTER_CENTER, "RB", egui::FontId::proportional(12.0), Color32::WHITE);

    // حلقة الدريكسون الخارجية
    painter.circle_stroke(center, radius, Stroke::new(18.0, Color32::from_rgb(32, 35, 42)));
    painter.circle_stroke(center, radius - 9.0, Stroke::new(2.0, Color32::from_rgb(60, 65, 75)));

    // خط تحديد المنتصف الأحمر
    let mark_angle = angle - PI / 2.0;
    let mark_pos = center + Vec2::new(mark_angle.cos(), mark_angle.sin()) * radius;
    painter.circle_filled(mark_pos, 7.0, Color32::from_rgb(255, 45, 85));

    // الأذرع الداخلية (Spokes)
    for spoke_offset in [PI, 0.0, PI / 2.0] {
        let a = angle + spoke_offset;
        let spoke_end = center + Vec2::new(a.cos(), a.sin()) * (radius - 12.0);
        painter.line_segment([center, spoke_end], Stroke::new(12.0, Color32::from_rgb(50, 55, 65)));
    }

    // مركز الدركسون وزر الـ Guide
    painter.circle_filled(center, 44.0, Color32::from_rgb(25, 28, 34));
    let guide_color = if buttons & btn::GUIDE != 0 { Color32::from_rgb(0, 230, 118) } else { Color32::from_rgb(50, 55, 65) };
    painter.circle_filled(center, 18.0, guide_color);
    painter.text(center, egui::Align2::CENTER_CENTER, "X", egui::FontId::proportional(16.0), Color32::WHITE);

    // أزرار A, B, X, Y
    let btn_center = center + Vec2::new(65.0, 0.0);
    draw_button(painter, btn_center + Vec2::new(0.0, 20.0), "A", buttons & btn::A != 0, Color32::from_rgb(0, 200, 83));
    draw_button(painter, btn_center + Vec2::new(20.0, 0.0), "B", buttons & btn::B != 0, Color32::from_rgb(244, 67, 54));
    draw_button(painter, btn_center + Vec2::new(-20.0, 0.0), "X", buttons & btn::X != 0, Color32::from_rgb(33, 150, 243));
    draw_button(painter, btn_center + Vec2::new(0.0, -20.0), "Y", buttons & btn::Y != 0, Color32::from_rgb(255, 235, 59));

    // الصليب D-Pad
    let dpad_center = center + Vec2::new(-65.0, 0.0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(0.0, -18.0), "▲", buttons & btn::DPAD_UP != 0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(0.0, 18.0), "▼", buttons & btn::DPAD_DOWN != 0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(-18.0, 0.0), "◀", buttons & btn::DPAD_LEFT != 0);
    draw_dpad_arrow(painter, dpad_center + Vec2::new(18.0, 0.0), "▶", buttons & btn::DPAD_RIGHT != 0);

    // Start / Back
    draw_button(painter, center + Vec2::new(-25.0, 32.0), "<", buttons & btn::BACK != 0, Color32::LIGHT_GRAY);
    draw_button(painter, center + Vec2::new(25.0, 32.0), ">", buttons & btn::START != 0, Color32::LIGHT_GRAY);
}

fn draw_button(painter: &egui::Painter, pos: Pos2, text: &str, active: bool, color: Color32) {
    let fill = if active { color } else { Color32::from_rgb(38, 42, 50) };
    let text_color = if active { Color32::BLACK } else { color };
    painter.circle_filled(pos, 8.5, fill);
    painter.text(pos, egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(11.0), text_color);
}

fn draw_dpad_arrow(painter: &egui::Painter, pos: Pos2, text: &str, active: bool) {
    let fill = if active { Color32::from_rgb(0, 229, 255) } else { Color32::from_rgb(38, 42, 50) };
    let text_color = if active { Color32::BLACK } else { Color32::GRAY };
    painter.circle_filled(pos, 7.5, fill);
    painter.text(pos, egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(10.0), text_color);
}

fn draw_pedal(ui: &mut egui::Ui, name: &str, pct: f32, color: Color32) {
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(55.0, 240.0), egui::Sense::hover());
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

        ui.add_space(6.0);
        ui.label(egui::RichText::new(format!("{:.0}%", pct * 100.0)).color(color).strong().size(16.0));
        ui.label(egui::RichText::new(name).color(Color32::WHITE).size(12.0));
    });
}

fn usb_worker(state: Arc<Mutex<WheelState>>, rx_cmd: Receiver<FfbCommand>) {
    loop {
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

        let mut motor_inited = false;

        while state.lock().unwrap().connected {
            while let Ok(cmd) = rx_cmd.try_recv() {
                match cmd {
                    FfbCommand::MotorPower(on) => {
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, if on { 0x04 } else { 0x02 }]);
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
                }
            }

            if let Some(transfer) = ep_in.wait_next_complete(Duration::from_millis(8)) {
                if let Ok(()) = transfer.status {
                    let data = &transfer.buffer[..transfer.actual_len];
                    if data.starts_with(&[0x08, 0x00]) {
                        state.lock().unwrap().connected = false;
                    } else if data.starts_with(&[0x08, 0x80]) {
                        let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x08, 0x42]);
                    } else if data.len() >= 18 && data.starts_with(&[0x00, 0x01, 0x00, 0xf0]) {
                        let buttons = u16::from_le_bytes([data[6], data[7]]);
                        let lt = data[8];
                        let rt = data[9];
                        let lx = i16::from_le_bytes([data[10], data[11]]);

                        {
                            let mut s = state.lock().unwrap();
                            s.steering = lx;
                            s.brake = lt;
                            s.throttle = rt;
                            s.buttons = buttons;
                            s.connected = true;
                        }

                        if !motor_inited {
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x04]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x02, 0x01, 0x05, 0x01, 0xff, 0xff, 0x00, 0x01]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, 0x00, 0, 0, 0, 0, 0]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x03, 0x01, 0, 0, 0, 0, 0, 0]);
                            let _ = send_pkt(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x0b, 0x08, 0x00, 0x01, 0, 0, 0, 0]);
                            motor_inited = true;
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
    ep.transfer_blocking(buf, Duration::from_millis(200)).status
}
