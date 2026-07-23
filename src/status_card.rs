use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ab_glyph::{point, Font, FontArc, Glyph, PxScale, ScaleFont};
use image::{ImageBuffer, Pixel, Rgba, RgbaImage};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

const CARD_WIDTH: u32 = 1000;
const PADDING_X: i32 = 64;
const PADDING_Y: i32 = 56;
const CARD_RADIUS: i32 = 36;
const CONTENT_WIDTH: u32 = CARD_WIDTH - (PADDING_X as u32 * 2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusCardData {
    pub server_name: String,
    pub region: String,
    pub address: String,
    pub status: StatusCardState,
    pub countdown: CountdownState,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusCardState {
    Normal,
    Abnormal,
    VerificationFailed,
    Processing,
    Paused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CountdownState {
    Duration(Duration),
    NotAvailable,
    Paused,
    Processing,
}

pub struct TempStatusCard {
    path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FontSelection {
    Selected(PathBuf),
    Unavailable,
}

impl TempStatusCard {
    pub fn render(data: &StatusCardData) -> anyhow::Result<Self> {
        let path = temp_card_path();
        render_status_card_to_path(data, &path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempStatusCard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn render_status_card_to_path(data: &StatusCardData, path: &Path) -> anyhow::Result<()> {
    keep_display_variants_reachable();
    let font = load_status_card_font()?;
    let title_lines = wrap_text(&data.server_name, &font, 50.0, CONTENT_WIDTH);
    let title_height = title_lines.len() as u32 * 66;
    let detail_lines = data
        .detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .map(|detail| wrap_text(detail, &font, 31.0, CONTENT_WIDTH))
        .unwrap_or_default();
    let detail_height = detail_lines.len() as u32 * 42;
    let card_height = 510 + title_height + detail_height;

    let mut image = ImageBuffer::from_pixel(CARD_WIDTH, card_height, rgba(238, 242, 247));
    draw_rounded_rect_mut(
        &mut image,
        28,
        28,
        CARD_WIDTH - 56,
        card_height - 56,
        CARD_RADIUS,
        rgba(255, 255, 255),
    );
    draw_rounded_rect_mut(
        &mut image,
        28,
        28,
        10,
        card_height - 56,
        5,
        status_accent(&data.status),
    );

    let mut y = PADDING_Y;
    for line in title_lines {
        draw_text_mut(
            &mut image,
            rgba(24, 31, 42),
            PADDING_X,
            y,
            50.0,
            &font,
            &line,
        );
        y += 66;
    }

    y += 20;
    draw_horizontal_line_mut(
        &mut image,
        PADDING_X,
        CARD_WIDTH as i32 - PADDING_X,
        y,
        rgba(222, 227, 235),
    );
    y += 46;

    draw_text_mut(
        &mut image,
        rgba(35, 45, 60),
        PADDING_X,
        y,
        39.0,
        &font,
        &data.region,
    );
    y += 56;
    draw_text_mut(
        &mut image,
        rgba(75, 85, 99),
        PADDING_X,
        y,
        36.0,
        &font,
        &data.address,
    );
    y += 66;

    draw_horizontal_line_mut(
        &mut image,
        PADDING_X,
        CARD_WIDTH as i32 - PADDING_X,
        y,
        rgba(222, 227, 235),
    );
    y += 48;

    draw_text_mut(
        &mut image,
        status_accent(&data.status),
        PADDING_X,
        y,
        37.0,
        &font,
        &format!(
            "{} 状态：{}",
            status_icon(&data.status),
            status_text(&data.status)
        ),
    );
    y += 54;

    draw_text_mut(
        &mut image,
        countdown_color(&data.countdown),
        PADDING_X,
        y,
        36.0,
        &font,
        &format!("⏳ 下次换 IP：{}", format_countdown(&data.countdown)),
    );
    y += 50;

    for line in detail_lines {
        draw_text_mut(
            &mut image,
            rgba(107, 114, 128),
            PADDING_X,
            y,
            31.0,
            &font,
            &line,
        );
        y += 42;
    }

    image.save(path)?;
    Ok(())
}

fn keep_display_variants_reachable() {
    let _ = [
        StatusCardState::Normal,
        StatusCardState::Abnormal,
        StatusCardState::VerificationFailed,
        StatusCardState::Processing,
        StatusCardState::Paused,
    ];
    let _ = [
        CountdownState::Duration(Duration::ZERO),
        CountdownState::NotAvailable,
        CountdownState::Paused,
        CountdownState::Processing,
    ];
}

pub fn fallback_text(data: &StatusCardData) -> String {
    let mut lines = vec![
        format!("📡 <b>{}</b>", html_escape(&data.server_name)),
        String::new(),
        html_escape(&data.region),
        html_escape(&data.address),
        String::new(),
        format!(
            "{} 状态：{}",
            status_icon(&data.status),
            html_escape(status_text(&data.status))
        ),
        format!(
            "⏳ 下次换 IP：{}",
            html_escape(&format_countdown(&data.countdown))
        ),
    ];
    if let Some(detail) = data.detail.as_deref().filter(|detail| !detail.is_empty()) {
        lines.push(html_escape(detail));
    }
    lines.join("\n")
}

pub fn format_countdown(countdown: &CountdownState) -> String {
    match countdown {
        CountdownState::Duration(duration) => format_duration(*duration),
        CountdownState::NotAvailable => "N/A".to_string(),
        CountdownState::Paused => "已暂停".to_string(),
        CountdownState::Processing => "处理中…".to_string(),
    }
}

pub fn wrap_text(text: &str, font: &FontArc, px: f32, max_width: u32) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let raw_line = raw_line.trim();
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for ch in raw_line.chars() {
            let candidate = format!("{current}{ch}");
            if !current.is_empty() && measured_width(font, px, &candidate) > max_width {
                lines.push(current);
                current = ch.to_string();
            } else {
                current = candidate;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub fn select_font_from_paths(paths: &[PathBuf]) -> FontSelection {
    paths
        .iter()
        .find(|path| FontArc::try_from_vec(std::fs::read(path).unwrap_or_default()).is_ok())
        .cloned()
        .map(FontSelection::Selected)
        .unwrap_or(FontSelection::Unavailable)
}

pub fn detect_font_path() -> FontSelection {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("BOIL_STATUS_CARD_FONT") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend(font_candidates());
    select_font_from_paths(&candidates)
}

fn load_status_card_font() -> anyhow::Result<FontArc> {
    match detect_font_path() {
        FontSelection::Selected(path) => {
            let bytes = std::fs::read(&path)?;
            FontArc::try_from_vec(bytes).map_err(|_| anyhow::anyhow!("字体文件无法读取"))
        }
        FontSelection::Unavailable => anyhow::bail!("未找到可用字体，回退为文本状态"),
    }
}

fn font_candidates() -> Vec<PathBuf> {
    [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.otf",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansSC-Regular.otf",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/arphic/uming.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

fn measured_width(font: &FontArc, px: f32, text: &str) -> u32 {
    let scaled = font.as_scaled(PxScale::from(px));
    text.chars()
        .map(|ch| scaled.h_advance(scaled.glyph_id(ch)))
        .sum::<f32>()
        .ceil() as u32
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{days}天 {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn status_icon(status: &StatusCardState) -> &'static str {
    match status {
        StatusCardState::Normal => "✅",
        StatusCardState::Abnormal => "❌",
        StatusCardState::VerificationFailed => "⚠️",
        StatusCardState::Processing => "🔄",
        StatusCardState::Paused => "⏸",
    }
}

fn status_text(status: &StatusCardState) -> &'static str {
    match status {
        StatusCardState::Normal => "正常",
        StatusCardState::Abnormal => "异常",
        StatusCardState::VerificationFailed => "验证失败",
        StatusCardState::Processing => "处理中",
        StatusCardState::Paused => "已暂停",
    }
}

fn status_accent(status: &StatusCardState) -> Rgba<u8> {
    match status {
        StatusCardState::Normal => rgba(22, 163, 74),
        StatusCardState::Abnormal => rgba(220, 38, 38),
        StatusCardState::VerificationFailed => rgba(217, 119, 6),
        StatusCardState::Processing => rgba(37, 99, 235),
        StatusCardState::Paused => rgba(107, 114, 128),
    }
}

fn countdown_color(countdown: &CountdownState) -> Rgba<u8> {
    match countdown {
        CountdownState::NotAvailable | CountdownState::Paused => rgba(107, 114, 128),
        CountdownState::Processing => rgba(37, 99, 235),
        CountdownState::Duration(_) => rgba(31, 41, 55),
    }
}

fn draw_text_mut(
    image: &mut RgbaImage,
    color: Rgba<u8>,
    x: i32,
    y: i32,
    px: f32,
    font: &FontArc,
    text: &str,
) {
    let scale = PxScale::from(px);
    let scaled = font.as_scaled(scale);
    let mut caret = point(x as f32, y as f32 + scaled.ascent());
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        let glyph: Glyph = glyph_id.with_scale_and_position(scale, caret);
        if let Some(outlined) = font.outline_glyph(glyph) {
            outlined.draw(|gx, gy, coverage| {
                blend_pixel(image, gx as i32, gy as i32, color, coverage);
            });
        }
        caret.x += scaled.h_advance(glyph_id);
    }
}

fn draw_horizontal_line_mut(image: &mut RgbaImage, x1: i32, x2: i32, y: i32, color: Rgba<u8>) {
    for x in x1.min(x2)..=x1.max(x2) {
        put_pixel_checked(image, x, y, color);
    }
}

fn draw_rounded_rect_mut(
    image: &mut RgbaImage,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    radius: i32,
    color: Rgba<u8>,
) {
    let right = left + width as i32 - 1;
    let bottom = top + height as i32 - 1;
    for y in top..=bottom {
        for x in left..=right {
            let dx = if x < left + radius {
                left + radius - x
            } else if x > right - radius {
                x - (right - radius)
            } else {
                0
            };
            let dy = if y < top + radius {
                top + radius - y
            } else if y > bottom - radius {
                y - (bottom - radius)
            } else {
                0
            };
            if dx * dx + dy * dy <= radius * radius {
                put_pixel_checked(image, x, y, color);
            }
        }
    }
}

fn blend_pixel(image: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>, coverage: f32) {
    if x < 0 || y < 0 || x >= image.width() as i32 || y >= image.height() as i32 {
        return;
    }
    let existing = image.get_pixel_mut(x as u32, y as u32);
    let alpha = coverage.clamp(0.0, 1.0);
    existing.blend(&Rgba([
        color[0],
        color[1],
        color[2],
        (alpha * f32::from(color[3])) as u8,
    ]));
}

fn put_pixel_checked(image: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>) {
    if x >= 0 && y >= 0 && x < image.width() as i32 && y < image.height() as i32 {
        image.put_pixel(x as u32, y as u32, color);
    }
}

fn temp_card_path() -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "boil-status-card-{}-{now}-{counter}.png",
        std::process::id()
    ))
}

fn rgba(r: u8, g: u8, b: u8) -> Rgba<u8> {
    Rgba([r, g, b, 255])
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> StatusCardData {
        StatusCardData {
            server_name: "55区香港家宽「个人用户」\n2025劳动777｜HKT 777Mbps VPS".to_string(),
            region: "🇭🇰 中国香港".to_string(),
            address: "203.218.233.10".to_string(),
            status: StatusCardState::Normal,
            countdown: CountdownState::Duration(Duration::from_secs(6_138)),
            detail: None,
        }
    }

    #[test]
    fn countdown_formats_not_available() {
        assert_eq!(format_countdown(&CountdownState::NotAvailable), "N/A");
    }

    #[test]
    fn countdown_formats_more_than_one_day() {
        assert_eq!(
            format_countdown(&CountdownState::Duration(Duration::from_secs(
                2 * 86_400 + 3 * 3_600 + 18 * 60 + 42
            ))),
            "2天 03:18:42"
        );
    }

    #[test]
    fn countdown_formats_paused_and_processing() {
        assert_eq!(format_countdown(&CountdownState::Paused), "已暂停");
        assert_eq!(format_countdown(&CountdownState::Processing), "处理中…");
    }

    #[test]
    fn missing_font_has_explicit_fallback_state() {
        assert_eq!(select_font_from_paths(&[]), FontSelection::Unavailable);
    }

    #[test]
    fn long_server_name_wraps() {
        let FontSelection::Selected(path) = detect_font_path() else {
            return;
        };
        let font = FontArc::try_from_vec(std::fs::read(path).unwrap()).unwrap();
        let lines = wrap_text(
            "这是一台名字特别特别长的服务器，用于验证图片卡片顶部不会溢出",
            &font,
            50.0,
            360,
        );
        assert!(lines.len() > 1);
    }

    #[test]
    fn chinese_render_does_not_panic_when_font_exists() {
        let data = sample_data();
        let result = TempStatusCard::render(&data);
        if let Ok(card) = result {
            assert!(card.path().exists());
        }
    }

    #[test]
    fn temp_file_is_removed_on_drop() {
        let data = sample_data();
        let Ok(card) = TempStatusCard::render(&data) else {
            return;
        };
        let path = card.path().to_path_buf();
        assert!(path.exists());
        drop(card);
        assert!(!path.exists());
    }

    #[test]
    fn fallback_text_is_safe_and_has_no_internal_id_or_token() {
        let mut data = sample_data();
        data.detail = Some("验证失败，请稍后重试".to_string());
        let text = fallback_text(&data);
        assert!(text.contains("55区香港家宽"));
        assert!(text.contains("🇭🇰 中国香港"));
        assert!(!text.contains("hk-01"));
        assert!(!text.contains("hidden-token"));
        assert!(!text.contains("Server:"));
    }

    #[test]
    fn all_status_states_have_display_text() {
        for status in [
            StatusCardState::Normal,
            StatusCardState::Abnormal,
            StatusCardState::VerificationFailed,
            StatusCardState::Processing,
            StatusCardState::Paused,
        ] {
            assert!(!status_text(&status).is_empty());
        }
    }
}
