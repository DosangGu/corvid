#import <AppKit/AppKit.h>
#import <UserNotifications/UserNotifications.h>

extern void corvid_native_event(unsigned int kind, unsigned long long high, unsigned long long low);

@interface CorvidDeckDelegate : NSObject <UNUserNotificationCenterDelegate>
@property NSStatusItem *item;
@end

@implementation CorvidDeckDelegate
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
    NSDictionary *scope = response.notification.request.content.userInfo;
    corvid_native_event(5, [scope[@"corvid-high"] unsignedLongLongValue], [scope[@"corvid-low"] unsignedLongLongValue]);
    completion();
}
@end

static CorvidDeckDelegate *delegate;

void corvid_native_start(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        delegate = [CorvidDeckDelegate new];
        delegate.item = [[NSStatusBar systemStatusBar] statusItemWithLength:NSVariableStatusItemLength];
        delegate.item.button.title = @"◈";
        delegate.item.button.toolTip = @"Corvid Agent Deck";
        NSMenu *menu = [NSMenu new];
        NSArray *titles = @[@"Open Agent Deck", @"Notifications & Settings", @"Pause / Resume", @"Quit Corvid Agent Deck"];
        SEL actions[] = {@selector(open:), @selector(settings:), @selector(pause:), @selector(quit:)};
        for (NSUInteger i = 0; i < titles.count; i++) {
            NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:titles[i] action:actions[i] keyEquivalent:@""];
            item.target = delegate;
            [menu addItem:item];
        }
        delegate.item.menu = menu;
        @try { [UNUserNotificationCenter currentNotificationCenter].delegate = delegate; }
        @catch (NSException *exception) { corvid_native_event(6, 0, 0); }
    });
}

void corvid_native_stop(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (delegate.item) [[NSStatusBar systemStatusBar] removeStatusItem:delegate.item];
        delegate = nil;
    });
}

void corvid_native_status(const char *text, bool attention) {
    NSString *status = [[NSString alloc] initWithUTF8String:text];
    dispatch_async(dispatch_get_main_queue(), ^{
        delegate.item.button.toolTip = status;
        delegate.item.button.title = attention ? @"◈ •" : @"◈";
    });
}

void corvid_native_notice(unsigned long long high, unsigned long long low, const char *title, const char *body, bool sound) {
    NSString *heading = [[NSString alloc] initWithUTF8String:title];
    NSString *message = [[NSString alloc] initWithUTF8String:body];
    dispatch_async(dispatch_get_main_queue(), ^{
        @try {
            UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];
            [center requestAuthorizationWithOptions:UNAuthorizationOptionAlert | UNAuthorizationOptionSound
                completionHandler:^(BOOL granted, NSError *error) {
                    if (!granted || error) { corvid_native_event(6, 0, 0); return; }
                    UNMutableNotificationContent *content = [UNMutableNotificationContent new];
                    content.title = heading;
                    content.body = message;
                    content.userInfo = @{@"corvid-high": @(high), @"corvid-low": @(low)};
                    if (sound) content.sound = [UNNotificationSound defaultSound];
                    UNNotificationRequest *request = [UNNotificationRequest requestWithIdentifier:[NSString stringWithFormat:@"%016llx%016llx", high, low] content:content trigger:nil];
                    [center addNotificationRequest:request withCompletionHandler:^(NSError *failure) {
                        if (failure) corvid_native_event(6, 0, 0);
                    }];
                }];
        } @catch (NSException *exception) { corvid_native_event(6, 0, 0); }
    });
}

void corvid_window_visible(void *view, bool visible) {
    NSWindow *window = [(__bridge NSView *)view window];
    if (visible) [window orderFront:nil]; else [window orderOut:nil];
}

void corvid_window_configure(void *view, bool pinned, bool expanded) {
    NSWindow *window = [(__bridge NSView *)view window];
    window.level = pinned ? NSFloatingWindowLevel : NSNormalWindowLevel;
    window.movable = !pinned;
    window.titleVisibility = NSWindowTitleHidden;
    window.titlebarAppearsTransparent = YES;
    NSWindowStyleMask style = window.styleMask | NSWindowStyleMaskFullSizeContentView;
    window.styleMask = expanded && !pinned ? style | NSWindowStyleMaskResizable : style & ~NSWindowStyleMaskResizable;
    for (NSWindowButton button = NSWindowCloseButton; button <= NSWindowZoomButton; button++)
        [window standardWindowButton:button].hidden = YES;
}

void corvid_window_position(void *view, double width, double height, bool initial) {
    NSWindow *window = [(__bridge NSView *)view window];
    NSScreen *screen = window.screen ?: [NSScreen mainScreen];
    if (initial) {
        NSPoint mouse = [NSEvent mouseLocation];
        for (NSScreen *candidate in [NSScreen screens]) if (NSPointInRect(mouse, candidate.frame)) screen = candidate;
    }
    NSRect work = screen.visibleFrame;
    NSPoint origin = window.frame.origin;
    if (initial) origin = NSMakePoint(NSMaxX(work) - width - 16, NSMinY(work) + 16);
    origin.x = MAX(NSMinX(work), MIN(origin.x, NSMaxX(work) - width));
    origin.y = MAX(NSMinY(work), MIN(origin.y, NSMaxY(work) - height));
    [window setFrameOrigin:origin];
}
