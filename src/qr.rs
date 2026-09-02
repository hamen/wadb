// SPDX-License-Identifier: Apache-2.0

//! Rendering the pairing payload as a QR code made of unicode half-blocks.

use anyhow::Result;
use qrcode::{EcLevel, QrCode};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Modules of white space around the code. Below 4 a phone camera frequently fails to
/// find the finder patterns, especially against a busy terminal background.
pub const QUIET_ZONE: usize = 4;

/// A QR code as a grid of booleans, quiet zone already included.
pub struct Matrix {
    pub width: usize,
    cells: Vec<bool>,
}

impl Matrix {
    pub fn get(&self, x: usize, y: usize) -> bool {
        if x >= self.width || y >= self.width {
            return false;
        }
        self.cells[y * self.width + x]
    }

    /// Rows of half-block glyphs. Two module rows share one terminal row, because
    /// terminal cells are about twice as tall as they are wide and a code drawn one row
    /// per module comes out stretched and unscannable.
    pub fn half_block_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        let mut y = 0;
        while y < self.width {
            let row: String = (0..self.width)
                .map(|x| match (self.get(x, y), self.get(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            rows.push(row);
            y += 2;
        }
        rows
    }

    /// Styled for a terminal: dark modules as black ink on a white field, painted
    /// explicitly rather than inherited. On a dark theme the default foreground would
    /// invert the code, and Android's scanner rejects an inverted code.
    pub fn to_lines(&self) -> Vec<Line<'static>> {
        let style = Style::default().fg(Color::Black).bg(Color::White);
        self.half_block_rows()
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style)))
            .collect()
    }
}

/// Build the matrix for a pairing payload.
///
/// The error correction level is pinned to Low on purpose: a 45-byte payload is a
/// version-3 (29x29) code at Low, but the crate's default of Medium silently promotes it
/// to version 4 (33x33), which changes the rendered width and the terminal size the pane
/// needs. Pairing codes live for seconds on a screen a phone is pointed straight at, so
/// the redundancy buys nothing.
pub fn encode(payload: &[u8]) -> Result<Matrix> {
    let code = QrCode::with_error_correction_level(payload, EcLevel::L)?;
    let inner = code.width();
    let width = inner + QUIET_ZONE * 2;
    let colors = code.to_colors();
    let mut cells = vec![false; width * width];
    for y in 0..inner {
        for x in 0..inner {
            let dark = colors[y * inner + x] == qrcode::Color::Dark;
            cells[(y + QUIET_ZONE) * width + (x + QUIET_ZONE)] = dark;
        }
    }
    Ok(Matrix { width, cells })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"WIFI:T:ADB;S:studio-Ab3xK9pQr2;P:7fRt2LmNz4;;";

    #[test]
    fn payload_is_a_version_3_code_at_low_ecc() {
        // 45 bytes is version 3 (29 modules) at Low. If this ever becomes 33 the ECC
        // default has crept back in, and every size calculation downstream is wrong.
        assert_eq!(SAMPLE.len(), 45);
        let m = encode(SAMPLE).unwrap();
        assert_eq!(
            m.width,
            29 + QUIET_ZONE * 2,
            "29 modules + quiet zone on both sides"
        );
        assert_eq!(m.width, 37);
    }

    #[test]
    fn half_block_rows_halve_the_height() {
        let m = encode(SAMPLE).unwrap();
        let rows = m.half_block_rows();
        assert_eq!(rows.len(), 19, "37 module rows over 2, rounded up");
        assert!(rows.iter().all(|r| r.chars().count() == 37));
    }

    #[test]
    fn quiet_zone_is_blank_on_every_side() {
        let m = encode(SAMPLE).unwrap();
        for i in 0..m.width {
            for q in 0..QUIET_ZONE {
                assert!(!m.get(i, q), "top row {q} must be clear");
                assert!(!m.get(i, m.width - 1 - q), "bottom row {q} must be clear");
                assert!(!m.get(q, i), "left column {q} must be clear");
                assert!(!m.get(m.width - 1 - q, i), "right column {q} must be clear");
            }
        }
    }

    #[test]
    fn finder_pattern_is_where_it_should_be() {
        let m = encode(SAMPLE).unwrap();
        // Top-left finder: a 7x7 ring starting just inside the quiet zone.
        for i in 0..7 {
            assert!(m.get(QUIET_ZONE + i, QUIET_ZONE), "finder top edge");
            assert!(m.get(QUIET_ZONE, QUIET_ZONE + i), "finder left edge");
        }
        assert!(
            !m.get(QUIET_ZONE + 1, QUIET_ZONE + 1),
            "finder inner ring is light"
        );
    }

    #[test]
    fn polarity_is_forced_black_on_white() {
        // Inheriting the terminal's colours would invert the code on a dark theme.
        let m = encode(SAMPLE).unwrap();
        let lines = m.to_lines();
        let style = lines[0].spans[0].style;
        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::White));
    }

    #[test]
    fn odd_module_count_does_not_lose_the_last_row() {
        // An odd height must still render, with the final half-row padded blank.
        let m = Matrix {
            width: 3,
            cells: vec![true, false, true, false, true, false, true, true, true],
        };
        let rows = m.half_block_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], "▀▀▀");
    }
}
