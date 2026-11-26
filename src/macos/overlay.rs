//! Native macOS non-activating floating `NSPanel` compact pill overlay (zero WebView).
//!
//! Renders a dark translucent pill (`NSVisualEffectView`, `HUDWindow` material) with:
//! - Listening (`284 × 62 pt`): mic name, pulsing dot, 9-bar FFT waveform, cancel `×`
//! - Polishing (`284 × 62 pt`): spinning `NSProgressIndicator` + `"Polishing…"` + cancel `×`
//! - Notice (`310 × 44 pt`): compact error/warning pill auto-dismissed after 4 seconds

use crate::settings::OverlayPosition;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSButton, NSColor, NSControlSize, NSEvent, NSFont, NSPanel,
    NSProgressIndicator, NSProgressIndicatorStyle, NSScreen, NSTextAlignment, NSTextField, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
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

            // Pulsing recording dot on the left.
            let dot_size: CGFloat = 8.0;
            let dot_x: CGFloat = 8.0;
            let dot_y = (bounds.size.height - dot_size) / 2.0;
            let dot_color = if ready {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.36, 0.38, 0.98)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.72, 0.25, 0.85)
            };
            dot_color.setFill();
            let dot = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(dot_x, dot_y),
                NSSize::new(dot_size, dot_size),
            ));
            dot.fill();

            // 9 centered rounded waveform bars.
            let bar_w: CGFloat = 3.5;
            let gap: CGFloat = 5.0;
            let total_w = (WAVE_BARS as CGFloat) * bar_w + ((WAVE_BARS - 1) as CGFloat) * gap;
            let start_x = (bounds.size.width - total_w) / 2.0 + 4.0;
            let mid_y = bounds.size.height / 2.0;

            let bar_color = if ready {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.92)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.35)
            };
            bar_color.setFill();

            for (i, &val) in levels.iter().enumerate() {
                let h = if ready {
                    (3.5 + (val.clamp(0.0, 1.0) as CGFloat).powf(0.7) * 16.5).clamp(3.5, 20.0)
                } else {
                    4.0
                };
                let x = start_x + (i as CGFloat) * (bar_w + gap);
                let y = mid_y - h / 2.0;
                let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(bar_w, h));
                let pill = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 1.75, 1.75);
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
    blur: Retained<NSVisualEffectView>,
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

fn make_label(
    mtm: MainThreadMarker,
    frame: NSRect,
    font_size: CGFloat,
    alpha: CGFloat,
    align: NSTextAlignment,
) -> Retained<NSTextField> {
    let label = NSTextField::initWithFrame(mtm.alloc(), frame);
    label.setBezeled(false);
    label.setDrawsBackground(false);
    label.setEditable(false);
    label.setSelectable(false);
    label.setAlignment(align);
    label.setFont(Some(&NSFont::systemFontOfSize(font_size)));
    label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, alpha,
    )));
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

        let blur = NSVisualEffectView::initWithFrame(
            mtm.alloc(),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(COMPACT_WIDTH, COMPACT_HEIGHT),
            ),
        );
        blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
        blur.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        blur.setState(NSVisualEffectState::Active);
        blur.setWantsLayer(true);
        if let Some(layer) = blur.layer() {
            layer.setCornerRadius(20.0);
            layer.setMasksToBounds(true);
            layer.setBorderWidth(1.0);
            let border = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.16);
            layer.setBorderColor(Some(&border.CGColor()));
        }

        let mic_label = make_label(
            mtm,
            NSRect::new(
                NSPoint::new(14.0, 40.0),
                NSSize::new(COMPACT_WIDTH - 28.0, 16.0),
            ),
            11.0,
            0.62,
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

        let status_label = make_label(
            mtm,
            NSRect::new(
                NSPoint::new(42.0, 13.0),
                NSSize::new(COMPACT_WIDTH - 84.0, 18.0),
            ),
            12.5,
            0.94,
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
        cancel_btn.setTitle(&NSString::from_str("✕"));
        cancel_btn.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        unsafe {
            cancel_btn.setTarget(Some(&action_target));
            cancel_btn.setAction(Some(sel!(onCancelClicked:)));
        }

        blur.addSubview(&mic_label);
        blur.addSubview(&waveform);
        blur.addSubview(&spinner);
        blur.addSubview(&status_label);
        blur.addSubview(&cancel_btn);

        panel.setContentView(Some(&blur));
        panel.orderOut(None);

        *slot.borrow_mut() = Some(OverlayInner {
            panel,
            blur,
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
    inner
        .blur
        .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));

    if inner.notice_mode {
        inner.mic_label.setHidden(true);
        inner.waveform.setHidden(true);
        inner.spinner.setHidden(true);
        inner.cancel_btn.setHidden(true);
        inner.status_label.setHidden(false);
        inner.status_label.setFrame(NSRect::new(
            NSPoint::new(14.0, (h - 18.0) / 2.0),
            NSSize::new(w - 28.0, 18.0),
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
                    1.0, 1.0, 1.0, 0.94,
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
            inner.waveform.setHidden(true);
            inner.spinner.setHidden(false);
            unsafe { inner.spinner.startAnimation(None) };
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

            let prefix = match kind {
                NoticeKind::Error => "⚠ ",
                NoticeKind::Warning => "● ",
            };
            let color = match kind {
                NoticeKind::Error => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.56, 0.54, 0.98)
                }
                NoticeKind::Warning => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.82, 0.42, 0.98)
                }
            };
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
