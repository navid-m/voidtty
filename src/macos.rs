//! Cocoa workarounds for the GLFW version bundled with this build.
//!
//! AppKit consumes Ctrl+Tab and Ctrl+Shift+Tab before they reach `keyDown:`,
//! so GLFW 3.3 never reports them. GLFW 3.5 handles these as key equivalents
//! upstream; until the bundled copy is updated, claim them here and leave a
//! note for the main loop to pick up.

use std::sync::atomic::{AtomicI8, Ordering};

use objc2::{
    ffi, msg_send,
    runtime::{AnyClass, AnyObject, Bool, Imp, Sel},
};

static PENDING_TAB_STEP: AtomicI8 = AtomicI8::new(0);

/// Returns a tab step requested by Ctrl+Tab (`1`) or Ctrl+Shift+Tab (`-1`).
pub fn take_pending_tab_step() -> Option<isize> {
    match PENDING_TAB_STEP.swap(0, Ordering::SeqCst) {
        1 => Some(1),
        -1 => Some(-1),
        _ => None,
    }
}

/// Installs a `performKeyEquivalent:` override on the GLFW content view.
pub fn install_ctrl_tab_handler(ns_window: *mut std::ffi::c_void) {
    const KEYCODE_TAB: u16 = 48;
    const MOD_SHIFT: usize = 1 << 17;
    const MOD_CONTROL: usize = 1 << 18;

    unsafe extern "C-unwind" fn perform_key_equivalent(
        view: *mut AnyObject,
        cmd: Sel,
        event: *mut AnyObject,
    ) -> Bool {
        let mods: usize = unsafe { msg_send![event, modifierFlags] };
        let keycode: u16 = unsafe { msg_send![event, keyCode] };
        if mods & MOD_CONTROL != 0 && keycode == KEYCODE_TAB {
            let step = if mods & MOD_SHIFT != 0 { -1 } else { 1 };
            PENDING_TAB_STEP.store(step, Ordering::SeqCst);
            return Bool::YES;
        }
        unsafe { super_perform_key_equivalent(view, cmd, event) }
    }

    unsafe fn super_perform_key_equivalent(
        view: *mut AnyObject,
        cmd: Sel,
        event: *mut AnyObject,
    ) -> Bool {
        let class = unsafe { ffi::object_getClass(view) };
        let superclass = unsafe { ffi::class_getSuperclass(class) };
        let Some(method) =
            (unsafe { superclass.as_ref() }).and_then(|superclass| superclass.instance_method(cmd))
        else {
            return Bool::NO;
        };
        let imp: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool =
            unsafe { std::mem::transmute(method.implementation()) };
        unsafe { imp(view, cmd, event) }
    }

    unsafe {
        let ns_window = ns_window as *mut AnyObject;
        if ns_window.is_null() {
            return;
        }
        let view: *mut AnyObject = msg_send![ns_window, contentView];
        if view.is_null() {
            return;
        }
        let view_class = ffi::object_getClass(view) as *mut AnyClass;
        let selector = Sel::register(c"performKeyEquivalent:");
        let imp: Imp = std::mem::transmute(
            perform_key_equivalent
                as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
        );
        // BOOL from (id self, SEL _cmd, NSEvent *event): "c@:@".
        let _ = ffi::class_addMethod(view_class, selector, imp, c"c@:@".as_ptr());
    }
}
