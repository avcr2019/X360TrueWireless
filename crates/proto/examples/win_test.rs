use std::time::{Duration, Instant};
use nusb::transfer::{Buffer, In, Interrupt, Out};
use nusb::MaybeFuture;

const RECEIVER_IDS: &[(u16, u16)] = &[
    (0x045e, 0x0719), // الدونجل الرسمي
    (0x045e, 0x0291),
    (0x045e, 0x02a9),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Xbox 360 Wireless Wheel - Windows Test Tool ===");
    println!("Looking for Xbox 360 Wireless Receiver...");

    let info = nusb::list_devices()
        .wait()?
        .find(|d| RECEIVER_IDS.contains(&(d.vendor_id(), d.product_id())))
        .expect("لم يتم العثور على الدونجل! تأكد من توصيله وتثبيت تعريف WinUSB بواسطة Zadig");

    println!("Found receiver: {:04x}:{:04x}", info.vendor_id(), info.product_id());

    let device = info.open().wait()?;
    println!("Device opened successfully!");

    // فتح السلوت رقم 0 (اللاعب الأول)
    let slot = 0;
    let iface = device.claim_interface(slot * 2).wait()?;
    println!("Claimed USB Interface {}", slot * 2);

    let mut ep_in = iface.endpoint::<Interrupt, In>(0x81 + slot * 2)?;
    let mut ep_out = iface.endpoint::<Interrupt, Out>(0x01 + slot * 2)?;

    // ملء طابور الاستقبال
    while ep_in.pending() < 4 {
        ep_in.submit(ep_in.allocate(32));
    }

    // إرسال استعلام وجود (Presence inquiry)
    send(&mut ep_out, &[0x08, 0x00, 0x0f, 0xc0])?;
    println!("Sent presence inquiry. Turn ON your wheel (press Guide button)...");

    let mut wheel_initialized = false;
    let start = Instant::now();

    loop {
        if let Some(transfer) = ep_in.wait_next_complete(Duration::from_millis(50)) {
            if let Ok(()) = transfer.status {
                let data = &transfer.buffer[..transfer.actual_len];

                // كشف حزمة الاتصال
                if data.starts_with(&[0x08, 0x80]) {
                    println!(">>> Wheel Connected to Slot {slot}!");
                    // إرسال تشغيل إضاءة LED رقم 1
                    send(&mut ep_out, &[0x00, 0x00, 0x08, 0x42])?;
                }

                // كشف بيانات الحركة والأزرار
                if data.len() >= 18 && data.starts_with(&[0x00, 0x01, 0x00, 0xf0]) {
                    let lx = i16::from_le_bytes([data[10], data[11]]);
                    let lt = data[8]; // الفرامل
                    let rt = data[9]; // البنزين
                    let buttons = u16::from_le_bytes([data[6], data[7]]);

                    print!("\rSteering LX: {:6} | Brake (LT): {:3} | Throttle (RT): {:3} | Buttons: {:04x}", lx, lt, rt, buttons);

                    // أول ما يتم رصد الدركسون، نجرب نبضات FFB لتحريك الموتور تلقائياً
                    if !wheel_initialized {
                        println!("\n>>> Testing Force Feedback motor...");
                        // 1. تشغيل الموتور ومستوى المقاومة (Autocenter strength)
                        send(&mut ep_out, &[0x00, 0x00, 0x0b, 0x04])?; // Motor ON
                        send(&mut ep_out, &[0x00, 0x00, 0x0b, 0x7f])?; // Full gain

                        // 2. إرسال أمر قوة مستمرة تلف الدركسون لليمين
                        send(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x02, 0x01, 0x05, 0x01, 0xff, 0xff, 0x00, 0x01])?;
                        send(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x08, 0x01, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00])?;
                        send(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x03, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])?;
                        send(&mut ep_out, &[0x00, 0x01, 0x0f, 0xc0, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00])?;

                        println!(">>> FFB command sent! Feel the wheel turning right.");
                        wheel_initialized = true;
                    }
                }
            }
            let mut buf = transfer.buffer;
            buf.clear();
            ep_in.submit(buf);
        }
    }
}

fn send(ep: &mut nusb::Endpoint<Interrupt, Out>, packet: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut padded = [0u8; 12];
    let len = packet.len().min(12);
    padded[..len].copy_from_slice(&packet[..len]);

    let mut buf = Buffer::new(12);
    buf.extend_from_slice(&padded);
    ep.transfer_blocking(buf, Duration::from_millis(200)).status?;
    Ok(())
}
