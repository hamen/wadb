// SPDX-License-Identifier: Apache-2.0

//! The pairing pane: the QR the phone scans, and what happened after it did.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::qr::Matrix;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// QR is on screen; waiting for the phone to scan it and advertise itself.
    Waiting,
    /// The phone was found; running `adb pair`.
    Pairing(String),
    Connected(String),
    Failed(String),
}

pub struct PairView<'a> {
    pub qr: &'a Matrix,
    pub phase: &'a Phase,
    pub elapsed: u64,
    pub timeout: u64,
}

const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

pub fn render(frame: &mut Frame, area: Rect, view: &PairView, tick: u64) {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " pair a device ",
        Style::default().add_modifier(Modifier::BOLD),
    ));

    let mut lines: Vec<Line> = Vec::new();
    lines.extend(view.qr.to_lines());
    lines.push(Line::from(""));

    match view.phase {
        Phase::Waiting => {
            let spin = SPINNER[(tick as usize) % SPINNER.len()];
            lines.push(Line::from(Span::styled(
                format!(
                    "{spin} waiting for a scan  {}s left",
                    view.timeout.saturating_sub(view.elapsed)
                ),
                Style::default().fg(Color::Cyan),
            )));
            lines.push(Line::from(Span::styled(
                "Settings -> Developer options -> Wireless debugging -> Pair device with QR code",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Phase::Pairing(host) => lines.push(Line::from(Span::styled(
            format!(
                "{} pairing with {host}",
                SPINNER[(tick as usize) % SPINNER.len()]
            ),
            Style::default().fg(Color::Cyan),
        ))),
        Phase::Connected(what) => lines.push(Line::from(Span::styled(
            format!("paired and connected: {what}"),
            Style::default().fg(Color::Green),
        ))),
        Phase::Failed(why) => lines.push(Line::from(Span::styled(
            format!("failed: {why}"),
            Style::default().fg(Color::Red),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Cells the pane needs: the QR plus its borders. The matrix width already includes the
/// quiet zone, so it must not be added again.
pub fn min_size(qr: &Matrix) -> (u16, u16) {
    let w = qr.width as u16 + 2;
    // Rows below the code: a blank, the status line, and the instruction line, which wraps
    // to three at this width. Plus the block's two borders.
    let h = qr.half_block_rows().len() as u16 + 7;
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qr;

    #[test]
    fn min_size_counts_the_quiet_zone_once() {
        let m = qr::encode(b"WIFI:T:ADB;S:studio-Ab3xK9pQr2;P:7fRt2LmNz4;;").unwrap();
        let (w, h) = min_size(&m);
        // 37 already includes 4 modules of quiet zone on each side; +2 for the borders.
        assert_eq!(w, 39);
        assert_eq!(
            h, 26,
            "19 QR rows, blank, status, 3 wrapped instruction lines, 2 borders"
        );
    }
}
