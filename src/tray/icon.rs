// SPDX-License-Identifier: Apache-2.0

//! The panel icon, drawn here rather than named from the user's icon theme.
//!
//! Two reasons it is drawn. The first is that the icon must not be confusable with the panel's
//! own network indicator: an earlier version asked for `network-wireless-*-symbolic`, which are
//! the very glyphs XFCE's network plugin draws, and the author read wadb's icon as the machine's
//! Wi-Fi state. The second is that no installed icon theme ships a phone with an on/off pair.
//! elementary-xfce, Adwaita and Yaru all ship `phone-symbolic`, `phone-apple-iphone-symbolic` and
//! friends, and at 22 px they are the same rounded slab: they differ by handset model, not by
//! state. `phone-disabled`, `phone-offline` and `phone-disconnected` exist in none of them.
//!
//! So the tray sends an `IconPixmap` and leaves every icon *name* empty. A StatusNotifierItem
//! host prefers `IconName` whenever its theme resolves it, so one surviving theme name anywhere
//! would put the Wi-Fi glyph straight back on the panel.
//!
//! State is carried by shape *and* colour, not by colour alone: green against grey is close to
//! the one distinction a red-green colour-blind reader cannot make, and it is also the pair that
//! loses most contrast on a mid-tone panel. The body outline is identical in both states, so the
//! item keeps one silhouette and stays recognisable as wadb; the screen inside it is lit when a
//! phone is attached and dark when none is.

use ksni::Icon;

/// The sizes a panel may ask for. Each is drawn at its own size rather than scaled from one
/// bitmap, so 16 px is drawn as 16 px instead of being squeezed out of 48.
pub const SIZES: [u32; 5] = [16, 22, 24, 32, 48];

/// ARGB, opaque. Mid-tone on purpose: a drawn pixmap does not recolour with the panel theme the
/// way a `-symbolic` theme icon does, so white would vanish on a light panel and black on a dark
/// one.
pub const ATTACHED: u32 = 0xFF2F_A85A;
pub const DETACHED: u32 = 0xFF8A_8A8E;

/// Every size, for `Tray::icon_pixmap`. The host picks the one it wants.
pub fn pixmaps(attached: bool) -> Vec<Icon> {
    SIZES.iter().map(|&s| phone_icon(s, attached)).collect()
}

/// The geometry of the glyph at one size. Integer arithmetic throughout, so there is nothing to
/// round differently on another machine.
struct Layout {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    stroke: u32,
    inset: u32,
}

fn layout(size: u32) -> Layout {
    // At least one fully transparent row and column on every side: a panel crops and centres the
    // pixmap, and a glyph touching an edge reads as clipped.
    let pad = (size / 8).max(1);
    let h = size - 2 * pad;
    // Portrait, about 5:9, which is roughly a phone.
    let w = ((h * 5) / 9).max(3);
    Layout {
        x: (size - w) / 2,
        y: pad,
        w,
        h,
        // Thin, like a symbolic icon. 1 px up to 32, 2 px at 48.
        stroke: (size / 22).max(1),
        inset: (size / 22).max(1) + (size / 16).max(1),
    }
}

fn fill(data: &mut [u8], size: u32, colour: u32, x: u32, y: u32, w: u32, h: u32) {
    let bytes = colour.to_be_bytes();
    for row in y..(y + h).min(size) {
        for col in x..(x + w).min(size) {
            let i = ((row * size + col) * 4) as usize;
            data[i..i + 4].copy_from_slice(&bytes);
        }
    }
}

/// One phone, `size` by `size`, in ARGB32 with the bytes in network order — which is what
/// `ksni::Icon` documents, and what a little-endian mistake would silently get wrong.
pub fn phone_icon(size: u32, attached: bool) -> Icon {
    let colour = if attached { ATTACHED } else { DETACHED };
    let l = layout(size);
    let mut data = vec![0u8; (size * size * 4) as usize];

    // The body. The top and bottom bars stop short of the corners, leaving the four corner pixels
    // clear, which reads as a rounded case rather than as a box. The side bars span the full
    // height, so the silhouette's bounding box is identical in both states.
    let inner_w = l.w.saturating_sub(2 * l.stroke);
    fill(
        &mut data,
        size,
        colour,
        l.x + l.stroke,
        l.y,
        inner_w,
        l.stroke,
    );
    fill(
        &mut data,
        size,
        colour,
        l.x + l.stroke,
        l.y + l.h - l.stroke,
        inner_w,
        l.stroke,
    );
    // The sides stop short at both ends for the same reason, which the first version did not do:
    // the bars were inset but the sides still ran the full height, so they painted the very
    // corners the inset was there to clear and the case came out a sharp rectangle. Found in
    // cross-review by grok45high and opencode, both reading the comment against the code.
    let inner_h = l.h.saturating_sub(2 * l.stroke);
    fill(
        &mut data,
        size,
        colour,
        l.x,
        l.y + l.stroke,
        l.stroke,
        inner_h,
    );
    fill(
        &mut data,
        size,
        colour,
        l.x + l.w - l.stroke,
        l.y + l.stroke,
        l.stroke,
        inner_h,
    );

    // A speaker slot in the forehead. This is most of what makes the glyph read as a handset
    // rather than as a rectangle. Skipped at 16 px, where the forehead is one pixel and the slot
    // would only muddy the outline. Drawn in both states: it is part of the phone, not part of
    // what the phone is telling you.
    let forehead = l.inset.saturating_sub(l.stroke);
    if size >= 22 && forehead >= l.stroke {
        let slot_w = (l.w / 3).max(2);
        fill(
            &mut data,
            size,
            colour,
            l.x + (l.w - slot_w) / 2,
            l.y + l.stroke + (forehead - l.stroke).div_ceil(2),
            slot_w,
            l.stroke,
        );
    }

    // The screen, lit only when a phone is attached. The gap below it is twice the gap above, so
    // there is a visible chin and the handset sits the right way up.
    if attached {
        let sw = l.w.saturating_sub(2 * l.inset);
        let sh = l.h.saturating_sub(3 * l.inset);
        if sw > 0 && sh > 0 {
            fill(
                &mut data,
                size,
                colour,
                l.x + l.inset,
                l.y + l.inset,
                sw,
                sh,
            );
        }
    }

    Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(icon: &Icon, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * icon.width as u32 + x) * 4) as usize;
        icon.data[i..i + 4].try_into().unwrap()
    }

    fn opaque(icon: &Icon, x: u32, y: u32) -> bool {
        pixel(icon, x, y)[0] == 0xFF
    }

    #[test]
    fn every_size_is_the_right_length_and_fully_opaque_or_fully_clear() {
        for &size in &SIZES {
            for attached in [true, false] {
                let icon = phone_icon(size, attached);
                assert_eq!(icon.width, size as i32);
                assert_eq!(icon.height, size as i32);
                assert_eq!(icon.data.len(), (size * size * 4) as usize);
                for chunk in icon.data.as_chunks::<4>().0 {
                    assert!(
                        chunk[0] == 0 || chunk[0] == 0xFF,
                        "size {size}: alpha {} is neither clear nor opaque",
                        chunk[0]
                    );
                }
            }
        }
    }

    #[test]
    fn the_glyph_never_touches_an_edge() {
        // A panel crops and centres the pixmap; a glyph on the border reads as clipped.
        for &size in &SIZES {
            for attached in [true, false] {
                let icon = phone_icon(size, attached);
                for i in 0..size {
                    assert!(!opaque(&icon, i, 0), "size {size}: top row is drawn on");
                    assert!(
                        !opaque(&icon, i, size - 1),
                        "size {size}: bottom row is drawn on"
                    );
                    assert!(!opaque(&icon, 0, i), "size {size}: left column is drawn on");
                    assert!(
                        !opaque(&icon, size - 1, i),
                        "size {size}: right column is drawn on"
                    );
                }
            }
        }
    }

    #[test]
    fn the_two_states_share_one_outline_and_differ_inside_it() {
        for &size in &SIZES {
            let on = phone_icon(size, true);
            let off = phone_icon(size, false);

            let bounds = |icon: &Icon| {
                let (mut x0, mut y0, mut x1, mut y1) = (size, size, 0, 0);
                for y in 0..size {
                    for x in 0..size {
                        if opaque(icon, x, y) {
                            x0 = x0.min(x);
                            y0 = y0.min(y);
                            x1 = x1.max(x);
                            y1 = y1.max(y);
                        }
                    }
                }
                (x0, y0, x1, y1)
            };
            assert_eq!(
                bounds(&on),
                bounds(&off),
                "size {size}: the silhouette must not change with the state"
            );
            assert_ne!(
                on.data, off.data,
                "size {size}: the two states must be distinguishable"
            );
            // Not by colour alone: the lit screen is real pixels the dark one does not have.
            let lit = |icon: &Icon| {
                (0..size * size)
                    .filter(|i| icon.data[(*i * 4) as usize] == 0xFF)
                    .count()
            };
            assert!(
                lit(&on) > lit(&off),
                "size {size}: attached must draw more than detached, not just recolour it"
            );
        }
    }

    #[test]
    fn bytes_are_argb_in_network_order() {
        // The only assertion that catches an endian swap: every "alpha is 0 or 255" and "the two
        // states differ" check passes either way round.
        for &size in &SIZES {
            let l = layout(size);
            let on = phone_icon(size, true);
            let off = phone_icon(size, false);
            // Midway down the left wall, drawn in both states. Not a corner: those are clear.
            let (x, y) = (l.x, l.y + l.h / 2);
            assert_eq!(pixel(&on, x, y), [0xFF, 0x2F, 0xA8, 0x5A], "size {size}");
            assert_eq!(pixel(&off, x, y), [0xFF, 0x8A, 0x8A, 0x8E], "size {size}");
        }
    }

    #[test]
    fn the_four_corners_of_the_case_are_clear() {
        // What makes it read as a rounded case rather than a box. Nothing asserted this before,
        // and the first version's comment claimed the corners were clear while the side bars
        // painted them.
        for &size in &SIZES {
            for attached in [true, false] {
                let icon = phone_icon(size, attached);
                let l = layout(size);
                for (cx, cy) in [
                    (l.x, l.y),
                    (l.x + l.w - 1, l.y),
                    (l.x, l.y + l.h - 1),
                    (l.x + l.w - 1, l.y + l.h - 1),
                ] {
                    assert!(
                        !opaque(&icon, cx, cy),
                        "size {size}: corner ({cx},{cy}) is painted"
                    );
                }
            }
        }
    }

    #[test]
    fn pixmaps_offers_every_size() {
        let sizes: Vec<i32> = pixmaps(true).iter().map(|i| i.width).collect();
        assert_eq!(sizes, SIZES.iter().map(|&s| s as i32).collect::<Vec<_>>());
    }
}
