//! USB driver for the ChiZhu "USBDISPLAY" panel (87ad:70db) used in
//! Thermalright Vision coolers.
//!
//! Protocol (from the TRCC Linux port's reverse-engineering notes):
//! - Handshake: write 64 bytes (magic 12 34 56 78, byte 56 = 1) to EP 0x01,
//!   read 1024 bytes from EP 0x81. resp[24] is the product mode (PM), non-zero
//!   when the panel is alive.
//! - Frame: 64-byte header + JPEG, written to EP 0x01, followed by a
//!   zero-length packet when the total size is a multiple of the packet size.
//! - The firmware does not latch frames: after ~2-3 s without one it falls
//!   back to the Thermalright logo, so frames must keep coming.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use nusb::transfer::{Buffer, Bulk, In, Out};
use nusb::{Endpoint, MaybeFuture};

pub const VID: u16 = 0x87ad;
pub const PID: u16 = 0x70db;
pub const HEADER_LEN: usize = 64;

const MAGIC: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x81;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// Room for any frame we produce (a 480x480 JPEG is ~30-80 KB).
const FRAME_CAPACITY: usize = 512 * 1024;

/// Product modes whose firmware wants raw RGB565 instead of JPEG.
const RGB565_PMS: [u8; 2] = [32, 50];

pub struct Panel {
    out: Endpoint<Bulk, Out>,
    /// Reused transfer buffer; on Linux this is zero-copy memory shared with usbfs.
    buf: Option<Buffer>,
    pub pm: u8,
}

impl Panel {
    pub fn open() -> Result<Self> {
        let info = nusb::list_devices()
            .wait()?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .context("cooler screen (87ad:70db) not found on USB")?;
        let device = info.open().wait().context("opening USB device")?;
        let iface = device
            .detach_and_claim_interface(0)
            .wait()
            .context("claiming USB interface (is TRCC still running? try `trcc kill`)")?;

        let mut out = iface.endpoint::<Bulk, Out>(EP_OUT)?;
        let mut inp = iface.endpoint::<Bulk, In>(EP_IN)?;

        let mut hello = [0u8; 64];
        hello[..4].copy_from_slice(&MAGIC);
        hello[56] = 1;
        out.transfer_blocking(hello.to_vec().into(), HANDSHAKE_TIMEOUT)
            .status
            .context("handshake write")?;

        let reply = inp.transfer_blocking(inp.allocate(1024), HANDSHAKE_TIMEOUT);
        reply.status.context("handshake read")?;
        let resp = &reply.buffer[..reply.actual_len];
        if resp.len() < 41 || resp[24] == 0 {
            bail!("handshake rejected (got {} bytes)", resp.len());
        }
        let pm = resp[24];
        if RGB565_PMS.contains(&pm) {
            bail!("panel PM={pm} expects RGB565 frames, which aren't supported yet");
        }

        let buf = Some(out.allocate(FRAME_CAPACITY));
        Ok(Self { out, buf, pm })
    }

    /// Sends a frame built by [`write_header`]: 64-byte header + JPEG.
    pub fn send(&mut self, frame: &[u8]) -> Result<()> {
        let mut buf = self
            .buf
            .take()
            .unwrap_or_else(|| self.out.allocate(FRAME_CAPACITY));
        buf.clear();
        if frame.len() > buf.capacity() {
            buf = Buffer::new(frame.len());
        }
        buf.extend_from_slice(frame);

        let done = self.out.transfer_blocking(buf, FRAME_TIMEOUT);
        let sent = done.actual_len;
        self.buf = Some(done.buffer);
        done.status.context("frame write")?;
        if sent != frame.len() {
            bail!("short frame write: {sent} of {} bytes", frame.len());
        }

        if frame.len().is_multiple_of(self.out.max_packet_size()) {
            self.out
                .transfer_blocking(Buffer::new(0), FRAME_TIMEOUT)
                .status
                .context("zero-length packet")?;
        }
        Ok(())
    }
}

/// Fills the first [`HEADER_LEN`] bytes of `frame` for a JPEG of the given
/// size that follows them.
pub fn write_header(frame: &mut [u8], width: u32, height: u32) {
    let payload_len = (frame.len() - HEADER_LEN) as u32;
    let header = &mut frame[..HEADER_LEN];
    header.fill(0);
    header[0..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&2u32.to_le_bytes()); // cmd 2 = JPEG picture
    header[8..12].copy_from_slice(&width.to_le_bytes());
    header[12..16].copy_from_slice(&height.to_le_bytes());
    header[56..60].copy_from_slice(&2u32.to_le_bytes());
    header[60..64].copy_from_slice(&payload_len.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_trcc_layout() {
        let mut frame = vec![0xAAu8; HEADER_LEN + 1000];
        write_header(&mut frame, 480, 480);
        let u32_at = |i: usize| u32::from_le_bytes(frame[i..i + 4].try_into().unwrap());
        assert_eq!(&frame[0..4], &MAGIC);
        assert_eq!(u32_at(4), 2);
        assert_eq!(u32_at(8), 480);
        assert_eq!(u32_at(12), 480);
        assert!(frame[16..56].iter().all(|&b| b == 0));
        assert_eq!(u32_at(56), 2);
        assert_eq!(u32_at(60), 1000);
        assert_eq!(frame[HEADER_LEN], 0xAA, "payload untouched");
    }
}
