//! macOS app control for Supported Apps: activation, Accessibility window
//! titles, clipboard paste with restore, and synthesized keys via CGEvent.

use crate::session::{InjectError, Injector};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSPasteboard, NSPasteboardItem, NSPasteboardTypeString,
    NSPasteboardWriting, NSRunningApplication, NSWorkspace,
};
use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXUIElement,
    kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};
use objc2_foundation::{NSArray, NSData, NSString};
use std::ptr::NonNull;
use std::thread::sleep;
use std::time::{Duration, Instant};

const KEY_V: u16 = 9;
const KEY_RETURN: u16 = 36;
const KEY_DELETE: u16 = 51;
/// How long the pasted text stays on the clipboard before restoring.
const RESTORE_DELAY: Duration = Duration::from_millis(400);
const ACTIVATE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Check Accessibility trust, optionally showing the system prompt.
pub fn accessibility_trusted(prompt: bool) -> bool {
    if !prompt {
        return unsafe { AXIsProcessTrusted() };
    }
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let value = CFBoolean::new(true);
    let dict = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[value]);
    unsafe { AXIsProcessTrustedWithOptions(Some(dict.as_opaque())) }
}

fn running_app(bundle_id: &str) -> Option<Retained<NSRunningApplication>> {
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
        bundle_id,
    ));
    apps.iter().find(|a| !a.isTerminated())
}

/// Whether an app with this bundle id is running (any thread).
pub fn app_running(bundle_id: &str) -> bool {
    running_app(bundle_id).is_some()
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
}

/// The main display is asleep: nobody is looking, background polling can
/// slow right down.
pub fn display_asleep() -> bool {
    // SAFETY: plain CoreGraphics queries without arguments to keep alive.
    unsafe { CGDisplayIsAsleep(CGMainDisplayID()) != 0 }
}

fn frontmost_app() -> Option<Retained<NSRunningApplication>> {
    NSWorkspace::sharedWorkspace().frontmostApplication()
}

fn ax_copy(element: &AXUIElement, attribute: &str) -> Option<CFRetained<CFType>> {
    let attr = CFString::from_str(attribute);
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut value)) };
    if err != AXError::Success || value.is_null() {
        return None;
    }
    // SAFETY: Copy rule — we own the returned reference.
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(value as *mut CFType)) })
}

fn window_title(pid: i32) -> Option<String> {
    let app = unsafe { AXUIElement::new_application(pid) };
    let window = ax_copy(&app, "AXFocusedWindow").or_else(|| ax_copy(&app, "AXMainWindow"))?;
    let window = window.downcast::<AXUIElement>().ok()?;
    let title = ax_copy(&window, "AXTitle")?;
    let title = title.downcast::<CFString>().ok()?;
    Some(title.to_string())
}

fn is_frontmost(pid: i32) -> bool {
    // AX reads the live state; NSWorkspace depends on the main run loop.
    let app = unsafe { AXUIElement::new_application(pid) };
    if let Some(v) = ax_copy(&app, "AXFrontmost")
        && let Ok(b) = v.downcast::<CFBoolean>()
    {
        return b.as_bool();
    }
    frontmost_app().is_some_and(|a| a.processIdentifier() == pid)
}

fn activate(app: &NSRunningApplication) -> Result<(), InjectError> {
    let pid = app.processIdentifier();
    if is_frontmost(pid) {
        return Ok(());
    }
    #[allow(deprecated)]
    app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    let start = Instant::now();
    let mut tried_ax = false;
    while start.elapsed() < ACTIVATE_TIMEOUT {
        sleep(Duration::from_millis(30));
        if is_frontmost(pid) {
            // Let the app settle its key window before keys arrive.
            sleep(Duration::from_millis(80));
            return Ok(());
        }
        if !tried_ax && start.elapsed() > Duration::from_millis(300) {
            // Cooperative activation may refuse a background agent; AX can
            // still raise the app.
            tried_ax = true;
            let el = unsafe { AXUIElement::new_application(pid) };
            let attr = CFString::from_str("AXFrontmost");
            let _ = unsafe { el.set_attribute_value(&attr, CFBoolean::new(true).as_ref()) };
        }
    }
    Err(InjectError::Failed("app did not come to the front".into()))
}

fn post_key(key: u16, flags: CGEventFlags) -> Result<(), InjectError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    for down in [true, false] {
        let ev = CGEvent::new_keyboard_event(source.as_deref(), key, down)
            .ok_or_else(|| InjectError::Failed("cannot create key event".into()))?;
        CGEvent::set_flags(Some(&ev), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    }
    Ok(())
}

type SavedClipboard = Vec<Vec<(Retained<NSString>, Retained<NSData>)>>;

fn save_clipboard(pb: &NSPasteboard) -> SavedClipboard {
    let Some(items) = pb.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| {
            item.types()
                .iter()
                .filter_map(|t| item.dataForType(&t).map(|d| (t.clone(), d)))
                .collect()
        })
        .collect()
}

fn restore_clipboard(pb: &NSPasteboard, saved: SavedClipboard) {
    pb.clearContents();
    if saved.is_empty() {
        return;
    }
    let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = saved
        .into_iter()
        .map(|entries| {
            let item = NSPasteboardItem::new();
            for (t, d) in entries {
                item.setData_forType(&d, &t);
            }
            ProtocolObject::from_retained(item)
        })
        .collect();
    let array = NSArray::from_retained_slice(&items);
    pb.writeObjects(&array);
}

/// Real macOS injector. Never launches apps.
#[derive(Default)]
pub struct MacInjector;

impl MacInjector {
    /// Check Accessibility, then bring the app to the front.
    fn target(&self, bundle_id: &str) -> Result<(), InjectError> {
        if !accessibility_trusted(false) {
            return Err(InjectError::Permission);
        }
        let app = running_app(bundle_id).ok_or(InjectError::NotRunning)?;
        activate(&app)
    }
}

impl Injector for MacInjector {
    fn accessibility_trusted(&mut self) -> bool {
        accessibility_trusted(false)
    }

    fn is_running(&mut self, bundle_id: &str) -> bool {
        running_app(bundle_id).is_some()
    }

    fn frontmost_bundle_id(&mut self) -> Option<String> {
        frontmost_app()
            .and_then(|a| a.bundleIdentifier())
            .map(|b| b.to_string())
    }

    fn window_title(&mut self, bundle_id: &str) -> Option<String> {
        if !accessibility_trusted(false) {
            return None;
        }
        window_title(running_app(bundle_id)?.processIdentifier())
    }

    fn activate(&mut self, bundle_id: &str) -> Result<(), InjectError> {
        let app = running_app(bundle_id).ok_or(InjectError::NotRunning)?;
        activate(&app)
    }

    fn insert(&mut self, bundle_id: &str, text: &str) -> Result<(), InjectError> {
        self.target(bundle_id)?;
        let pb = NSPasteboard::generalPasteboard();
        let saved = save_clipboard(&pb);
        pb.clearContents();
        let item = NSPasteboardItem::new();
        item.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
        // Ask clipboard managers to ignore this transient entry.
        item.setString_forType(
            &NSString::from_str(""),
            &NSString::from_str("org.nspasteboard.TransientType"),
        );
        let array = NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]);
        pb.writeObjects(&array);
        let ours = pb.changeCount();
        log::info!("paste: clipboard set (change {ours}), posting Cmd+V");
        let result = post_key(KEY_V, CGEventFlags::MaskCommand);
        sleep(RESTORE_DELAY);
        if pb.changeCount() == ours {
            restore_clipboard(&pb, saved);
        } else {
            log::info!("clipboard changed meanwhile; not restoring");
        }
        result
    }

    fn submit(&mut self, bundle_id: &str) -> Result<(), InjectError> {
        self.target(bundle_id)?;
        post_key(KEY_RETURN, CGEventFlags::empty())
    }

    fn delete_back(&mut self, bundle_id: &str, count: usize) -> Result<(), InjectError> {
        self.target(bundle_id)?;
        for _ in 0..count {
            post_key(KEY_DELETE, CGEventFlags::empty())?;
            sleep(Duration::from_millis(4));
        }
        Ok(())
    }
}
