#import <AppKit/AppKit.h>
#import <UserNotifications/UserNotifications.h>
#include <stdatomic.h>
#include <unistd.h>

extern void corvid_native_ready(bool status_icon, bool notifications);
extern void corvid_native_submitted(unsigned long long high, unsigned long long low, bool accepted);
extern void corvid_native_activation_yielded(void *completion);

static atomic_bool stopping;

extern void corvid_native_event(unsigned int kind, unsigned long long high, unsigned long long low);

@interface CorvidDeckDelegate : NSObject <UNUserNotificationCenterDelegate>
@property NSStatusItem *item;
@property NSMenu *menu;
@property NSImage *icon;
@property NSImage *attentionIcon;
@end

@implementation CorvidDeckDelegate
- (void)statusClicked:(NSStatusBarButton *)button {
    if (NSApp.currentEvent.type == NSEventTypeRightMouseUp) {
        [self.menu popUpMenuPositioningItem:nil atLocation:NSMakePoint(0, NSHeight(button.bounds)) inView:button];
    } else {
        corvid_native_event(7, 0, 0);
    }
}
- (void)open:(id)sender { corvid_native_event(1, 0, 0); }
- (void)settings:(id)sender { corvid_native_event(2, 0, 0); }
- (void)pause:(id)sender { corvid_native_event(3, 0, 0); }
- (void)quit:(id)sender { corvid_native_event(4, 0, 0); }
- (void)userNotificationCenter:(UNUserNotificationCenter *)center
    willPresentNotification:(UNNotification *)notification
    withCompletionHandler:(void (^)(UNNotificationPresentationOptions))completion {
    completion(UNNotificationPresentationOptionBanner | UNNotificationPresentationOptionList | UNNotificationPresentationOptionSound);
}
- (void)userNotificationCenter:(UNUserNotificationCenter *)center
    didReceiveNotificationResponse:(UNNotificationResponse *)response
    withCompletionHandler:(void (^)(void))completion {
    if ([response.actionIdentifier isEqualToString:UNNotificationDefaultActionIdentifier]) {
        NSDictionary *scope = response.notification.request.content.userInfo;
        corvid_native_event(5, [scope[@"corvid-high"] unsignedLongLongValue], [scope[@"corvid-low"] unsignedLongLongValue]);
    }
    completion();
}
@end

static CorvidDeckDelegate *delegate;

NSStatusBarButton *corvid_status_button(void) { return delegate.item.button; }

static NSImage *statusIcon(bool attention) {
    NSImage *image = [NSImage imageWithSize:NSMakeSize(18, 18) flipped:YES drawingHandler:^BOOL(NSRect rect) {
        // Geometric Wing mark matching assets/icons/corvid.svg, without the app tile.
        NSBezierPath *upper = [NSBezierPath bezierPath];
        [upper moveToPoint:NSMakePoint(3.5, 18.8)];
        const NSPoint upperPoints[] = {
            {9.1, 6.4}, {14.1, 6.4}, {19.5, 10.9}, {12.5, 12.5}, {8.4, 18.8},
        };
        for (NSUInteger i = 0; i < sizeof(upperPoints) / sizeof(upperPoints[0]); i++) {
            [upper lineToPoint:upperPoints[i]];
        }
        [upper closePath];
        [upper appendBezierPathWithOvalInRect:NSMakeRect(10.9, 8.7, 1.8, 1.8)];
        upper.windingRule = NSWindingRuleEvenOdd;

        NSBezierPath *wing = [NSBezierPath bezierPath];
        [wing moveToPoint:NSMakePoint(8.4, 18.8)];
        const NSPoint wingPoints[] = {
            {12.5, 12.5}, {18.1, 15.2}, {12.9, 19.9},
        };
        for (NSUInteger i = 0; i < sizeof(wingPoints) / sizeof(wingPoints[0]); i++) {
            [wing lineToPoint:wingPoints[i]];
        }
        [wing closePath];

        [NSGraphicsContext saveGraphicsState];
        NSAffineTransform *transform = [NSAffineTransform transform];
        [transform translateXBy:NSMinX(rect) yBy:NSMinY(rect)];
        [transform scaleXBy:NSWidth(rect) / 24 yBy:NSHeight(rect) / 24];
        [transform concat];
        [NSColor.blackColor setFill];
        [upper fill];
        [[NSColor.blackColor colorWithAlphaComponent:0.55] setFill];
        [wing fill];
        if (attention) {
            [NSColor.blackColor setFill];
            [[NSBezierPath bezierPathWithOvalInRect:NSMakeRect(18.0, 2.0, 4.5, 4.5)] fill];
        }
        [NSGraphicsContext restoreGraphicsState];
        return YES;
    }];
    image.template = YES;
    image.accessibilityDescription = attention ? @"Corvid Agent Deck, attention needed" : @"Corvid Agent Deck";
    return image;
}

static void yieldActivation(unsigned int pid, void *completion, unsigned int attempts) {
    if (!atomic_load(&stopping)) {
        if (NSApp.isActive) {
            NSRunningApplication *desktop =
                [NSRunningApplication runningApplicationWithProcessIdentifier:(pid_t)pid];
            if (desktop) [NSApp yieldActivationToApplication:desktop];
        } else if (attempts > 0) {
            // WindowServer can finish activating the clicked panel asynchronously.
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_MSEC),
                dispatch_get_main_queue(), ^{
                    yieldActivation(pid, completion, attempts - 1);
                });
            return;
        }
    }
    corvid_native_activation_yielded(completion);
}

void corvid_native_yield_activation(unsigned int pid, void *completion) {
    dispatch_async(dispatch_get_main_queue(), ^{
        yieldActivation(pid, completion, 10);
    });
}

void corvid_native_start(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (atomic_load(&stopping)) return;
        delegate = [CorvidDeckDelegate new];
        delegate.item = [[NSStatusBar systemStatusBar] statusItemWithLength:NSVariableStatusItemLength];
        delegate.icon = statusIcon(false);
        delegate.attentionIcon = statusIcon(true);
        delegate.item.button.image = delegate.icon;
        delegate.item.button.imagePosition = NSImageOnly;
        delegate.item.button.toolTip = @"Corvid Agent Deck";
        NSMenu *menu = [NSMenu new];
        NSArray *titles = @[@"Open Agent Deck", @"Notifications & Settings", @"Pause / Resume", @"Quit Corvid Agent Deck"];
        SEL actions[] = {@selector(open:), @selector(settings:), @selector(pause:), @selector(quit:)};
        for (NSUInteger i = 0; i < titles.count; i++) {
            NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:titles[i] action:actions[i] keyEquivalent:@""];
            item.target = delegate;
            [menu addItem:item];
        }
        delegate.menu = menu;
        NSStatusBarButton *button = delegate.item.button;
        button.target = delegate;
        button.action = @selector(statusClicked:);
        [button sendActionOn:NSEventMaskLeftMouseUp | NSEventMaskRightMouseUp];
        bool notifications = false;
        @try {
            UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];
            center.delegate = delegate;
            notifications = center != nil;
        } @catch (NSException *exception) {
            // Unbundled launches may not have a notification center.
        }
        corvid_native_ready(button != nil, notifications);
    });
}

void corvid_native_stop(void) {
    atomic_store(&stopping, true);
    void (^cleanup)(void) = ^{
        if (!delegate) return;
        @try {
            UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];
            if (center.delegate == delegate) center.delegate = nil;
        } @catch (NSException *exception) {
            // No delegate was installed on this notification center.
        }
        if (delegate.item) [[NSStatusBar systemStatusBar] removeStatusItem:delegate.item];
        delegate = nil;
    };
    if ([NSThread isMainThread]) cleanup();
    else dispatch_async(dispatch_get_main_queue(), cleanup);
}

static NSString *pendingStatus;
static bool pendingAttention;

void corvid_native_status(const char *text, bool attention) {
    NSString *status = [[NSString alloc] initWithUTF8String:text];
    @synchronized (CorvidDeckDelegate.class) {
        bool scheduled = pendingStatus != nil;
        pendingStatus = status;
        pendingAttention = attention;
        if (scheduled) return;
    }
    dispatch_async(dispatch_get_main_queue(), ^{
        NSString *latest;
        bool showAttention;
        @synchronized (CorvidDeckDelegate.class) {
            latest = pendingStatus;
            showAttention = pendingAttention;
            pendingStatus = nil;
        }
        if (atomic_load(&stopping)) return;
        delegate.item.button.toolTip = latest;
        delegate.item.button.image = showAttention ? delegate.attentionIcon : delegate.icon;
    });
}

void corvid_native_notice(unsigned long long high, unsigned long long low, const char *title, const char *body, bool sound) {
    NSString *heading = [[NSString alloc] initWithUTF8String:title];
    NSString *message = [[NSString alloc] initWithUTF8String:body];
    dispatch_async(dispatch_get_main_queue(), ^{
        if (atomic_load(&stopping)) return;
        @try {
            UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];
            if (!center) { corvid_native_submitted(high, low, false); return; }
            [center requestAuthorizationWithOptions:UNAuthorizationOptionAlert | UNAuthorizationOptionSound
                completionHandler:^(BOOL granted, NSError *error) {
                    if (atomic_load(&stopping)) return;
                    if (!granted || error) { corvid_native_submitted(high, low, false); return; }
                    UNMutableNotificationContent *content = [UNMutableNotificationContent new];
                    content.title = heading;
                    content.body = message;
                    content.userInfo = @{@"corvid-high": @(high), @"corvid-low": @(low)};
                    if (sound) content.sound = [UNNotificationSound defaultSound];
                    UNNotificationRequest *request = [UNNotificationRequest requestWithIdentifier:[NSString stringWithFormat:@"%016llx%016llx", high, low] content:content trigger:nil];
                    [center addNotificationRequest:request withCompletionHandler:^(NSError *failure) {
                        corvid_native_submitted(high, low, failure == nil);
                    }];
                }];
        } @catch (NSException *exception) { corvid_native_submitted(high, low, false); }
    });
}
