//! How a display is read with the core `GetImage`: in strips of whole rows, each
//! no larger than the server takes in one request, and how a strip's pixels
//! become RGBA. Pure.
//!
//! One `GetImage` of a whole 4K root answers 33 MB in a single reply, which a
//! server may refuse to allocate. A strip is sized from the server's own maximum
//! request length (BIG-REQUESTS' where the server has it), and never past
//! [`STRIP_BYTES_CAP`].

/// The most bytes one strip asks for, whatever the server would take.
pub const STRIP_BYTES_CAP: usize = 8 << 20;

/// The pixel layouts a 24- or 32-bit visual comes in: four bytes a pixel, in the
/// server's image byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub bits_per_pixel: u8,
    /// MSBFirst: X R G B. LSBFirst (every PC server): B G R X.
    pub msb_first: bool,
}

/// How many rows of `width` pixels one strip holds: as many as fit the smaller of
/// the server's request size and the cap, and always at least one.
pub fn strip_rows(width: u16, height: u16, max_request_bytes: usize) -> u16 {
    let row = usize::from(width.max(1)) * 4;
    let budget = max_request_bytes.min(STRIP_BYTES_CAP);
    let rows = (budget / row).max(1);
    u16::try_from(rows).unwrap_or(u16::MAX).min(height.max(1))
}

/// The strips that cover `height` rows: (first row, rows), top to bottom.
pub fn strips(height: u16, rows: u16) -> Vec<(u16, u16)> {
    let rows = rows.max(1);
    let mut out = Vec::new();
    let mut top: u16 = 0;
    while top < height {
        let take = rows.min(height - top);
        out.push((top, take));
        top += take;
    }
    out
}

/// A strip's ZPixmap bytes as RGBA, appended to `out`.
pub fn to_rgba(
    data: &[u8],
    width: usize,
    rows: usize,
    format: Format,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if format.bits_per_pixel != 32 {
        return Err(format!(
            "capture: the display stores {} bits a pixel; compux reads 32",
            format.bits_per_pixel
        ));
    }
    let expected = width * rows * 4;
    if data.len() != expected {
        return Err(format!(
            "capture: a strip of {width}x{rows} came back as {} bytes, not {expected}",
            data.len()
        ));
    }
    out.reserve(expected);
    let (pixels, _) = data.as_chunks::<4>();
    for &[first, second, third, fourth] in pixels {
        let (r, g, b) = if format.msb_first {
            (second, third, fourth)
        } else {
            (third, second, first)
        };
        out.extend_from_slice(&[r, g, b, 255]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Without BIG-REQUESTS a server takes 256 KB a request: a 1280-pixel row is
    // 5120 bytes, so a strip is 51 rows.
    #[test]
    fn a_strip_is_as_many_rows_as_the_servers_request_size_holds() {
        assert_eq!(strip_rows(1280, 1024, 262_140), 51);
        // BIG-REQUESTS lets a server take far more; the cap holds it.
        assert_eq!(
            strip_rows(1280, 4096, 16 << 20),
            (STRIP_BYTES_CAP / 5120) as u16
        );
        assert_eq!(strip_rows(1280, 1024, 16 << 20), 1024);
        assert_eq!(
            strip_rows(3840, 2160, 1 << 30),
            (STRIP_BYTES_CAP / 15360) as u16
        );
    }

    #[test]
    fn a_strip_is_never_empty_and_never_taller_than_the_display() {
        assert_eq!(strip_rows(u16::MAX, 10, 4), 1);
        assert_eq!(strip_rows(10, 3, 1 << 20), 3);
        assert_eq!(strip_rows(0, 0, 0), 1);
    }

    #[test]
    fn the_strips_cover_every_row_once_in_order() {
        assert_eq!(strips(1024, 51).len(), 21);
        assert_eq!(strips(1024, 51).last(), Some(&(1020, 4)));
        assert_eq!(strips(100, 100), vec![(0, 100)]);
        assert_eq!(strips(5, 2), vec![(0, 2), (2, 2), (4, 1)]);
        assert!(strips(0, 10).is_empty());
        let total: u32 = strips(1024, 51).iter().map(|&(_, n)| u32::from(n)).sum();
        assert_eq!(total, 1024);
    }

    #[test]
    fn a_pc_servers_bgrx_pixels_become_opaque_rgba() {
        let mut out = Vec::new();
        let lsb = Format {
            bits_per_pixel: 32,
            msb_first: false,
        };
        to_rgba(&[0x30, 0x20, 0x10, 0x00, 1, 2, 3, 9], 2, 1, lsb, &mut out).unwrap();
        assert_eq!(out, vec![0x10, 0x20, 0x30, 255, 3, 2, 1, 255]);

        let mut out = Vec::new();
        let msb = Format {
            bits_per_pixel: 32,
            msb_first: true,
        };
        to_rgba(&[0x00, 0x10, 0x20, 0x30], 1, 1, msb, &mut out).unwrap();
        assert_eq!(out, vec![0x10, 0x20, 0x30, 255]);
    }

    #[test]
    fn a_strip_of_the_wrong_size_or_depth_is_refused() {
        let mut out = Vec::new();
        let lsb = Format {
            bits_per_pixel: 32,
            msb_first: false,
        };
        assert!(to_rgba(&[0; 7], 2, 1, lsb, &mut out).is_err());
        let sixteen = Format {
            bits_per_pixel: 16,
            msb_first: false,
        };
        assert!(to_rgba(&[0; 4], 2, 1, sixteen, &mut out).is_err());
        assert!(out.is_empty());
    }
}
