//! Native macOS non-activating floating `NSPanel` compact pill overlay (zero WebView).
//!
//! Renders a high-contrast dark obsidian pill (forced `DarkAqua` appearance + solid-feeling
//! deep obsidian gradient surface + dual dark-outer / luminous-inner rim, with no vibrancy
//! bleed-through) so it stays crisp and legible over both Light Mode and Dark Mode windows:
//! - Listening (`284 × 62 pt`): mic name, glowing status dot, 9-bar FFT waveform, cancel `×`
//! - Polishing (`284 × 62 pt`): spinning `NSProgressIndicator` + `"Polishing…"` + cancel `×`
//! - Notice (`310 × 44 pt`): compact error/warning pill auto-dismissed after 4 seconds

use crate::settings::OverlayPosition;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSBezierPath, NSButton, NSCellImagePosition, NSColor, NSControlSize, NSEvent, NSFont,
    NSFontWeightMedium, NSFontWeightSemibold, NSGradient, NSImage, NSLineCapStyle, NSPanel,
    NSProgressIndicator, NSProgressIndicatorStyle, NSScreen, NSTextAlignment, NSTextField, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

type CGFloat = f64;

const COMPACT_WIDTH: CGFloat = 284.0;
const COMPACT_HEIGHT: CGFloat = 62.0;
const NOTICE_WIDTH: CGFloat = 310.0;
const NOTICE_HEIGHT: CGFloat = 44.0;

const TOP_OFFSET: CGFloat = 46.0;
const BOTTOM_OFFSET: CGFloat = 16.0;
const WAVE_BARS: usize = 9;
const NOTICE_HOLD: Duration = Duration::from_secs(4);
const NS_STATUS_WINDOW_LEVEL: isize = 25;

static OVERLAY_GEN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceAccent {
    WarmingUp,
    Listening,
    Polishing,
    Error,
    Warning,
}

fn corner_radius_for_height(h: CGFloat) -> CGFloat {
    if h <= NOTICE_HEIGHT + 1.0 {
        h / 2.0
    } else {
        22.0
    }
}

fn waveform_bar_color(index: usize) -> Retained<NSColor> {
    // Smooth high-luminance cyan -> icy white -> lavender gradient matching LIVI branding.
    let u = index as CGFloat / (WAVE_BARS - 1) as CGFloat;
    let (r, g, b) = if u <= 0.5 {
        let t = u * 2.0;
        (
            0.34 + (0.88 - 0.34) * t,
            0.88 + (0.97 - 0.88) * t,
            1.0,
        )
    } else {
        let t = (u - 0.5) * 2.0;
        (
            0.88 + (0.76 - 0.88) * t,
            0.97 + (0.60 - 0.97) * t,
            1.0,
        )
    };
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 0.98)
}

struct SurfaceIvars {
    accent: Cell<SurfaceAccent>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LiviOverlaySurfaceView"]
    #[ivars = SurfaceIvars]
    struct LiviOverlaySurfaceView;

    impl LiviOverlaySurfaceView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let radius = corner_radius_for_height(bounds.size.height);
            let accent = self.ivars().accent.get();

            // Deep high-opacity obsidian gradient so bright/colorful backgrounds never bleed
            // through and wash out the foreground elements.
            let fill_path =
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, radius, radius);
            let top_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.09, 0.11, 0.16, 0.96);
            let bottom_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.05, 0.09, 0.98);
            if let Some(grad) = NSGradient::initWithStartingColor_endingColor(
                NSGradient::alloc(),
                &bottom_bg,
                &top_bg,
            ) {
                grad.drawInBezierPath_angle(&fill_path, 90.0);
            } else {
                top_bg.setFill();
                fill_path.fill();
            }

            // Subtle state-matched wash for errors/warnings so alerts are instantly scannable.
            let wash = match accent {
                SurfaceAccent::Error => {
                    Some(NSColor::colorWithSRGBRed_green_blue_alpha(0.95, 0.24, 0.28, 0.12))
                }
                SurfaceAccent::Warning => {
                    Some(NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.72, 0.20, 0.10))
                }
                _ => None,
            };
            if let Some(wash_color) = wash {
                wash_color.setFill();
                fill_path.fill();
            }

            // Inner luminous accent rim so the pill stands out cleanly on dark backgrounds.
            let inner_inset: CGFloat = 1.5;
            let inner_rect = NSRect::new(
                NSPoint::new(bounds.origin.x + inner_inset, bounds.origin.y + inner_inset),
                NSSize::new(
                    (bounds.size.width - inner_inset * 2.0).max(0.0),
                    (bounds.size.height - inner_inset * 2.0).max(0.0),
                ),
            );
            let inner_r = (radius - inner_inset).max(0.0);
            let inner_path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                inner_rect, inner_r, inner_r,
            );
            let inner_rim = match accent {
                SurfaceAccent::WarmingUp => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.84, 0.90, 1.0, 0.34)
                }
                SurfaceAccent::Listening => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.45, 0.88, 1.0, 0.46)
                }
                SurfaceAccent::Polishing => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.68, 0.56, 1.0, 0.52)
                }
                SurfaceAccent::Error => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.46, 0.46, 0.68)
                }
                SurfaceAccent::Warning => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.80, 0.32, 0.62)
                }
            };
            inner_rim.setStroke();
            inner_path.setLineWidth(1.25);
            inner_path.stroke();

            // Outer dark hairline so the pill edge stays razor-sharp against bright white pages.
            let outer_inset: CGFloat = 0.5;
            let outer_rect = NSRect::new(
                NSPoint::new(bounds.origin.x + outer_inset, bounds.origin.y + outer_inset),
                NSSize::new(
                    (bounds.size.width - outer_inset * 2.0).max(0.0),
                    (bounds.size.height - outer_inset * 2.0).max(0.0),
                ),
            );
            let outer_r = (radius - outer_inset).max(0.0);
            let outer_path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                outer_rect, outer_r, outer_r,
            );
            NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.78).setStroke();
            outer_path.setLineWidth(1.0);
            outer_path.stroke();
        }
    }
);

impl LiviOverlaySurfaceView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(SurfaceIvars {
            accent: Cell::new(SurfaceAccent::WarmingUp),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn set_accent(&self, accent: SurfaceAccent) {
        self.ivars().accent.set(accent);
        self.setNeedsDisplay(true);
    }
}

struct WaveformIvars {
    levels: RefCell<[f32; WAVE_BARS]>,
    ready: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LiviWaveformView"]
    #[ivars = WaveformIvars]
    struct LiviWaveformView;

    impl LiviWaveformView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let levels = self.ivars().levels.borrow();
            let ready = self.ivars().ready.get();

            // Glowing live recording status dot on the left.
            let dot_cx: CGFloat = 13.0;
            let dot_cy = bounds.size.height / 2.0;

            let halo_r: CGFloat = 6.5;
            let halo_color = if ready {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.30, 0.35, 0.30)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.78, 0.24, 0.28)
            };
            halo_color.setFill();
            NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(dot_cx - halo_r, dot_cy - halo_r),
                NSSize::new(halo_r * 2.0, halo_r * 2.0),
            ))
            .fill();

            let dot_r: CGFloat = 4.25;
            let dot_color = if ready {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.32, 0.36, 1.0)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.78, 0.24, 1.0)
            };
            dot_color.setFill();
            NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(dot_cx - dot_r, dot_cy - dot_r),
                NSSize::new(dot_r * 2.0, dot_r * 2.0),
            ))
            .fill();

            // 9 centered rounded high-contrast waveform bars.
            let bar_w: CGFloat = 4.0;
            let gap: CGFloat = 5.5;
            let total_w = (WAVE_BARS as CGFloat) * bar_w + ((WAVE_BARS - 1) as CGFloat) * gap;
            let start_x = (bounds.size.width - total_w) / 2.0 + 4.0;
            let mid_y = bounds.size.height / 2.0;

            let standby_color =
                NSColor::colorWithSRGBRed_green_blue_alpha(0.76, 0.85, 0.98, 0.58);

            for (i, &val) in levels.iter().enumerate() {
                if ready {
                    waveform_bar_color(i).setFill();
                } else {
                    standby_color.setFill();
                }
                let h = if ready {
                    (4.5 + (val.clamp(0.0, 1.0) as CGFloat).powf(0.65) * 17.5).clamp(4.5, 22.0)
                } else {
                    4.5
                };
                let x = start_x + (i as CGFloat) * (bar_w + gap);
                let y = mid_y - h / 2.0;
                let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(bar_w, h));
                let pill = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    rect,
                    bar_w / 2.0,
                    bar_w / 2.0,
                );
                pill.fill();
            }
        }
    }
);

impl LiviWaveformView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(WaveformIvars {
            levels: RefCell::new([0.0; WAVE_BARS]),
            ready: Cell::new(false),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn set_ready(&self, ready: bool) {
        self.ivars().ready.set(ready);
        self.setNeedsDisplay(true);
    }

    fn update_levels(&self, incoming: &[f32]) {
        let mut smoothed = self.ivars().levels.borrow_mut();
        for (i, slot) in smoothed.iter_mut().enumerate() {
            let target = incoming.get(i).copied().unwrap_or(0.0);
            *slot = *slot * 0.65 + target * 0.35;
        }
        drop(smoothed);
        self.setNeedsDisplay(true);
    }

    fn reset(&self) {
        *self.ivars().levels.borrow_mut() = [0.0; WAVE_BARS];
        self.ivars().ready.set(false);
        self.setNeedsDisplay(true);
    }
}

struct ButtonTargetIvars {
    on_cancel: Arc<dyn Fn() + Send + Sync>,
}

define_class!(
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LiviOverlayActionTarget"]
    #[ivars = ButtonTargetIvars]
    struct LiviOverlayActionTarget;

    impl LiviOverlayActionTarget {
        #[unsafe(method(onCancelClicked:))]
        fn on_cancel_clicked(&self, _sender: *mut AnyObject) {
            (self.ivars().on_cancel)();
        }
    }
);

impl LiviOverlayActionTarget {
    fn new(mtm: MainThreadMarker, on_cancel: Arc<dyn Fn() + Send + Sync>) -> Retained<Self> {
        let this = mtm
            .alloc::<Self>()
            .set_ivars(ButtonTargetIvars { on_cancel });
        unsafe { msg_send![super(this), init] }
    }
}

struct OverlayInner {
    panel: Retained<NSPanel>,
    surface: Retained<LiviOverlaySurfaceView>,
    mic_label: Retained<NSTextField>,
    waveform: Retained<LiviWaveformView>,
    spinner: Retained<NSProgressIndicator>,
    status_label: Retained<NSTextField>,
    cancel_btn: Retained<NSButton>,
    _action_target: Retained<LiviOverlayActionTarget>,
    position: OverlayPosition,
    notice_mode: bool,
}

thread_local! {
    static OVERLAY: RefCell<Option<OverlayInner>> = const { RefCell::new(None) };
}

fn build_cancel_icon() -> Retained<NSImage> {
    let size = NSSize::new(22.0, 22.0);
    let handler = RcBlock::new(|dst_rect: NSRect| -> Bool {
        let ox = dst_rect.origin.x;
        let oy = dst_rect.origin.y;
        let w = dst_rect.size.width;
        let h = dst_rect.size.height;

        // Subtle circular pill backing so the close affordance is always crisp and clear.
        let badge = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
            NSPoint::new(ox + 1.0, oy + 1.0),
            NSSize::new(w - 2.0, h - 2.0),
        ));
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.14).setFill();
        badge.fill();
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.28).setStroke();
        badge.setLineWidth(0.9);
        badge.stroke();

        // Crisp high-contrast white cross.
        let cx = ox + w / 2.0;
        let cy = oy + h / 2.0;
        let arm: CGFloat = 3.6;
        let cross = NSBezierPath::bezierPath();
        cross.moveToPoint(NSPoint::new(cx - arm, cy - arm));
        cross.lineToPoint(NSPoint::new(cx + arm, cy + arm));
        cross.moveToPoint(NSPoint::new(cx + arm, cy - arm));
        cross.lineToPoint(NSPoint::new(cx - arm, cy + arm));
        cross.setLineWidth(1.6);
        cross.setLineCapStyle(NSLineCapStyle::Round);
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.94).setStroke();
        cross.stroke();

        Bool::YES
    });

    let img = NSImage::imageWithSize_flipped_drawingHandler(size, false, &handler);
    img.setTemplate(false);
    img
}

fn make_label(
    mtm: MainThreadMarker,
    frame: NSRect,
    font: &NSFont,
    color: &NSColor,
    align: NSTextAlignment,
) -> Retained<NSTextField> {
    let label = NSTextField::initWithFrame(mtm.alloc(), frame);
    label.setBezeled(false);
    label.setDrawsBackground(false);
    label.setEditable(false);
    label.setSelectable(false);
    label.setAlignment(align);
    label.setFont(Some(font));
    label.setTextColor(Some(color));
    label
}

/// Initializes the hidden floating `NSPanel` on the macOS main thread.
pub fn init_on_main(mtm: MainThreadMarker, on_cancel: Arc<dyn Fn() + Send + Sync>) {
    OVERLAY.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }

        let initial_rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(COMPACT_WIDTH, COMPACT_HEIGHT),
        );
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            initial_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setLevel(NS_STATUS_WINDOW_LEVEL);
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHidesOnDeactivate(false);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );

        // Force DarkAqua appearance on the panel so macOS Light Mode never washes out the HUD
        // or turns the NSProgressIndicator spinner dark-on-dark.
        let dark_appearance = unsafe { NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua) };
        if let Some(ref appearance) = dark_appearance {
            panel.setAppearance(Some(appearance));
        }

        let content_bounds = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(COMPACT_WIDTH, COMPACT_HEIGHT),
        );
        let surface = LiviOverlaySurfaceView::new(mtm, content_bounds);
        if let Some(ref appearance) = dark_appearance {
            surface.setAppearance(Some(appearance));
        }

        let mic_font = unsafe { NSFont::systemFontOfSize_weight(11.5, NSFontWeightMedium) };
        let mic_color = NSColor::colorWithSRGBRed_green_blue_alpha(0.92, 0.95, 1.0, 0.90);
        let mic_label = make_label(
            mtm,
            NSRect::new(
                NSPoint::new(14.0, 40.0),
                NSSize::new(COMPACT_WIDTH - 28.0, 16.0),
            ),
            &mic_font,
            &mic_color,
            NSTextAlignment::Center,
        );

        let waveform = LiviWaveformView::new(
            mtm,
            NSRect::new(
                NSPoint::new(12.0, 8.0),
                NSSize::new(COMPACT_WIDTH - 52.0, 28.0),
            ),
        );

        let spinner = NSProgressIndicator::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(18.0, 14.0), NSSize::new(16.0, 16.0)),
        );
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setControlSize(NSControlSize::Small);
        spinner.setIndeterminate(true);
        spinner.setHidden(true);

        let status_font = unsafe { NSFont::systemFontOfSize_weight(13.0, NSFontWeightSemibold) };
        let status_color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.98);
        let status_label = make_label(
            mtm,
            NSRect::new(
                NSPoint::new(42.0, 13.0),
                NSSize::new(COMPACT_WIDTH - 84.0, 18.0),
            ),
            &status_font,
            &status_color,
            NSTextAlignment::Center,
        );
        status_label.setHidden(true);

        let action_target = LiviOverlayActionTarget::new(mtm, on_cancel);
        let cancel_btn = NSButton::initWithFrame(
            mtm.alloc(),
            NSRect::new(
                NSPoint::new(COMPACT_WIDTH - 34.0, 11.0),
                NSSize::new(22.0, 22.0),
            ),
        );
        cancel_btn.setBordered(false);
        cancel_btn.setTitle(&NSString::from_str(""));
        cancel_btn.setImage(Some(&build_cancel_icon()));
        cancel_btn.setImagePosition(NSCellImagePosition::ImageOnly);
        unsafe {
            cancel_btn.setTarget(Some(&action_target));
            cancel_btn.setAction(Some(sel!(onCancelClicked:)));
        }

        surface.addSubview(&mic_label);
        surface.addSubview(&waveform);
        surface.addSubview(&spinner);
        surface.addSubview(&status_label);
        surface.addSubview(&cancel_btn);

        panel.setContentView(Some(&surface));
        panel.orderOut(None);

        *slot.borrow_mut() = Some(OverlayInner {
            panel,
            surface,
            mic_label,
            waveform,
            spinner,
            status_label,
            cancel_btn,
            _action_target: action_target,
            position: OverlayPosition::Bottom,
            notice_mode: false,
        });
    });
}

fn target_screen(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let mouse = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    for i in 0..screens.count() {
        let screen = screens.objectAtIndex(i);
        let frame = screen.frame();
        if mouse.x >= frame.origin.x
            && mouse.x <= frame.origin.x + frame.size.width
            && mouse.y >= frame.origin.y
            && mouse.y <= frame.origin.y + frame.size.height
        {
            return Some(screen);
        }
    }
    NSScreen::mainScreen(mtm)
}

fn frame_for_size(
    mtm: MainThreadMarker,
    position: OverlayPosition,
    w: CGFloat,
    h: CGFloat,
) -> NSRect {
    let Some(screen) = target_screen(mtm) else {
        return NSRect::new(NSPoint::new(200.0, 80.0), NSSize::new(w, h));
    };
    let visible = screen.visibleFrame();
    let full = screen.frame();
    let x = full.origin.x + (full.size.width - w) / 2.0;
    let y = match position {
        OverlayPosition::Bottom => visible.origin.y + BOTTOM_OFFSET,
        OverlayPosition::Top => full.origin.y + full.size.height - h - TOP_OFFSET,
    };
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn layout_subviews(inner: &OverlayInner, w: CGFloat, h: CGFloat) {
    let full = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
    inner.surface.setFrame(full);
    inner.surface.setNeedsDisplay(true);

    if inner.notice_mode {
        inner.mic_label.setHidden(true);
        inner.waveform.setHidden(true);
        inner.spinner.setHidden(true);
        inner.cancel_btn.setHidden(true);
        inner.status_label.setHidden(false);
        inner.status_label.setFrame(NSRect::new(
            NSPoint::new(16.0, (h - 18.0) / 2.0),
            NSSize::new(w - 32.0, 18.0),
        ));
        return;
    }

    inner.mic_label.setHidden(false);
    inner.mic_label.setFrame(NSRect::new(
        NSPoint::new(14.0, h - 22.0),
        NSSize::new(w - 28.0, 16.0),
    ));
    inner.cancel_btn.setHidden(false);
    inner.cancel_btn.setFrame(NSRect::new(
        NSPoint::new(w - 34.0, 10.0),
        NSSize::new(22.0, 22.0),
    ));
    inner.waveform.setFrame(NSRect::new(
        NSPoint::new(12.0, 7.0),
        NSSize::new(w - 52.0, 28.0),
    ));
    inner.spinner.setFrame(NSRect::new(
        NSPoint::new(18.0, 13.0),
        NSSize::new(16.0, 16.0),
    ));
    inner.status_label.setFrame(NSRect::new(
        NSPoint::new(42.0, 12.0),
        NSSize::new(w - 84.0, 18.0),
    ));
}

fn apply_frame(mtm: MainThreadMarker, inner: &OverlayInner, w: CGFloat, h: CGFloat) {
    let rect = frame_for_size(mtm, inner.position, w, h);
    inner.panel.setFrame_display(rect, true);
    layout_subviews(inner, w, h);
    inner.panel.invalidateShadow();
}

pub fn show_listening(mic_name: String, position: OverlayPosition) {
    OVERLAY_GEN.fetch_add(1, Ordering::SeqCst);
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        OVERLAY.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let Some(inner) = borrow.as_mut() else { return };
            inner.position = position;
            inner.notice_mode = false;
            inner.surface.set_accent(SurfaceAccent::WarmingUp);

            inner
                .mic_label
                .setStringValue(&NSString::from_str(&mic_name));
            inner.waveform.reset();
            inner.waveform.setHidden(false);
            unsafe { inner.spinner.stopAnimation(None) };
            inner.spinner.setHidden(true);
            inner.status_label.setHidden(true);
            inner
                .status_label
                .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                    1.0, 1.0, 1.0, 0.98,
                )));

            apply_frame(mtm, inner, COMPACT_WIDTH, COMPACT_HEIGHT);
            inner.panel.orderFrontRegardless();
        });
    });
}

pub fn mark_capture_ready() {
    DispatchQueue::main().exec_async(|| {
        OVERLAY.with(|slot| {
            if let Some(inner) = slot.borrow().as_ref() {
                inner.surface.set_accent(SurfaceAccent::Listening);
                inner.waveform.set_ready(true);
            }
        });
    });
}

pub fn update_levels(levels: Vec<f32>) {
    DispatchQueue::main().exec_async(move || {
        OVERLAY.with(|slot| {
            if let Some(inner) = slot.borrow().as_ref()
                && !inner.notice_mode
                && !inner.waveform.isHidden()
            {
                inner.waveform.update_levels(&levels);
            }
        });
    });
}

pub fn show_polishing() {
    DispatchQueue::main().exec_async(|| {
        OVERLAY.with(|slot| {
            let borrow = slot.borrow();
            let Some(inner) = borrow.as_ref() else { return };
            if inner.notice_mode {
                return;
            }
            inner.surface.set_accent(SurfaceAccent::Polishing);
            inner.waveform.setHidden(true);
            inner.spinner.setHidden(false);
            unsafe { inner.spinner.startAnimation(None) };
            inner
                .status_label
                .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                    1.0, 1.0, 1.0, 0.98,
                )));
            inner
                .status_label
                .setStringValue(&NSString::from_str("Polishing…"));
            inner.status_label.setHidden(false);
        });
    });
}

pub fn show_notice(message: String, kind: NoticeKind, position: OverlayPosition) {
    let generation = OVERLAY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        OVERLAY.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let Some(inner) = borrow.as_mut() else { return };
            inner.position = position;
            inner.notice_mode = true;
            unsafe { inner.spinner.stopAnimation(None) };

            let (prefix, accent, color) = match kind {
                NoticeKind::Error => (
                    "⚠ ",
                    SurfaceAccent::Error,
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.56, 0.54, 1.0),
                ),
                NoticeKind::Warning => (
                    "● ",
                    SurfaceAccent::Warning,
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.86, 0.42, 1.0),
                ),
            };
            inner.surface.set_accent(accent);
            inner.status_label.setTextColor(Some(&color));
            inner
                .status_label
                .setStringValue(&NSString::from_str(&format!("{prefix}{message}")));

            apply_frame(mtm, inner, NOTICE_WIDTH, NOTICE_HEIGHT);
            inner.panel.orderFrontRegardless();
        });
    });

    std::thread::spawn(move || {
        std::thread::sleep(NOTICE_HOLD);
        if OVERLAY_GEN.load(Ordering::SeqCst) == generation {
            hide();
        }
    });
}

pub fn hide() {
    OVERLAY_GEN.fetch_add(1, Ordering::SeqCst);
    DispatchQueue::main().exec_async(|| {
        OVERLAY.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let Some(inner) = borrow.as_mut() else { return };
            unsafe { inner.spinner.stopAnimation(None) };
            inner.panel.orderOut(None);
            inner.notice_mode = false;
        });
    });
}
