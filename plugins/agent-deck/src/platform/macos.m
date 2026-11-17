#import <AppKit/AppKit.h>
#import <UserNotifications/UserNotifications.h>
#include <stdatomic.h>

extern void corvid_native_ready(bool status_icon, bool notifications);
extern void corvid_native_submitted(unsigned long long high, unsigned long long low, bool accepted);

static atomic_bool stopping;

extern void corvid_native_event(unsigned int kind, unsigned long long high, unsigned long long low);

@interface CorvidDeckDelegate : NSObject <UNUserNotificationCenterDelegate>
@property NSStatusItem *item;
@property NSMenu *menu;
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
    NSDictionary *scope = response.notification.request.content.userInfo;
    corvid_native_event(5, [scope[@"corvid-high"] unsignedLongLongValue], [scope[@"corvid-low"] unsignedLongLongValue]);
    completion();
}
@end

static CorvidDeckDelegate *delegate;

NSStatusBarButton *corvid_status_button(void) { return delegate.item.button; }

void corvid_native_start(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (atomic_load(&stopping)) return;
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
        delegate.item.button.title = showAttention ? @"◈ •" : @"◈";
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
