use crate::{
    config::Settings,
    protocol::{self, DeviceInfo},
};
use anyhow::{Context, Result, ensure};
use image::RgbaImage;
use rusb::{DeviceHandle, Direction, TransferType, UsbContext};
use std::time::Duration;

pub struct Lcd {
    handle: DeviceHandle<rusb::Context>,
    interface: u8,
    ep_out: u8,
    ep_in: u8,
    pid: u16,
    pub info: DeviceInfo,
}

impl Lcd {
    pub fn open() -> Result<Self> {
        let context = rusb::Context::new()?;
        for device in context.devices()?.iter() {
            let descriptor = device.device_descriptor()?;
            if descriptor.vendor_id() != 0x0416
                || ![0x5408, 0x5409].contains(&descriptor.product_id())
            {
                continue;
            }
            let handle = device
                .open()
                .context("Cannot open LCD. Close TRCC and check the WinUSB driver")?;
            let config = device
                .active_config_descriptor()
                .or_else(|_| device.config_descriptor(0))?;
            for interface in config.interfaces() {
                for alt in interface.descriptors() {
                    if alt.class_code() != 255 {
                        continue;
                    }
                    let endpoints: Vec<_> = alt
                        .endpoint_descriptors()
                        .filter(|e| e.transfer_type() == TransferType::Bulk)
                        .collect();
                    let Some(out) = endpoints.iter().find(|e| e.direction() == Direction::Out)
                    else {
                        continue;
                    };
                    let Some(input) = endpoints.iter().find(|e| e.direction() == Direction::In)
                    else {
                        continue;
                    };
                    handle.claim_interface(alt.interface_number()).context(
                        "Cannot claim LCD interface; check WinUSB and competing applications",
                    )?;
                    if alt.setting_number() != 0 {
                        handle
                            .set_alternate_setting(alt.interface_number(), alt.setting_number())?;
                    }
                    let mut lcd = Self {
                        handle,
                        interface: alt.interface_number(),
                        ep_out: out.address(),
                        ep_in: input.address(),
                        pid: descriptor.product_id(),
                        info: DeviceInfo {
                            pm: 0,
                            width: 1920,
                            height: 480,
                        },
                    };
                    let mut init = [0; 2048];
                    init[..16].copy_from_slice(&protocol::HANDSHAKE_HEADER);
                    lcd.write(&init)?;
                    let mut response = [0; 512];
                    let n =
                        lcd.handle
                            .read_bulk(lcd.ep_in, &mut response, Duration::from_secs(1))?;
                    lcd.info = protocol::parse_handshake(&response[..n], lcd.pid)?;
                    return Ok(lcd);
                }
            }
            anyhow::bail!("LCD has no vendor bulk interface");
        }
        anyhow::bail!("LCD 0416:5408/5409 not available (unplugged or missing WinUSB driver)")
    }
    fn write(&self, bytes: &[u8]) -> Result<()> {
        let n = self
            .handle
            .write_bulk(self.ep_out, bytes, Duration::from_secs(2))?;
        ensure!(n == bytes.len(), "Short USB transfer: {n}/{}", bytes.len());
        Ok(())
    }
    pub fn send(&mut self, frame: &RgbaImage, settings: &Settings) -> Result<()> {
        let mut frame = image::imageops::resize(
            frame,
            self.info.width,
            self.info.height,
            image::imageops::FilterType::Triangle,
        );
        if settings.rotate {
            frame = image::imageops::rotate180(&frame);
        }
        let factor = 1.0 + (settings.brightness.clamp(1, 10) - 1) as f32 * 0.3;
        for pixel in frame.pixels_mut() {
            for c in &mut pixel.0[..3] {
                *c = (*c as f32 * factor).min(255.0) as u8;
            }
        }
        let rgb = image::DynamicImage::ImageRgba8(frame).to_rgb8();
        let mut encoded = Vec::new();
        for quality in [90, 80, 70, 60, 50, 40, 30] {
            encoded.clear();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
                .encode_image(&rgb)?;
            if encoded.len() <= protocol::MAX_JPEG {
                break;
            }
        }
        let packets = protocol::packetize(&encoded, self.pid)?;
        for batch in packets.chunks(4096) {
            self.write(batch)?;
        }
        let mut ack = [0; 512];
        let n = self
            .handle
            .read_bulk(self.ep_in, &mut ack, Duration::from_secs(1))?;
        ensure!(n > 0, "Empty LCD acknowledgement");
        Ok(())
    }
}

impl Drop for Lcd {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(self.interface);
    }
}
