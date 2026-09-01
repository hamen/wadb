// SPDX-License-Identifier: Apache-2.0

//! The wireless devices table.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::adb::{Device, Transport};

fn state_style(state: &str) -> Style {
    match state {
        "device" => Style::default().fg(Color::Green),
        "offline" => Style::default().fg(Color::Yellow),
        "unauthorized" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn state_glyph(state: &str) -> &'static str {
    match state {
        "device" => "●",
        "offline" => "○",
        "unauthorized" => "▲",
        _ => "·",
    }
}

fn how(t: Transport) -> &'static str {
    match t {
        Transport::Mdns => "mdns",
        Transport::Tcp => "tcp",
        Transport::Emulator => "emu",
        Transport::Usb => "usb",
    }
}

/// Shorten an mDNS serial, which is far too long for a column but whose middle segment
/// is the phone's actual identity.
pub fn short_serial(serial: &str) -> String {
    match serial.strip_suffix("._adb-tls-connect._tcp") {
        Some(head) => head.to_string(),
        None => serial.to_string(),
    }
}

pub fn render(frame: &mut Frame, area: Rect, devices: &[Device], server_up: bool) {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " wireless devices ",
        Style::default().add_modifier(Modifier::BOLD),
    ));

    if devices.is_empty() {
        let msg = if !server_up {
            vec![
                Line::from(Span::styled(
                    "adb server is not running",
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                Line::from("press i to install the service, or s to start it"),
            ]
        } else {
            vec![
                Line::from(Span::styled(
                    "no wireless devices",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from("press p to pair a phone over QR"),
                Line::from(Span::styled(
                    "USB devices stay available to adb but are not listed here",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        };
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    }

    let rows: Vec<Row> = devices
        .iter()
        .map(|d| {
            Row::new(vec![
                Cell::from(Span::styled(state_glyph(&d.state), state_style(&d.state))),
                Cell::from(d.model.clone().unwrap_or_else(|| "-".into())),
                Cell::from(short_serial(&d.serial)),
                Cell::from(Span::styled(d.state.clone(), state_style(&d.state))),
                Cell::from(Span::styled(
                    how(d.transport),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(16),
            Constraint::Min(20),
            Constraint::Length(13),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(vec!["", "model", "serial", "state", "how"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(block);
    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_serials_lose_their_service_suffix() {
        assert_eq!(
            short_serial("adb-39061FDJH00KZR-vWTMTB._adb-tls-connect._tcp"),
            "adb-39061FDJH00KZR-vWTMTB"
        );
        assert_eq!(short_serial("192.168.1.42:37219"), "192.168.1.42:37219");
    }

    #[test]
    fn every_state_has_its_own_colour() {
        assert_eq!(state_style("device").fg, Some(Color::Green));
        assert_eq!(state_style("offline").fg, Some(Color::Yellow));
        assert_eq!(state_style("unauthorized").fg, Some(Color::Red));
    }
}
