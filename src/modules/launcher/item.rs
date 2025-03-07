use super::open_state::OpenState;
use crate::clients::wayland::ToplevelHandle;
use crate::image::ImageProvider;
use crate::modules::launcher::{ItemEvent, LauncherUpdate};
use crate::modules::ModuleUpdateEvent;
use crate::popup::Popup;
use crate::{read_lock, try_send};
use color_eyre::{Report, Result};
use gtk::prelude::*;
use gtk::{Button, IconTheme, Orientation};
use indexmap::IndexMap;
use std::rc::Rc;
use std::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tracing::error;
use wayland_client::protocol::wl_seat::WlSeat;

#[derive(Debug, Clone)]
pub struct Item {
    pub app_id: String,
    pub favorite: bool,
    pub open_state: OpenState,
    pub windows: IndexMap<usize, Window>,
    pub name: String,
}

impl Item {
    pub fn new(app_id: String, open_state: OpenState, favorite: bool) -> Self {
        Self {
            app_id,
            favorite,
            open_state,
            windows: IndexMap::new(),
            name: String::new(),
        }
    }

    /// Merges the provided node into this launcher item
    pub fn merge_toplevel(&mut self, handle: ToplevelHandle) -> Result<Window> {
        let info = handle
            .info()
            .ok_or_else(|| Report::msg("Toplevel is missing associated info"))?;

        let id = info.id;

        if self.windows.is_empty() {
            self.name = info.title;
        }

        let window = Window::try_from(handle)?;
        self.windows.insert(id, window.clone());

        self.recalculate_open_state();

        Ok(window)
    }

    pub fn unmerge_toplevel(&mut self, handle: &ToplevelHandle) {
        if let Some(info) = handle.info() {
            self.windows.remove(&info.id);
            self.recalculate_open_state();
        }
    }

    pub fn set_window_name(&mut self, window_id: usize, name: String) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            if let OpenState::Open { focused: true, .. } = window.open_state {
                self.name = name.clone();
            }

            window.name = name;
        }
    }

    pub fn set_window_focused(&mut self, window_id: usize, focused: bool) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.open_state =
                OpenState::merge_states(&[&window.open_state, &OpenState::focused(focused)]);

            self.recalculate_open_state();
        }
    }

    /// Sets this item's open state
    /// to the merged result of its windows' open states
    fn recalculate_open_state(&mut self) {
        let new_state = OpenState::merge_states(
            &self
                .windows
                .iter()
                .map(|(_, win)| &win.open_state)
                .collect::<Vec<_>>(),
        );
        self.open_state = new_state;
    }
}

impl TryFrom<ToplevelHandle> for Item {
    type Error = Report;

    fn try_from(handle: ToplevelHandle) -> std::result::Result<Self, Self::Error> {
        let info = handle
            .info()
            .ok_or_else(|| Report::msg("Toplevel is missing associated info"))?;

        let name = info.title.clone();
        let app_id = info.app_id.clone();
        let open_state = OpenState::from(&info);

        let mut windows = IndexMap::new();
        let window = Window::try_from(handle)?;
        windows.insert(info.id, window);

        Ok(Self {
            app_id,
            favorite: false,
            open_state,
            windows,
            name,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Window {
    pub id: usize,
    pub name: String,
    pub open_state: OpenState,
    handle: ToplevelHandle,
}

impl TryFrom<ToplevelHandle> for Window {
    type Error = Report;

    fn try_from(handle: ToplevelHandle) -> Result<Self, Self::Error> {
        let info = handle
            .info()
            .ok_or_else(|| Report::msg("Toplevel is missing associated info"))?;
        let open_state = OpenState::from(&info);

        Ok(Self {
            id: info.id,
            name: info.title,
            open_state,
            handle,
        })
    }
}

impl Window {
    pub fn focus(&self, seat: &WlSeat) {
        self.handle.focus(seat);
    }
}

pub struct MenuState {
    pub num_windows: usize,
}

pub struct ItemButton {
    pub button: Button,
    pub persistent: bool,
    pub show_names: bool,
    pub menu_state: Rc<RwLock<MenuState>>,
}

#[derive(Clone, Copy)]
pub struct AppearanceOptions {
    pub show_names: bool,
    pub show_icons: bool,
    pub icon_size: i32,
}

impl ItemButton {
    pub fn new(
        item: &Item,
        appearance: AppearanceOptions,
        icon_theme: &IconTheme,
        orientation: Orientation,
        tx: &Sender<ModuleUpdateEvent<LauncherUpdate>>,
        controller_tx: &Sender<ItemEvent>,
    ) -> Self {
        let mut button = Button::builder();

        if appearance.show_names {
            button = button.label(&item.name);
        }

        let button = button.build();

        if appearance.show_icons {
            let gtk_image = gtk::Image::new();
            let image =
                ImageProvider::parse(&item.app_id.clone(), icon_theme, appearance.icon_size);
            if let Some(image) = image {
                button.set_image(Some(&gtk_image));
                button.set_always_show_image(true);

                if let Err(err) = image.load_into_image(gtk_image) {
                    error!("{err:?}");
                }
            };
        }

        let style_context = button.style_context();
        style_context.add_class("item");

        if item.favorite {
            style_context.add_class("favorite");
        }
        if item.open_state.is_open() {
            style_context.add_class("open");
        }
        if item.open_state.is_focused() {
            style_context.add_class("focused");
        }

        {
            let app_id = item.app_id.clone();
            let tx = controller_tx.clone();
            button.connect_clicked(move |button| {
                // lazy check :| TODO: Improve this
                let style_context = button.style_context();
                if style_context.has_class("open") {
                    try_send!(tx, ItemEvent::FocusItem(app_id.clone()));
                } else {
                    try_send!(tx, ItemEvent::OpenItem(app_id.clone()));
                }
            });
        }

        let menu_state = Rc::new(RwLock::new(MenuState {
            num_windows: item.windows.len(),
        }));

        {
            let app_id = item.app_id.clone();
            let tx = tx.clone();
            let menu_state = menu_state.clone();

            button.connect_enter_notify_event(move |button, _| {
                let menu_state = read_lock!(menu_state);

                if menu_state.num_windows > 1 {
                    try_send!(
                        tx,
                        ModuleUpdateEvent::Update(LauncherUpdate::Hover(app_id.clone(),))
                    );

                    try_send!(
                        tx,
                        ModuleUpdateEvent::OpenPopup(Popup::widget_geometry(button, orientation))
                    );
                } else {
                    try_send!(tx, ModuleUpdateEvent::ClosePopup);
                }

                Inhibit(false)
            });
        }

        button.show_all();

        Self {
            button,
            persistent: item.favorite,
            show_names: appearance.show_names,
            menu_state,
        }
    }

    pub fn set_open(&self, open: bool) {
        self.update_class("open", open);

        if !open {
            self.set_focused(false);
        }
    }

    pub fn set_focused(&self, focused: bool) {
        self.update_class("focused", focused);
    }

    /// Adds or removes a class to the button based on `toggle`.
    fn update_class(&self, class: &str, toggle: bool) {
        let style_context = self.button.style_context();

        if toggle {
            style_context.add_class(class);
        } else {
            style_context.remove_class(class);
        }
    }
}
