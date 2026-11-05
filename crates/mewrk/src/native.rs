//! The macOS side of the shell: upstream's `app_tray.rs`, `app_menu.rs` and `app_keys.rs`.
//!
//! Closing the main window only hides it; background subagents, workflows, shell tasks and the model runs keep going. The
//! menu bar item offers exactly two actions, bring the window back or quit, and quitting is the only one that runs the exit
//! barrier. If the item cannot be installed the window's close falls back to quitting, so the process can never become
//! unreachable. A closed window takes the app out of the Dock and the app switcher, and bringing it back restores both.
//!
//! winit's default menu quits through `-[NSApplication terminate:]`, which ends the process whatever the handler does, so
//! Cmd+Q would skip the flushes and the "save failed, keep the application open" answer. Quit here is an ordinary item that
//! asks the barrier instead. The rest is the usual menu rebuilt item by item, worded in the app's language.
//!
//! A browser page is a child of the window's content view. A key it leaves unhandled (→ with the caret at the end of a field, a
//! letter with no field focused) goes up the responder chain to winit's view, which would hand it to whatever field of ours last
//! had the keyboard. [`drop_bubbled_keys`] stops it there.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Imp, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask, NSEventType, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSView};
use objc2_foundation::{NSArray, NSData, NSObject, NSObjectProtocol, NSPoint, NSSize, NSString};

/// What the native side asks of the window driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEvent {
    /// Quit, from the application menu or the menu bar item: through the barrier.
    Quit,
    /// `Settings…`.
    Settings,
    /// Bring the window back, from the menu bar item or the Dock.
    Show,
}

type Sink = Box<dyn Fn(NativeEvent) + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();

fn post(event: NativeEvent) {
    if let Some(sink) = SINK.get() {
        sink(event);
    }
}

/// Every worded entry of the menu bar. The application menu's own title is not one: macOS always shows the app's name there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    About,
    Settings,
    Services,
    Hide,
    HideOthers,
    Quit,
    File,
    CloseWindow,
    Edit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    View,
    FullScreen,
    Window,
    Minimize,
    Zoom,
    Help,
    TrayOpen,
    TrayQuit,
}

impl Label {
    /// The words macOS itself uses for these items in each language.
    pub fn text(self, chinese: bool) -> &'static str {
        let (zh, en) = match self {
            Self::About => ("关于 Mewrk", "About Mewrk"),
            Self::Settings => ("设置…", "Settings…"),
            Self::Services => ("服务", "Services"),
            Self::Hide => ("隐藏 Mewrk", "Hide Mewrk"),
            Self::HideOthers => ("隐藏其他", "Hide Others"),
            Self::Quit | Self::TrayQuit => ("退出 Mewrk", "Quit Mewrk"),
            Self::File => ("文件", "File"),
            Self::CloseWindow => ("关闭窗口", "Close Window"),
            Self::Edit => ("编辑", "Edit"),
            Self::Undo => ("撤销", "Undo"),
            Self::Redo => ("重做", "Redo"),
            Self::Cut => ("剪切", "Cut"),
            Self::Copy => ("拷贝", "Copy"),
            Self::Paste => ("粘贴", "Paste"),
            Self::SelectAll => ("全选", "Select All"),
            Self::View => ("显示", "View"),
            Self::FullScreen => ("切换全屏幕", "Toggle Full Screen"),
            Self::Window => ("窗口", "Window"),
            Self::Minimize => ("最小化", "Minimize"),
            Self::Zoom => ("缩放", "Zoom"),
            Self::Help => ("帮助", "Help"),
            Self::TrayOpen => ("打开 Mewrk", "Open Mewrk"),
        };
        if chinese { zh } else { en }
    }
}

define_class!(
    // The target menu items and the status item send to. Everything it does is posted to the event loop, so nothing runs
    // inside AppKit's menu tracking.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MewrkMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(mewrkQuit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            post(NativeEvent::Quit);
        }

        #[unsafe(method(mewrkSettings:))]
        fn settings(&self, _sender: Option<&AnyObject>) {
            post(NativeEvent::Settings);
        }

        #[unsafe(method(mewrkShow:))]
        fn show(&self, _sender: Option<&AnyObject>) {
            post(NativeEvent::Show);
        }

        /// A left click brings the window back and a right click opens the menu. macOS gives every click on a status item
        /// that owns a menu to that menu, so the item is built without one and is lent it only here: attached, opened,
        /// detached, which is how AppKit apps give a status item a left-click action and a right-click menu.
        #[unsafe(method(mewrkStatusClicked:))]
        fn status_clicked(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            let application = NSApplication::sharedApplication(mtm);
            let right = application.currentEvent().is_some_and(|event| event.r#type() == NSEventType::RightMouseUp);
            if !right {
                post(NativeEvent::Show);
                return;
            }
            let ivars = self.ivars();
            let (Some(item), Some(menu)) = (ivars.status_item.get(), ivars.status_menu.get()) else { return };
            // SAFETY: both were built on this thread and live as long as the target, and `performClick:` is the documented way
            // to open a status item's menu.
            unsafe {
                item.setMenu(Some(menu));
                if let Some(button) = item.button(mtm) {
                    button.performClick(None);
                }
                item.setMenu(None);
            }
        }
    }
);

struct TargetIvars {
    status_item: OnceLock2<Retained<NSStatusItem>>,
    status_menu: OnceLock2<Retained<NSMenu>>,
}

/// A slot set once on the main thread. The class's ivars must be `'static`, and these are never touched elsewhere.
struct OnceLock2<T>(std::cell::OnceCell<T>);

impl<T> OnceLock2<T> {
    fn new() -> Self {
        Self(std::cell::OnceCell::new())
    }

    fn get(&self) -> Option<&T> {
        self.0.get()
    }

    fn set(&self, value: T) {
        let _ = self.0.set(value);
    }
}

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { status_item: OnceLock2::new(), status_menu: OnceLock2::new() });
        // SAFETY: `NSObject`'s `init`.
        unsafe { msg_send![super(this), init] }
    }
}

/// What the app holds on to: dropping it would take the status item off the bar.
pub struct NativeShell {
    target: Retained<MenuTarget>,
    labelled_items: Vec<(Label, Retained<NSMenuItem>)>,
    labelled_menus: Vec<(Label, Retained<NSMenu>)>,
    language: bool,
    tray_installed: bool,
}

fn item(mtm: MainThreadMarker, title: &str, action: Option<Sel>, key: &str) -> Retained<NSMenuItem> {
    // SAFETY: an AppKit object made on the main thread; the selector is checked by the responder chain when it is sent.
    unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), action, &NSString::from_str(key)) }
}

fn submenu(mtm: MainThreadMarker, title: &str, items: &[Retained<NSMenuItem>]) -> (Retained<NSMenuItem>, Retained<NSMenu>) {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    for entry in items {
        menu.addItem(entry);
    }
    let holder = item(mtm, title, None, "");
    holder.setSubmenu(Some(&menu));
    (holder, menu)
}

/// Installs the menu bar and the menu bar item. Must run on the main thread after the event loop started, which `resumed`
/// is. `events` hears what they ask for, from the main thread.
pub fn install(events: impl Fn(NativeEvent) + Send + Sync + 'static, chinese: bool, template_png: Option<&[u8]>) -> Option<NativeShell> {
    let mtm = MainThreadMarker::new()?;
    let _ = SINK.set(Box::new(events));
    let application = NSApplication::sharedApplication(mtm);
    let target = MenuTarget::new(mtm);
    let any_target: &AnyObject = target.as_ref();

    let labelled_items = std::cell::RefCell::new(Vec::new());
    let mut labelled_menus = Vec::new();
    let labelled = |label: Label, action: Option<Sel>, key: &str, targeted: bool| -> Retained<NSMenuItem> {
        let entry = item(mtm, label.text(chinese), action, key);
        if targeted {
            // SAFETY: the target outlives the items, which `NativeShell` holds together.
            unsafe { entry.setTarget(Some(any_target)) };
        }
        labelled_items.borrow_mut().push((label, entry.clone()));
        entry
    };

    {
        let separator = || NSMenuItem::separatorItem(mtm);
        // ---- the application menu
        let about = labelled(Label::About, Some(sel!(orderFrontStandardAboutPanel:)), "", false);
        let settings = labelled(Label::Settings, Some(sel!(mewrkSettings:)), ",", true);
        let services = labelled(Label::Services, None, "", false);
        let services_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(Label::Services.text(chinese)));
        services.setSubmenu(Some(&services_menu));
        application.setServicesMenu(Some(&services_menu));
        labelled_menus.push((Label::Services, services_menu));
        let hide = labelled(Label::Hide, Some(sel!(hide:)), "h", false);
        let hide_others = labelled(Label::HideOthers, Some(sel!(hideOtherApplications:)), "h", false);
        hide_others.setKeyEquivalentModifierMask(objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Option);
        let quit = labelled(Label::Quit, Some(sel!(mewrkQuit:)), "q", true);
        let (application_holder, _) = submenu(mtm, "Mewrk", &[about, separator(), settings, separator(), services, separator(), hide, hide_others, separator(), quit]);

        // ---- File
        let close_window = labelled(Label::CloseWindow, Some(sel!(performClose:)), "w", false);
        let (file_holder, file_menu) = submenu(mtm, Label::File.text(chinese), &[close_window]);
        labelled_items.borrow_mut().push((Label::File, file_holder.clone()));
        labelled_menus.push((Label::File, file_menu));

        // ---- Edit: so Cut, Copy and Paste reach whatever native field has the keyboard (a page's), and so they read in the
        // menu at all. The element layer hears them first, as key presses, when one of its own fields has the keyboard.
        let undo = labelled(Label::Undo, Some(sel!(undo:)), "z", false);
        let redo = labelled(Label::Redo, Some(sel!(redo:)), "Z", false);
        let cut = labelled(Label::Cut, Some(sel!(cut:)), "x", false);
        let copy = labelled(Label::Copy, Some(sel!(copy:)), "c", false);
        let paste = labelled(Label::Paste, Some(sel!(paste:)), "v", false);
        let select_all = labelled(Label::SelectAll, Some(sel!(selectAll:)), "a", false);
        let (edit_holder, edit_menu) = submenu(mtm, Label::Edit.text(chinese), &[undo, redo, separator(), cut, copy, paste, select_all]);
        labelled_items.borrow_mut().push((Label::Edit, edit_holder.clone()));
        labelled_menus.push((Label::Edit, edit_menu));

        // ---- View
        let full_screen = labelled(Label::FullScreen, Some(sel!(toggleFullScreen:)), "f", false);
        full_screen.setKeyEquivalentModifierMask(objc2_app_kit::NSEventModifierFlags::Command | objc2_app_kit::NSEventModifierFlags::Control);
        let (view_holder, view_menu) = submenu(mtm, Label::View.text(chinese), &[full_screen]);
        labelled_items.borrow_mut().push((Label::View, view_holder.clone()));
        labelled_menus.push((Label::View, view_menu));

        // ---- Window and Help: macOS treats these as the Window and Help menus (the window list, the search field).
        let minimize = labelled(Label::Minimize, Some(sel!(performMiniaturize:)), "m", false);
        let zoom = labelled(Label::Zoom, Some(sel!(performZoom:)), "", false);
        let close = labelled(Label::CloseWindow, Some(sel!(performClose:)), "", false);
        let (window_holder, window_menu) = submenu(mtm, Label::Window.text(chinese), &[minimize, zoom, separator(), close]);
        labelled_items.borrow_mut().push((Label::Window, window_holder.clone()));
        application.setWindowsMenu(Some(&window_menu));
        labelled_menus.push((Label::Window, window_menu));
        let (help_holder, help_menu) = submenu(mtm, Label::Help.text(chinese), &[]);
        labelled_items.borrow_mut().push((Label::Help, help_holder.clone()));
        application.setHelpMenu(Some(&help_menu));
        labelled_menus.push((Label::Help, help_menu));

        let main_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Mewrk"));
        for holder in [application_holder, file_holder, edit_holder, view_holder, window_holder, help_holder] {
            main_menu.addItem(&holder);
        }
        application.setMainMenu(Some(&main_menu));
    }

    let mut tray_installed = false;
    if let Some(png) = template_png {
        // SAFETY: as above.
        unsafe {
            let bar = NSStatusBar::systemStatusBar();
            let status = bar.statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = status.button(mtm) {
                let data = NSData::with_bytes(png);
                if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                    // A template image is what the bar tints for a light, dark or highlighted bar.
                    image.setTemplate(true);
                    image.setSize(NSSize::new(9.0, 18.0));
                    button.setImage(Some(&image));
                }
                button.setToolTip(Some(&NSString::from_str("Mewrk")));
                button.setTarget(Some(any_target));
                button.setAction(Some(sel!(mewrkStatusClicked:)));
                let _: isize = msg_send![&*button, sendActionOn: (NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp).bits()];
                let open = labelled(Label::TrayOpen, Some(sel!(mewrkShow:)), "", true);
                let quit = labelled(Label::TrayQuit, Some(sel!(mewrkQuit:)), "", true);
                let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Mewrk"));
                menu.addItem(&open);
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                menu.addItem(&quit);
                target.ivars().status_menu.set(menu);
                target.ivars().status_item.set(status);
                tray_installed = true;
            }
        }
    }
    install_reopen_handler(&application);
    Some(NativeShell { target, labelled_items: labelled_items.into_inner(), labelled_menus, language: chinese, tray_installed })
}

extern "C-unwind" fn should_handle_reopen(_this: &AnyObject, _cmd: Sel, _application: &AnyObject, has_visible_windows: Bool) -> Bool {
    // Opening a running app again (its pinned Dock icon, Finder, Launchpad, Spotlight) is how macOS asks for its window back.
    // Closing the window only hides it into the menu bar, so without this all of those would do nothing at all.
    if !has_visible_windows.as_bool() {
        post(NativeEvent::Show);
    }
    Bool::YES
}

/// winit's application delegate has no `applicationShouldHandleReopen:hasVisibleWindows:`, and AppKit asks whether it
/// responds before sending it, so the delegate is given the method.
fn install_reopen_handler(application: &NSApplication) {
    let Some(delegate) = application.delegate() else { return };
    let object: &AnyObject = delegate.as_ref();
    let class = object.class();
    let selector = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
    if class.responds_to(selector) {
        return;
    }
    let imp: extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool = should_handle_reopen;
    // SAFETY: the encoding matches `imp` (a BOOL from an id and a BOOL), and adding a method a class lacks is what
    // `class_addMethod` is for. A class built by `define_class` is an ordinary class to the runtime.
    unsafe {
        let types = c"B@:@B";
        let added = objc2::ffi::class_addMethod(std::ptr::from_ref(class).cast_mut(), selector, std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool, unsafe extern "C-unwind" fn()>(imp), types.as_ptr());
        let _ = added;
    }
    let _ = std::mem::size_of::<ClassBuilder>();
}

impl NativeShell {
    /// Whether the menu bar item is there: without one the window's close has to quit, or the process would be unreachable.
    pub fn tray_installed(&self) -> bool {
        self.tray_installed
    }

    /// Relabels everything in place when the language changes.
    pub fn sync_language(&mut self, chinese: bool) {
        if self.language == chinese {
            return;
        }
        self.language = chinese;
        for (label, entry) in &self.labelled_items {
            entry.setTitle(&NSString::from_str(label.text(chinese)));
        }
        for (label, menu) in &self.labelled_menus {
            menu.setTitle(&NSString::from_str(label.text(chinese)));
        }
        let _ = &self.target;
    }
}

/// `keyDown:` as winit's view class implemented it before it was wrapped.
static ORIGINAL_KEY_DOWN: OnceLock<usize> = OnceLock::new();

unsafe extern "C-unwind" fn key_down(this: *mut AnyObject, cmd: Sel, event: *mut AnyObject) {
    let Some(&original) = ORIGINAL_KEY_DOWN.get() else { return };
    // SAFETY: only installed on an `NSView` subclass, and AppKit sends `keyDown:` on the main thread, where the view is alive for
    // the duration of the call.
    let view = unsafe { &*(this as *const NSView) };
    let aimed_at_view = view.window().and_then(|window| window.firstResponder()).is_some_and(|responder| Retained::as_ptr(&responder).cast::<AnyObject>() == this);
    if !aimed_at_view {
        return;
    }
    // SAFETY: what `class_replaceMethod` replaced, with this same signature.
    let original: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) = unsafe { std::mem::transmute(original) };
    unsafe { original(this, cmd, event) };
}

/// `app_keys::install`: wraps `keyDown:` of the class of the window's content view, so a key that bubbled up from a descendant (a
/// browser page) is dropped, and one aimed at the view itself still gets winit's handling. Every winit window shares that class,
/// so one call covers them all; later calls do nothing.
pub fn drop_bubbled_keys(view: *mut std::ffi::c_void) -> Result<(), String> {
    if ORIGINAL_KEY_DOWN.get().is_some() {
        return Ok(());
    }
    if view.is_null() {
        return Err("the window has no content view".to_owned());
    }
    // SAFETY: winit's content view, retained by its window, which outlives this call.
    let class: &'static AnyClass = AnyObject::class(unsafe { &*(view as *const AnyObject) });
    // Only an override the class declares itself is replaced: replacing an inherited `keyDown:` would add one where there was none,
    // and on `NSView` would reach every view.
    if !class.instance_methods().iter().any(|method| method.name() == sel!(keyDown:)) {
        return Err(format!("{} has no keyDown: of its own", class.name().to_string_lossy()));
    }
    let original = class.instance_method(sel!(keyDown:)).ok_or("the content view has no keyDown:")?.implementation();
    let _ = ORIGINAL_KEY_DOWN.set(original as usize);
    // SAFETY: the encoding is `keyDown:`'s, and the replacement calls what it replaced.
    unsafe {
        let wrapped: Imp = std::mem::transmute(key_down as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject));
        objc2::ffi::class_replaceMethod(std::ptr::from_ref(class).cast_mut(), sel!(keyDown:), wrapped, c"v@:@".as_ptr());
    }
    Ok(())
}

/// `set_in_dock`: the activation policy, not `TransformProcessType`: it takes effect at once and leaves Cmd+H working.
pub fn set_in_dock(in_dock: bool) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(if in_dock { NSApplicationActivationPolicy::Regular } else { NSApplicationActivationPolicy::Accessory });
}

/// Brings the app forward, which a window alone can't do once the app has been put away.
pub fn activate() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// The system's "reduce motion" setting. Read on start and each time the window comes back: System Settings is another window.
pub fn reduce_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

/// Where the pointer is over the view, in logical points from the top-left, for a drag: AppKit sends the view no moves while
/// files are dragged over it.
pub fn pointer_in_view(view: *mut std::ffi::c_void) -> Option<(f32, f32)> {
    if view.is_null() {
        return None;
    }
    // SAFETY: the view is the live window's content view, read on the main thread, with selectors matching their declarations.
    unsafe {
        let view: *mut AnyObject = view.cast();
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return None;
        }
        let in_window: NSPoint = msg_send![window, mouseLocationOutsideOfEventStream];
        let nil: *mut AnyObject = std::ptr::null_mut();
        let local: NSPoint = msg_send![view, convertPoint: in_window, fromView: nil];
        let flipped: bool = msg_send![view, isFlipped];
        let bounds: objc2_foundation::NSRect = msg_send![view, bounds];
        Some((local.x as f32, if flipped { local.y as f32 } else { (bounds.size.height - local.y) as f32 }))
    }
}

// ---------------------------------------------------------------- accessibility

/// The class AccessKit's own subclass of the content view is: what [`accessible_children`] and [`accessible_hit_test`] call through to.
static ACCESSIBLE_VIEW_SUPER: OnceLock<usize> = OnceLock::new();

fn accessible_view_super() -> &'static AnyClass {
    // SAFETY: set to a class, which lives for ever, before any instance can reach the methods that read it.
    unsafe { &*(*ACCESSIBLE_VIEW_SUPER.get().expect("installed before use") as *const AnyClass) }
}

/// The view's children: AccessKit's tree, then the native views on show on top of it. AccessKit answers with its tree alone, which
/// would leave a browser page as a child view no assistive client could find.
extern "C-unwind" fn accessible_children(this: &AnyObject, _: Sel) -> *mut AnyObject {
    // SAFETY: this method lives on a subclass of the view's own class, whose `accessibilityChildren` answers an array or nil, and it is
    // AppKit asking, on the main thread.
    unsafe {
        let tree: Option<Retained<NSArray<AnyObject>>> = msg_send![super(this, accessible_view_super()), accessibilityChildren];
        let tree = tree.unwrap_or_else(NSArray::new);
        let view = &*(this as *const AnyObject as *const NSView);
        let pages: Vec<Retained<AnyObject>> = view.subviews().to_vec().into_iter().filter(|child| !child.isHiddenOrHasHiddenAncestor()).map(|child| Retained::cast_unchecked(child)).collect();
        if pages.is_empty() {
            return Retained::autorelease_return(tree).cast();
        }
        // A plain container view is nothing to a client: what counts is the elements in it.
        let elements = objc2_app_kit::NSAccessibilityUnignoredChildren(&NSArray::from_retained_slice(&pages));
        let mut all = tree.to_vec();
        all.extend(elements.to_vec());
        Retained::autorelease_return(NSArray::from_retained_slice(&all)).cast()
    }
}

/// A point over a native view is that view's to answer, as it is for the pointer.
extern "C-unwind" fn accessible_hit_test(this: &AnyObject, _: Sel, point: NSPoint) -> *mut AnyObject {
    // SAFETY: as above, with a point in screen coordinates, which is what `accessibilityHitTest:` takes.
    unsafe {
        let view = &*(this as *const AnyObject as *const NSView);
        if let Some(window) = view.window() {
            let local = view.convertPoint_fromView(window.convertPointFromScreen(point), None);
            for child in view.subviews().to_vec().into_iter().rev() {
                let frame = child.frame();
                let inside = local.x >= frame.origin.x && local.x < frame.origin.x + frame.size.width && local.y >= frame.origin.y && local.y < frame.origin.y + frame.size.height;
                if inside && !child.isHiddenOrHasHiddenAncestor() {
                    let hit: *mut AnyObject = msg_send![&*child, accessibilityHitTest: point];
                    let on_window = std::ptr::eq(hit, Retained::as_ptr(&window).cast());
                    if !hit.is_null() && !on_window {
                        return hit;
                    }
                    // Chromium answers from where a page was parked, so for a point on it the window: the page is a better answer.
                    if let Some(page) = objc2_app_kit::NSAccessibilityUnignoredDescendant(&child) {
                        return Retained::autorelease_return(page);
                    }
                }
            }
        }
        msg_send![super(this, accessible_view_super()), accessibilityHitTest: point]
    }
}

/// Keeps native child views reachable for assistive clients once AccessKit has taken over the view. AppKit starts observing a view
/// that is on show and changing the class of an observed view is what breaks it, so like the adapter this comes before the window shows.
pub fn keep_child_views_accessible(view: *mut std::ffi::c_void) -> Result<(), String> {
    if view.is_null() {
        return Err("the window has no content view".to_owned());
    }
    // SAFETY: winit's content view, retained by its window. The new class adds methods and no storage, so the object fits it, and it
    // keeps every method of the class it came from. The signatures match AppKit's, and `add_method` checks the encodings.
    unsafe {
        let object = &*(view as *const AnyObject);
        let current = AnyObject::class(object);
        let mut builder = ClassBuilder::new(c"MewrkAccessibleContentView", current).ok_or("the accessible view class exists already")?;
        builder.add_method(sel!(accessibilityChildren), accessible_children as extern "C-unwind" fn(_, _) -> _);
        builder.add_method(sel!(accessibilityHitTest:), accessible_hit_test as extern "C-unwind" fn(_, _, _) -> _);
        let _ = ACCESSIBLE_VIEW_SUPER.set(current as *const AnyClass as usize);
        AnyObject::set_class(object, builder.register());
    }
    Ok(())
}

/// One element and what is under it, as an assistive client gets them from AppKit, for `--a11y-probe`.
fn describe_element(element: &AnyObject, depth: usize) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("class".to_owned(), element.class().name().to_string_lossy().into_owned().into());
    // SAFETY: `NSAccessibility`, asked only of what says it answers, each answering what is declared here.
    unsafe {
        macro_rules! text {
            ($name:literal, $selector:ident) => {
                if msg_send![element, respondsToSelector: sel!($selector)] {
                    let answer: Option<Retained<AnyObject>> = msg_send![element, $selector];
                    if let Some(answer) = answer {
                        let words: Retained<NSString> = msg_send![&*answer, description];
                        out.insert($name.to_owned(), words.to_string().into());
                    }
                }
            };
        }
        text!("role", accessibilityRole);
        text!("subrole", accessibilitySubrole);
        text!("title", accessibilityTitle);
        text!("label", accessibilityLabel);
        text!("help", accessibilityHelp);
        text!("placeholder", accessibilityPlaceholderValue);
        text!("value", accessibilityValue);
        if msg_send![element, respondsToSelector: sel!(accessibilityFrame)] {
            let frame: objc2_foundation::NSRect = msg_send![element, accessibilityFrame];
            out.insert("frame".to_owned(), serde_json::json!([frame.origin.x, frame.origin.y, frame.size.width, frame.size.height]));
        }
        if msg_send![element, respondsToSelector: sel!(isAccessibilityFocused)] {
            let focused: bool = msg_send![element, isAccessibilityFocused];
            out.insert("focused".to_owned(), focused.into());
        }
        if msg_send![element, respondsToSelector: sel!(isAccessibilityEnabled)] {
            let enabled: bool = msg_send![element, isAccessibilityEnabled];
            out.insert("enabled".to_owned(), enabled.into());
        }
        if depth < 40 && msg_send![element, respondsToSelector: sel!(accessibilityChildren)] {
            let children: Option<Retained<NSArray<AnyObject>>> = msg_send![element, accessibilityChildren];
            if let Some(children) = children {
                out.insert("children".to_owned(), children.to_vec().iter().map(|child| describe_element(child, depth + 1)).collect::<Vec<_>>().into());
            }
        }
    }
    serde_json::Value::Object(out)
}

/// The window as an assistive client finds it, and how a press finds the native views on it, for `--a11y-probe`.
pub fn describe_accessibility(view: *mut std::ffi::c_void) -> serde_json::Value {
    // SAFETY: the live window's content view, on the main thread.
    let view = unsafe { &*(view as *const NSView) };
    let Some(window) = view.window() else { return serde_json::Value::Null };
    let class_of = |object: &AnyObject| object.class().name().to_string_lossy().into_owned();
    let native: Vec<serde_json::Value> = view
        .subviews()
        .to_vec()
        .iter()
        .map(|child| {
            let frame = child.frame();
            let middle = NSPoint::new(frame.origin.x + frame.size.width * 0.5, frame.origin.y + frame.size.height * 0.5);
            // `hitTest:` takes a point in the superview's coordinates, and it is how a press finds its view.
            // SAFETY: the superview of a view on show, read on the main thread.
            let in_parent = view.convertPoint_toView(middle, unsafe { view.superview() }.as_deref());
            let press_lands_on = view.hitTest(in_parent).map(|hit| class_of(&hit));
            // And how a client asking about a point on screen finds it.
            let on_screen = window.convertPointToScreen(view.convertPoint_toView(middle, None));
            // SAFETY: `accessibilityHitTest:` of an `NSView`, which answers an element or nil.
            let reader_lands_on: Option<Retained<AnyObject>> = unsafe { msg_send![view, accessibilityHitTest: on_screen] };
            serde_json::json!({ "class": class_of(child), "hidden": child.isHiddenOrHasHiddenAncestor(), "frame": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height], "press_lands_on": press_lands_on, "reader_lands_on": reader_lands_on.map(|hit| class_of(&hit)) })
        })
        .collect();
    // SAFETY: `accessibilityChildren` of an `NSView` and an `NSWindow`, each answering an array or nil.
    let (of_view, of_window): (Option<Retained<NSArray<AnyObject>>>, Option<Retained<NSArray<AnyObject>>>) = unsafe { (msg_send![view, accessibilityChildren], msg_send![&*window, accessibilityChildren]) };
    let summary = |children: Option<Retained<NSArray<AnyObject>>>| -> Vec<serde_json::Value> { children.map_or(Vec::new(), |children| children.to_vec().iter().map(|child| describe_element(child, 0)).collect()) };
    serde_json::json!({
        "view_class": class_of(view),
        "first_responder": window.firstResponder().map(|responder| class_of(&responder)),
        "native_views": native,
        "view_children": summary(of_view),
        "window_children": summary(of_window).into_iter().map(|child| serde_json::json!({ "class": child["class"], "role": child["role"], "subrole": child["subrole"], "children": child["children"].as_array().map_or(0, Vec::len) })).collect::<Vec<_>>(),
    })
}

/// Finds the element with this title or placeholder and does `act` to it, as an assistive client would, for `--a11y-probe`. Answers
/// whether it was there.
pub fn act_on_accessibility(view: *mut std::ffi::c_void, words: &str, act: impl FnOnce(&AnyObject)) -> bool {
    fn find(element: &AnyObject, words: &str, depth: usize) -> Option<Retained<AnyObject>> {
        // SAFETY: as in `describe_element`.
        unsafe {
            let title: Option<Retained<NSString>> = if msg_send![element, respondsToSelector: sel!(accessibilityTitle)] { msg_send![element, accessibilityTitle] } else { None };
            let placeholder: Option<Retained<NSString>> = if msg_send![element, respondsToSelector: sel!(accessibilityPlaceholderValue)] { msg_send![element, accessibilityPlaceholderValue] } else { None };
            if title.is_some_and(|title| title.to_string() == words) || placeholder.is_some_and(|placeholder| placeholder.to_string() == words) {
                return Some(element.retain());
            }
            if depth > 40 || !msg_send![element, respondsToSelector: sel!(accessibilityChildren)] {
                return None;
            }
            let children: Option<Retained<NSArray<AnyObject>>> = msg_send![element, accessibilityChildren];
            children?.to_vec().iter().find_map(|child| find(child, words, depth + 1))
        }
    }
    // SAFETY: the live window's content view, on the main thread.
    match find(unsafe { &*(view as *const AnyObject) }, words, 0) {
        Some(element) => {
            act(&element);
            true
        }
        None => false,
    }
}

/// A real click in the middle of an element and real keys, through `sendEvent:` as AppKit delivers them, for `--a11y-probe`: what proves the
/// pointer and the keyboard still get where they went before AccessKit took the view over.
pub fn click_element(view: *mut std::ffi::c_void, element: &AnyObject, kinds: &[NSEventType]) {
    // SAFETY: the live window's content view and an element it just answered with, on the main thread.
    unsafe {
        let Some(window) = (*(view as *const NSView)).window() else { return };
        let frame: objc2_foundation::NSRect = msg_send![element, accessibilityFrame];
        post_mouse(&window, window.convertPointFromScreen(NSPoint::new(frame.origin.x + frame.size.width * 0.5, frame.origin.y + frame.size.height * 0.5)), kinds);
    }
}

/// The same at a point in the view's own coordinates, for what cannot say where it is: Chromium's elements answer from where a page was parked.
pub fn click_view_point(view: *mut std::ffi::c_void, x: f64, y: f64) {
    // SAFETY: as above.
    unsafe {
        let view = &*(view as *const NSView);
        let Some(window) = view.window() else { return };
        post_mouse(&window, view.convertPoint_toView(NSPoint::new(x, y), None), &[NSEventType::LeftMouseDown, NSEventType::LeftMouseUp]);
    }
}

fn post_mouse(window: &objc2_app_kit::NSWindow, location: NSPoint, kinds: &[NSEventType]) {
    let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("the main thread"));
    for kind in kinds.iter().copied() {
        if let Some(event) = objc2_app_kit::NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(kind, location, objc2_app_kit::NSEventModifierFlags::empty(), 0.0, window.windowNumber(), None, 0, 1, 1.0) {
            app.sendEvent(&event);
        }
    }
}

/// See [`click_element`].
pub fn type_keys(view: *mut std::ffi::c_void, words: &str) {
    // SAFETY: as in `click_element`.
    unsafe {
        let view = &*(view as *const NSView);
        let Some(window) = view.window() else { return };
        let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("the main thread"));
        for c in words.chars() {
            let c = NSString::from_str(&c.to_string());
            for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
                if let Some(event) = objc2_app_kit::NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(kind, NSPoint::new(0.0, 0.0), objc2_app_kit::NSEventModifierFlags::empty(), 0.0, window.windowNumber(), None, &c, &c, false, 0) {
                    app.sendEvent(&event);
                }
            }
        }
    }
}
