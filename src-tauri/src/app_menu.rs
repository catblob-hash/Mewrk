//! The macOS menu bar, worded in the app language, with a Quit that runs the
//! exit barrier.
//!
//! Tauri's default menu quits through `-[NSApplication terminate:]`, and tao
//! never asks the application first: there is no `applicationShouldTerminate:`,
//! so `applicationWillTerminate:` arrives as `RunEvent::Exit` and the process
//! ends whatever the handler does. Cmd+Q therefore skipped the flushes and the
//! "save failed, keep the application open" answer that the tray's Quit and a
//! window close get from `request_deferred_exit_with_barrier`. Quit here is an
//! ordinary item that asks the barrier instead. Quits that bypass every menu
//! (Dock → Quit, logout) still arrive as a bare `RunEvent::Exit`;
//! `finalize_app_shutdown` covers those.
//!
//! The rest is Tauri's default menu rebuilt item by item, because Tauri and
//! muda word every default item in English: with Chinese chosen, the bar read
//! File, Edit and View beside 退出 Mewrk. Each item is created with its label
//! in the app language and kept, so a language change relabels the whole bar
//! in place.

use tauri::{
    menu::{
        AboutMetadata, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu, HELP_SUBMENU_ID,
        WINDOW_SUBMENU_ID,
    },
    AppHandle, Manager, Wry,
};

use crate::model::ResolvedLanguage;
use crate::ui_text;

const QUIT_ID: &str = "app-menu-quit";
const SETTINGS_ID: &str = "app-menu-settings";

/// Every worded entry of the menu bar. The application menu's own title is
/// not one: macOS always shows the app's name there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Label {
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
}

impl Label {
    #[cfg(test)]
    const ALL: [Self; 21] = [
        Self::About,
        Self::Settings,
        Self::Services,
        Self::Hide,
        Self::HideOthers,
        Self::Quit,
        Self::File,
        Self::CloseWindow,
        Self::Edit,
        Self::Undo,
        Self::Redo,
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::SelectAll,
        Self::View,
        Self::FullScreen,
        Self::Window,
        Self::Minimize,
        Self::Zoom,
        Self::Help,
    ];

    /// The words macOS itself uses for these items in each language.
    fn text(self, language: ResolvedLanguage) -> &'static str {
        let (zh, en) = match self {
            Self::About => ("关于 Mewrk", "About Mewrk"),
            Self::Settings => ("设置…", "Settings…"),
            Self::Services => ("服务", "Services"),
            Self::Hide => ("隐藏 Mewrk", "Hide Mewrk"),
            Self::HideOthers => ("隐藏其他", "Hide Others"),
            Self::Quit => ("退出 Mewrk", "Quit Mewrk"),
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
        };
        ui_text::pick_for(language, zh, en)
    }
}

/// One worded item, of whichever kind it was created as.
enum Labelled {
    Submenu(Submenu<Wry>),
    Predefined(PredefinedMenuItem<Wry>),
    Item(MenuItem<Wry>),
}

impl Labelled {
    fn set_text(&self, text: &str) -> tauri::Result<()> {
        match self {
            Self::Submenu(submenu) => submenu.set_text(text),
            Self::Predefined(item) => item.set_text(text),
            Self::Item(item) => item.set_text(text),
        }
    }
}

/// The worded items, kept so a language change can relabel them in place.
pub(crate) struct AppMenu {
    labelled: Vec<(Label, Labelled)>,
}

/// Creates each item worded in one language and remembers it.
struct Builder<'a> {
    app: &'a AppHandle,
    language: ResolvedLanguage,
    labelled: Vec<(Label, Labelled)>,
}

impl Builder<'_> {
    fn predefined(
        &mut self,
        label: Label,
        create: impl FnOnce(&AppHandle, Option<&str>) -> tauri::Result<PredefinedMenuItem<Wry>>,
    ) -> tauri::Result<PredefinedMenuItem<Wry>> {
        let item = create(self.app, Some(label.text(self.language)))?;
        self.labelled.push((label, Labelled::Predefined(item.clone())));
        Ok(item)
    }

    fn submenu(
        &mut self,
        label: Label,
        id: Option<&str>,
        items: &[&dyn IsMenuItem<Wry>],
    ) -> tauri::Result<Submenu<Wry>> {
        let text = label.text(self.language);
        let submenu = match id {
            Some(id) => Submenu::with_id_and_items(self.app, id, text, true, items)?,
            None => Submenu::with_items(self.app, text, true, items)?,
        };
        self.labelled.push((label, Labelled::Submenu(submenu.clone())));
        Ok(submenu)
    }
}

/// Installs the menu. Must run on the main thread (Tauri's `setup` does).
/// `on_settings` answers the application menu's Settings… (⌘,), which every
/// Mac app has; the shortcut never reaches the page once the menu claims it.
pub(crate) fn install(
    app: &AppHandle,
    language: ResolvedLanguage,
    on_quit: impl Fn(&AppHandle) + Send + Sync + 'static,
    on_settings: impl Fn(&AppHandle) + Send + Sync + 'static,
) -> Result<(), String> {
    let failed = |error: tauri::Error| format!("无法创建应用菜单: {error}");
    let mut builder = Builder {
        app,
        language,
        labelled: Vec::new(),
    };
    let settings = MenuItem::with_id(
        app,
        SETTINGS_ID,
        Label::Settings.text(language),
        true,
        Some("CmdOrCtrl+,"),
    )
    .map_err(failed)?;
    builder.labelled.push((Label::Settings, Labelled::Item(settings.clone())));
    let (menu, application) = build(&mut builder, &settings).map_err(failed)?;
    let quit = MenuItem::with_id(
        app,
        QUIT_ID,
        Label::Quit.text(language),
        true,
        Some("CmdOrCtrl+Q"),
    )
    .map_err(failed)?;
    application.append(&quit).map_err(failed)?;
    builder.labelled.push((Label::Quit, Labelled::Item(quit)));
    app.set_menu(menu).map_err(failed)?;
    app.on_menu_event(move |app, event| match event.id().as_ref() {
        QUIT_ID => on_quit(app),
        SETTINGS_ID => on_settings(app),
        _ => {}
    });
    app.manage(AppMenu {
        labelled: builder.labelled,
    });
    Ok(())
}

/// Tauri's default macOS menu (`Menu::default`), each item worded by
/// `builder`, with `settings` under About as macOS places it, and the
/// application menu, which still lacks its Quit.
fn build(
    builder: &mut Builder<'_>,
    settings: &MenuItem<Wry>,
) -> tauri::Result<(Menu<Wry>, Submenu<Wry>)> {
    let app = builder.app;
    let package = app.package_info();
    let config = app.config();
    let about_metadata = AboutMetadata {
        name: Some(package.name.clone()),
        version: Some(package.version.to_string()),
        copyright: config.bundle.copyright.clone(),
        authors: config.bundle.publisher.clone().map(|publisher| vec![publisher]),
        ..Default::default()
    };

    let about = builder.predefined(Label::About, |app, text| {
        PredefinedMenuItem::about(app, text, Some(about_metadata))
    })?;
    let services = builder.predefined(Label::Services, PredefinedMenuItem::services)?;
    let hide = builder.predefined(Label::Hide, PredefinedMenuItem::hide)?;
    let hide_others = builder.predefined(Label::HideOthers, PredefinedMenuItem::hide_others)?;
    let application = Submenu::with_items(
        app,
        package.name.clone(),
        true,
        &[
            &about,
            &PredefinedMenuItem::separator(app)?,
            settings,
            &PredefinedMenuItem::separator(app)?,
            &services,
            &PredefinedMenuItem::separator(app)?,
            &hide,
            &hide_others,
            &PredefinedMenuItem::separator(app)?,
        ],
    )?;

    let close_window = builder.predefined(Label::CloseWindow, PredefinedMenuItem::close_window)?;
    let file = builder.submenu(Label::File, None, &[&close_window])?;

    let undo = builder.predefined(Label::Undo, PredefinedMenuItem::undo)?;
    let redo = builder.predefined(Label::Redo, PredefinedMenuItem::redo)?;
    let cut = builder.predefined(Label::Cut, PredefinedMenuItem::cut)?;
    let copy = builder.predefined(Label::Copy, PredefinedMenuItem::copy)?;
    let paste = builder.predefined(Label::Paste, PredefinedMenuItem::paste)?;
    let select_all = builder.predefined(Label::SelectAll, PredefinedMenuItem::select_all)?;
    let edit = builder.submenu(
        Label::Edit,
        None,
        &[
            &undo,
            &redo,
            &PredefinedMenuItem::separator(app)?,
            &cut,
            &copy,
            &paste,
            &select_all,
        ],
    )?;

    let full_screen = builder.predefined(Label::FullScreen, PredefinedMenuItem::fullscreen)?;
    let view = builder.submenu(Label::View, None, &[&full_screen])?;

    let minimize = builder.predefined(Label::Minimize, PredefinedMenuItem::minimize)?;
    let zoom = builder.predefined(Label::Zoom, PredefinedMenuItem::maximize)?;
    let close = builder.predefined(Label::CloseWindow, PredefinedMenuItem::close_window)?;
    // The ids make macOS treat these as the Window and Help menus (the window
    // list, the Help search field), as it does for Tauri's own.
    let window = builder.submenu(
        Label::Window,
        Some(WINDOW_SUBMENU_ID),
        &[&minimize, &zoom, &PredefinedMenuItem::separator(app)?, &close],
    )?;
    let help = builder.submenu(Label::Help, Some(HELP_SUBMENU_ID), &[])?;

    let menu = Menu::with_items(app, &[&application, &file, &edit, &view, &window, &help])?;
    Ok((menu, application))
}

/// Rewords the menu bar for `language`. A no-op until `install` succeeded.
/// Posted to the main thread rather than awaited, like the tray's relabel.
pub(crate) fn apply_language(app: &AppHandle, language: ResolvedLanguage) {
    if app.try_state::<AppMenu>().is_none() {
        return;
    }
    let handle = app.clone();
    let posted = app.run_on_main_thread(move || {
        let Some(menu) = handle.try_state::<AppMenu>() else {
            return;
        };
        for (label, item) in &menu.labelled {
            if let Err(error) = item.set_text(label.text(language)) {
                eprintln!("无法更新应用菜单文字：{error}");
            }
        }
    });
    if let Err(error) = posted {
        eprintln!("无法调度应用菜单文字更新：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{Label, ResolvedLanguage};

    #[test]
    fn quit_follows_the_resolved_application_language() {
        assert_eq!(Label::Quit.text(ResolvedLanguage::ZhCn), "退出 Mewrk");
        assert_eq!(Label::Quit.text(ResolvedLanguage::EnUs), "Quit Mewrk");
    }

    #[test]
    fn every_menu_bar_entry_is_worded_in_both_languages() {
        for label in Label::ALL {
            let chinese = label.text(ResolvedLanguage::ZhCn);
            let english = label.text(ResolvedLanguage::EnUs);
            assert!(
                chinese.chars().any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character)),
                "{label:?} 的中文是 {chinese}"
            );
            // macOS writes an item that opens a window with an ellipsis.
            assert!(
                !english.is_empty() && english.chars().all(|character| character.is_ascii() || character == '…'),
                "{label:?}: {english}"
            );
        }
        assert_eq!(Label::File.text(ResolvedLanguage::ZhCn), "文件");
        assert_eq!(Label::Edit.text(ResolvedLanguage::ZhCn), "编辑");
        assert_eq!(Label::View.text(ResolvedLanguage::ZhCn), "显示");
        assert_eq!(Label::Window.text(ResolvedLanguage::ZhCn), "窗口");
        assert_eq!(Label::Help.text(ResolvedLanguage::ZhCn), "帮助");
    }
}
