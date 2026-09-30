use anyhow::{Result, ensure};

pub const MAX_JPEG: usize = 650_000;
pub const HANDSHAKE_HEADER: [u8; 16] = [2, 255, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];

#[derive(Debug, Clone, Copy)]
pub struct DeviceInfo {
    pub pm: u16,
    pub width: u32,
    pub height: u32,
}

pub fn parse_handshake(response: &[u8], pid: u16) -> Result<DeviceInfo> {
    ensure!(
        response.len() >= 37 && response[0] == 3 && response[1] == 255 && response[8] == 1,
        "Invalid LY handshake response"
    );
    let pm = if pid == 0x5408 {
        64 + if response[20] <= 3 {
            1
        } else {
            response[20] as u16
        }
    } else {
        50 + response[36] as u16
    };
    let (width, height) = match pm {
        65 | 66 | 192 => (1920, 480),
        68 | 128 => (1280, 480),
        69 => (1920, 440),
        63 | 64 | 114 => (1600, 720),
        _ => anyhow::bail!("Unsupported LCD profile PM={pm}"),
    };
    Ok(DeviceInfo { pm, width, height })
}

/// LY sends a terminal chunk even when JPEG length is exactly divisible by 496.
pub fn packetize(jpeg: &[u8], pid: u16) -> Result<Vec<u8>> {
    ensure!(
        !jpeg.is_empty() && jpeg.len() <= MAX_JPEG,
        "JPEG must contain 1..={MAX_JPEG} bytes"
    );
    let count = jpeg.len() / 496 + 1;
    let padded = if pid == 0x5408 {
        count.next_multiple_of(4)
    } else {
        count
    };
    let mut out = vec![0; padded * 512];
    for i in 0..count {
        let n = (jpeg.len() - i * 496).min(496);
        let packet = &mut out[i * 512..(i + 1) * 512];
        packet[0..2].copy_from_slice(&[1, 255]);
        packet[2..6].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());
        packet[6..8].copy_from_slice(&(n as u16).to_le_bytes());
        packet[8] = if pid == 0x5408 { 1 } else { 2 };
        packet[9..11].copy_from_slice(&(count as u16).to_le_bytes());
        packet[11..13].copy_from_slice(&(i as u16).to_le_bytes());
        packet[16..16 + n].copy_from_slice(&jpeg[i * 496..i * 496 + n]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunk_boundaries_and_padding() {
        for size in [1usize, 495, 496, 497, 1984, 650000] {
            let data: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            for pid in [0x5408, 0x5409] {
                let packets = packetize(&data, pid).unwrap();
                let count = size / 496 + 1;
                assert_eq!(
                    packets.len(),
                    (if pid == 0x5408 {
                        count.next_multiple_of(4)
                    } else {
                        count
                    }) * 512
                );
                let mut decoded = vec![];
                for (i, p) in packets.chunks(512).take(count).enumerate() {
                    assert_eq!(&p[0..2], &[1, 255]);
                    assert_eq!(u16::from_le_bytes([p[11], p[12]]) as usize, i);
                    let n = u16::from_le_bytes([p[6], p[7]]) as usize;
                    decoded.extend_from_slice(&p[16..16 + n]);
                }
                assert_eq!(decoded, data);
                assert!(packets[count * 512..].iter().all(|b| *b == 0));
            }
        }
        assert!(packetize(&[], 0x5408).is_err());
        assert!(packetize(&vec![0; 650001], 0x5408).is_err());
    }
    #[test]
    fn profiles_and_bad_responses() {
        let mut response = [0; 512];
        response[0] = 3;
        response[1] = 255;
        response[8] = 1;
        for (raw, w, h) in [(1, 1920, 480), (4, 1280, 480), (5, 1920, 440)] {
            response[20] = raw;
            let info = parse_handshake(&response, 0x5408).unwrap();
            assert_eq!((info.width, info.height), (w, h));
        }
        assert!(parse_handshake(&response[..36], 0x5408).is_err());
        response[0] = 0;
        assert!(parse_handshake(&response, 0x5408).is_err());
    }
}
