use objc2::{
    ClassType, MainThreadMarker, define_class, msg_send, rc::Retained, runtime::AnyObject, sel,
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSRunningApplication};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSObject};

use crate::set_activation_policy;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "UsenvDockQuitHandler"]
    struct DockQuitHandler;

    impl DockQuitHandler {
        #[unsafe(method(handleQuitEvent:withReplyEvent:))]
        fn handle_quit_event(event: &NSAppleEventDescriptor, _reply: Option<&NSAppleEventDescriptor>) {
            // keySenderPIDAttr is a read-only SInt32 attribute supplied by macOS.
            let pid: Option<Retained<NSAppleEventDescriptor>> = unsafe {
                msg_send![event, attributeDescriptorForKeyword: u32::from_be_bytes(*b"spid")]
            };
            let sender: Option<Retained<NSRunningApplication>> = pid.and_then(|pid| unsafe {
                msg_send![NSRunningApplication::class(),
                    runningApplicationWithProcessIdentifier: pid.int32Value()]
            });
            let bundle = sender.and_then(|sender| sender.bundleIdentifier());
            if bundle.is_some_and(|id| id.to_string() == "com.apple.dock") {
                hide_to_menu_bar();
            } else {
                // Preserve normal termination for logout, restart and other system requests.
                NSApplication::sharedApplication(MainThreadMarker::new().expect("AppKit main thread"))
                    .terminate(None);
            }
        }
    }
);

pub fn install_quit_handler() {
    let manager = NSAppleEventManager::sharedAppleEventManager();
    let handler: &AnyObject = DockQuitHandler::class().as_ref();
    // A class object is a permanent target. The selector above implements the
    // NSAppleEventManager callback signature for kCoreEventClass / kAEQuitApplication.
    unsafe {
        let _: () = msg_send![&*manager,
            setEventHandler: handler,
            andSelector: sel!(handleQuitEvent:withReplyEvent:),
            forEventClass: u32::from_be_bytes(*b"aevt"),
            andEventID: u32::from_be_bytes(*b"quit"),
        ];
    }
}

pub fn hide_to_menu_bar() {
    let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("AppKit main thread"));
    app.hide(None);
    set_activation_policy(NSApplicationActivationPolicy::Accessory);
}
