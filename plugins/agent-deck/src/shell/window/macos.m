#import <AppKit/AppKit.h>
#include <stdint.h>

extern NSStatusBarButton *corvid_status_button(void);

enum { PanelKeep, PanelOpen, PanelToggle, PanelHide };

static id globalMonitor;
static id localMonitor;
static NSUInteger presentationGeneration;
static NSSize requestedSize;

static const CGFloat backdropRadius = 16;

@interface CorvidPanelBackdrop : NSView
@property(nonatomic, strong) NSView *material;
@end

@implementation CorvidPanelBackdrop
- (instancetype)initWithFrame:(NSRect)frame {
    self = [super initWithFrame:frame];
    if (!self) return nil;

#if __MAC_OS_X_VERSION_MAX_ALLOWED >= 260000
    if (@available(macOS 26.0, *)) {
        NSGlassEffectView *glass = [[NSGlassEffectView alloc] initWithFrame:self.bounds];
        glass.cornerRadius = backdropRadius;
        self.material = glass;
    }
#endif
    if (!self.material) {
        NSVisualEffectView *popover = [[NSVisualEffectView alloc] initWithFrame:self.bounds];
        popover.material = NSVisualEffectMaterialPopover;
        popover.blendingMode = NSVisualEffectBlendingModeBehindWindow;
        popover.state = NSVisualEffectStateActive;
        self.material = popover;
    }
    self.material.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    self.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    [self addSubview:self.material];

    [NSWorkspace.sharedWorkspace.notificationCenter
        addObserver:self
        selector:@selector(displayOptionsChanged:)
        name:NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification
        object:nil];
    [self displayOptionsChanged:nil];
    return self;
}

- (void)dealloc {
    [NSWorkspace.sharedWorkspace.notificationCenter removeObserver:self];
}

- (NSView *)hitTest:(NSPoint)point {
    return nil;
}

- (void)displayOptionsChanged:(NSNotification *)notification {
    // Workspace notifications may arrive on a background thread.
    if (!NSThread.isMainThread) {
        dispatch_async(dispatch_get_main_queue(), ^{ [self displayOptionsChanged:nil]; });
        return;
    }
    self.material.hidden = NSWorkspace.sharedWorkspace.accessibilityDisplayShouldReduceTransparency;
    self.needsDisplay = YES;
    [self.window invalidateShadow];
}

- (void)viewDidChangeEffectiveAppearance {
    [super viewDidChangeEffectiveAppearance];
    self.needsDisplay = YES;
}

- (void)drawRect:(NSRect)dirtyRect {
    [super drawRect:dirtyRect];
    if (self.material.hidden) {
        [NSColor.windowBackgroundColor setFill];
        [[NSBezierPath bezierPathWithRoundedRect:self.bounds
            xRadius:backdropRadius yRadius:backdropRadius] fill];
    }
}
@end

static void installBackdrop(NSView *view) {
    NSView *container = view.superview;
    for (NSView *subview in container.subviews) {
        if ([subview isKindOfClass:CorvidPanelBackdrop.class]) return;
    }

    CorvidPanelBackdrop *backdrop = [[CorvidPanelBackdrop alloc] initWithFrame:container.bounds];
    [container addSubview:backdrop positioned:NSWindowBelow relativeTo:view];
}

static uint32_t colorRGBA(NSColor *color) {
    NSColor *rgb = [color colorUsingColorSpace:NSColorSpace.sRGBColorSpace];
    return ((uint32_t)lround(rgb.redComponent * 255) << 24)
        | ((uint32_t)lround(rgb.greenComponent * 255) << 16)
        | ((uint32_t)lround(rgb.blueComponent * 255) << 8)
        | (uint32_t)lround(rgb.alphaComponent * 255);
}

void corvid_panel_colors(void *view, uint32_t colors[9]) {
    NSAppearance *appearance = ((__bridge NSView *)view).effectiveAppearance;
    [appearance performAsCurrentDrawingAppearance:^{
        colors[0] = colorRGBA(NSColor.controlBackgroundColor);
        colors[1] = colorRGBA(NSColor.labelColor);
        colors[2] = colorRGBA(NSColor.secondaryLabelColor);
        colors[3] = colorRGBA(NSColor.separatorColor);
        colors[4] = colorRGBA(NSColor.controlAccentColor);
        colors[5] = colorRGBA(NSColor.quaternaryLabelColor);
        colors[6] = colorRGBA(NSColor.unemphasizedSelectedContentBackgroundColor);
        colors[7] = colorRGBA(NSColor.quaternaryLabelColor);
    }];
    NSString *match = [appearance bestMatchFromAppearancesWithNames:@[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]];
    colors[8] = [match isEqualToString:NSAppearanceNameDarkAqua];
}

void corvid_panel_prepare(void) {
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
}

static void hidePanel(NSWindow *panel) {
    presentationGeneration++;
    if (globalMonitor) [NSEvent removeMonitor:globalMonitor];
    if (localMonitor) [NSEvent removeMonitor:localMonitor];
    globalMonitor = nil;
    localMonitor = nil;
    [panel orderOut:nil];
}

static bool belongsToPanel(NSWindow *window, NSWindow *panel) {
    for (NSWindow *candidate = window; candidate; candidate = candidate.parentWindow) {
        if (candidate == panel) return true;
    }
    return false;
}

static NSRect statusButtonFrame(NSStatusBarButton *button) {
    if (!button.window || button.isHiddenOrHasHiddenAncestor) return NSZeroRect;
    return [button.window convertRectToScreen:[button convertRect:button.bounds toView:nil]];
}

static NSScreen *screenContaining(NSPoint point) {
    for (NSScreen *screen in NSScreen.screens) {
        if (NSPointInRect(point, screen.frame)) return screen;
    }
    return nil;
}

static NSScreen *statusAnchor(NSRect *anchor) {
    *anchor = statusButtonFrame(corvid_status_button());
    if (NSIsEmptyRect(*anchor)) return nil;
    NSScreen *screen = screenContaining(NSMakePoint(NSMidX(*anchor), NSMidY(*anchor)));
    if (!screen) return nil;

    // AppKit can expose a placeholder frame near the display origin until
    // the status button is laid out. Only use frames that reach the menu bar.
    CGFloat menuBarBottom = NSMaxY(screen.frame) - NSStatusBar.systemStatusBar.thickness;
    return NSMaxY(*anchor) > menuBarBottom ? screen : nil;
}

static bool onStatusButton(void) {
    NSRect anchor;
    return statusAnchor(&anchor) != nil && NSPointInRect(NSEvent.mouseLocation, anchor);
}

static void dismissOnOutsideClick(NSWindow *panel) {
    if (globalMonitor || localMonitor) return;
    NSEventMask clicks = NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown | NSEventMaskOtherMouseDown;
    __weak NSWindow *weakPanel = panel;
    globalMonitor = [NSEvent addGlobalMonitorForEventsMatchingMask:clicks handler:^(NSEvent *event) {
        (void)event;
        if (!onStatusButton()) hidePanel(weakPanel);
    }];
    localMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:clicks handler:^NSEvent *(NSEvent *event) {
        if (!belongsToPanel(event.window, weakPanel) && !onStatusButton()) {
            hidePanel(weakPanel);
        }
        return event;
    }];
}

static void placePanel(NSWindow *panel, NSRect work, NSRect anchor) {
    NSSize requested = requestedSize;
    NSSize content = panel.contentView.bounds.size;
    NSSize frame = panel.frame.size;
    // Allow scrolling within the panel when the requested view is larger than
    // the available display. Leave room for the AppKit frame around the view.
    if (!NSIsEmptyRect(work)) {
        requested.width = MIN(requested.width, MAX(1, work.size.width - (frame.width - content.width)));
        requested.height = MIN(requested.height, MAX(1, work.size.height - (frame.height - content.height)));
    }
    if (!NSEqualSizes(content, requested)) [panel setContentSize:requested];
    if (NSIsEmptyRect(work)) return;

    frame = panel.frame.size;
    CGFloat x = NSIsEmptyRect(anchor) ? NSMaxX(work) - frame.width - 4 : NSMidX(anchor) - frame.width / 2;
    CGFloat top = NSIsEmptyRect(anchor) ? NSMaxY(work) : NSMinY(anchor);
    CGFloat y = top - frame.height - 4;
    [panel setFrameOrigin:NSMakePoint(
        MAX(NSMinX(work), MIN(x, NSMaxX(work) - frame.width)),
        MAX(NSMinY(work), MIN(y, NSMaxY(work) - frame.height))
    )];
}

static void presentPanel(NSWindow *panel, NSUInteger generation, NSUInteger retries) {
    if (!panel || generation != presentationGeneration) return;

    NSRect anchor;
    NSScreen *screen = statusAnchor(&anchor);
    if (!screen && retries > 0) {
        // The status item can exist before AppKit lays it out in the menu bar.
        __weak NSWindow *target = panel;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
            presentPanel(target, generation, retries - 1);
        });
        return;
    }

    if (!screen) {
        screen = screenContaining(NSEvent.mouseLocation) ?: panel.screen ?: NSScreen.mainScreen;
        anchor = NSZeroRect;
    }
    placePanel(panel, screen.visibleFrame, anchor);
    [panel makeKeyAndOrderFront:nil];
    dismissOnOutsideClick(panel);
}

void corvid_panel_hide(void *view) {
    NSWindow *panel = [(__bridge NSView *)view window];
    dispatch_async(dispatch_get_main_queue(), ^{ hidePanel(panel); });
}

void corvid_panel_update(void *view, double width, double height, uint8_t presentation) {
    __weak NSView *weakView = (__bridge NSView *)view;
    // Resize, measure and position after GPUI releases the current update.
    dispatch_async(dispatch_get_main_queue(), ^{
        NSView *content = weakView;
        NSPanel *panel = (NSPanel *)content.window;
        if (!panel) return;
        if (presentation == PanelHide || (presentation == PanelToggle && panel.visible)) {
            hidePanel(panel);
            return;
        }

        installBackdrop(content);
        panel.hidesOnDeactivate = NO;
        panel.movable = NO;
        panel.titleVisibility = NSWindowTitleHidden;
        for (NSWindowButton button = NSWindowCloseButton; button <= NSWindowZoomButton; button++) {
            [panel standardWindowButton:button].hidden = YES;
        }
        requestedSize = NSMakeSize(width, height);
        if (presentation == PanelOpen || presentation == PanelToggle) {
            presentPanel(panel, ++presentationGeneration, 10);
        } else if (panel.visible) {
            NSRect anchor;
            NSScreen *screen = statusAnchor(&anchor);
            placePanel(panel, (screen ?: panel.screen ?: NSScreen.mainScreen).visibleFrame,
                screen ? anchor : NSZeroRect);
        }
    });
}
