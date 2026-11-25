use crate::tui::themes::{Palette, palette};
use ratatui::style::Color;

pub(super) fn mix_rgb(a: Color, b: Color, num: u16, den: u16) -> Color {
    if den == 0 {
        return a;
    }
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| -> u8 {
                ((u32::from(x) * u32::from(den - num) + u32::from(y) * u32::from(num))
                    / u32::from(den)) as u8
            };
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => a,
    }
}

pub(super) fn relative_luminance(color: Color) -> Option<f64> {
    let Color::Rgb(r, g, b) = color else {
        return None;
    };
    let linear = |channel: u8| -> f64 {
        let u = f64::from(channel) / 255.0;
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

pub(in crate::tui) fn contrast_ratio(a: Color, b: Color) -> Option<f64> {
    let la = relative_luminance(a)?;
    let lb = relative_luminance(b)?;
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// 选择与 `bg` 对比度最高的候选色，保证反色徽章在暗色与浅色主题下均清晰可读。
pub(super) fn contrast_fg(bg: Color, candidates: &[Color]) -> Color {
    candidates
        .iter()
        .copied()
        .max_by(|a, b| {
            let ra = contrast_ratio(bg, *a).unwrap_or(0.0);
            let rb = contrast_ratio(bg, *b).unwrap_or(0.0);
            ra.total_cmp(&rb)
        })
        .unwrap_or(Color::White)
}

pub(in crate::tui) fn slab_body_bg(p: &Palette) -> Color {
    mix_rgb(p.md_bg, p.tool_bg, 2, 3)
}

pub(in crate::tui) fn slab_header_bg(p: &Palette, failed: bool) -> Color {
    if failed {
        p.tool_error_bg
    } else {
        // 无论暗色或浅色主题，向文字主色微调都能拉开表头与正文的层次。
        mix_rgb(slab_body_bg(p), p.text, 1, 14)
    }
}

pub(in crate::tui) fn badge_fg(bg: Color) -> Color {
    let p = palette();
    // 优先使用与画布一致的主题色；若对比度未达 WCAG AA (4.5:1)，回退纯黑或纯白。
    for candidate in [p.panel_bg, p.md_bg] {
        if contrast_ratio(bg, candidate).is_some_and(|r| r >= 4.5) {
            return candidate;
        }
    }
    contrast_fg(bg, &[Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)])
}
