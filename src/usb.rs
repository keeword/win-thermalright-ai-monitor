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
                    lcd.info = match lcd.handshake() {
                        Ok(info) => info,
                        Err(first) => {
                            // Reopening a WinUSB handle does not reset its pipes.
                            // Sleep or a partial frame can leave queued replies
                            // and a stalled endpoint behind on the next open.
                            lcd.handle.reset().with_context(|| {
                                format!("Resetting LCD USB after handshake failed: {first:#}")
                            })?;
                            lcd.handle
                                .clear_halt(lcd.ep_out)
                                .context("Clearing LCD output pipe")?;
                            lcd.handle
                                .clear_halt(lcd.ep_in)
                                .context("Clearing LCD input pipe")?;
                            lcd.handshake().with_context(|| {
                                format!("LCD handshake still failed after USB recovery (first error: {first:#})")
                            })?
                        }
                    };
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
            .write_bulk(self.ep_out, bytes, Duration::from_secs(2))
            .context("Writing LCD USB data")?;
        ensure!(n == bytes.len(), "Short USB transfer: {n}/{}", bytes.len());
        Ok(())
    }
    fn handshake(&self) -> Result<DeviceInfo> {
        let mut init = [0; 2048];
        init[..16].copy_from_slice(&protocol::HANDSHAKE_HEADER);
        self.write(&init).context("Sending LCD handshake")?;
        read_handshake(self.pid, |response, timeout| {
            self.handle
                .read_bulk(self.ep_in, response, timeout)
                .context("Reading LCD handshake response")
        })
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
            .read_bulk(self.ep_in, &mut ack, Duration::from_secs(1))
            .context("Waiting for LCD frame acknowledgement")?;
        ensure!(n > 0, "Empty LCD acknowledgement");
        Ok(())
    }
}

fn read_handshake(
    pid: u16,
    mut read: impl FnMut(&mut [u8], Duration) -> Result<usize>,
) -> Result<DeviceInfo> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut response = [0; 512];
    // A late frame ACK from before sleep must not be mistaken for the new
    // handshake. Bound both the duration and the number of unrelated replies.
    for _ in 0..16 {
        let timeout = deadline.saturating_duration_since(std::time::Instant::now());
        ensure!(!timeout.is_zero(), "LCD handshake response timed out");
        let n = read(&mut response, timeout)?;
        if n >= 37 && response[0] == 3 && response[1] == 255 && response[8] == 1 {
            return protocol::parse_handshake(&response[..n], pid);
        }
    }
    anyhow::bail!("LCD sent too many unrelated replies while waiting for handshake")
}

impl Drop for Lcd {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(self.interface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(profile: u8) -> [u8; 512] {
        let mut response = [0; 512];
        response[0] = 3;
        response[1] = 255;
        response[8] = 1;
        response[20] = profile;
        response
    }

    #[test]
    fn handshake_skips_late_frame_acknowledgement() {
        let mut replies = [vec![1, 255, 0, 0], response(1).to_vec()].into_iter();
        let info = read_handshake(0x5408, |buffer, timeout| {
            assert!(!timeout.is_zero() && timeout <= Duration::from_secs(1));
            let reply = replies.next().unwrap();
            buffer[..reply.len()].copy_from_slice(&reply);
            Ok(reply.len())
        })
        .unwrap();
        assert_eq!((info.pm, info.width, info.height), (65, 1920, 480));
        assert!(replies.next().is_none());
    }

    #[test]
    fn handshake_preserves_timeout_and_unsupported_profile_errors() {
        let error = read_handshake(0x5408, |_, _| Err(rusb::Error::Timeout.into())).unwrap_err();
        assert_eq!(
            error.downcast_ref::<rusb::Error>(),
            Some(&rusb::Error::Timeout)
        );
        let error = read_handshake(0x5408, |buffer, _| {
            buffer.copy_from_slice(&response(255));
            Ok(buffer.len())
        })
        .unwrap_err();
        assert!(error.to_string().contains("Unsupported LCD profile"));
    }

    #[test]
    fn handshake_bounds_unrelated_replies() {
        let error = read_handshake(0x5408, |_, _| Ok(0)).unwrap_err();
        assert!(error.to_string().contains("too many unrelated replies"));
    }
}
