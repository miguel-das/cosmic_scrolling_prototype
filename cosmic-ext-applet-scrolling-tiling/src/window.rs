// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    fl,
    layout::{LayoutTransition, WorkspaceLayoutMode, derive_layout_mode, plan_layout_transition},
    wayland::AppRequest,
    wayland_subscription,
    wayland_subscription::WorkspacesUpdate,
};
use cctk::sctk::reexports::calloop::channel::SyncSender;
use cosmic::{
    Element, Task,
    app::{self, Core},
    applet::{menu_button, padded_control},
    cosmic_config::{Config, ConfigSet, CosmicConfigEntry},
    cosmic_theme::Spacing,
    iced::widget::{column, row},
    iced::{
        Length, Limits, Subscription,
        platform_specific::shell::wayland::commands::popup::destroy_popup, window::Id,
    },
    theme,
    widget::{
        container, divider,
        segmented_button::{self, Entity, SingleSelectModel},
        segmented_control, text, toggler,
    },
};
use cosmic_comp_config::{CosmicCompConfig, TileBehavior};
use cosmic_protocols::workspace::v2::client::zcosmic_workspace_handle_v2::TilingState;
use std::thread;
use tracing::error;

#[cfg(not(feature = "parallel-test-install"))]
const ID: &str = "com.system76.CosmicAppletTiling";
#[cfg(feature = "parallel-test-install")]
const ID: &str = "com.system76.CosmicAppletWindowLayoutTest";

#[cfg(not(feature = "parallel-test-install"))]
const ON: &str = "com.system76.CosmicAppletTiling.On";
#[cfg(feature = "parallel-test-install")]
const ON: &str = "com.system76.CosmicAppletWindowLayoutTest.On";

#[cfg(not(feature = "parallel-test-install"))]
const OFF: &str = "com.system76.CosmicAppletTiling.Off";
#[cfg(feature = "parallel-test-install")]
const OFF: &str = "com.system76.CosmicAppletWindowLayoutTest.Off";

#[cfg(not(feature = "parallel-test-install"))]
const SCROLLING: &str = "com.system76.CosmicAppletTiling.Scrolling";
#[cfg(feature = "parallel-test-install")]
const SCROLLING: &str = "com.system76.CosmicAppletWindowLayoutTest.Scrolling";

#[derive(Clone, Copy)]
struct LayoutEntities {
    floating: Entity,
    tiling: Entity,
    scrolling: Entity,
}

impl LayoutEntities {
    fn mode(self, entity: Entity) -> Option<WorkspaceLayoutMode> {
        if entity == self.floating {
            Some(WorkspaceLayoutMode::Floating)
        } else if entity == self.tiling {
            Some(WorkspaceLayoutMode::Tiling)
        } else if entity == self.scrolling {
            Some(WorkspaceLayoutMode::Scrolling)
        } else {
            None
        }
    }

    fn entity(self, mode: WorkspaceLayoutMode) -> Entity {
        match mode {
            WorkspaceLayoutMode::Floating => self.floating,
            WorkspaceLayoutMode::Tiling => self.tiling,
            WorkspaceLayoutMode::Scrolling => self.scrolling,
        }
    }
}

pub struct Window {
    core: Core,
    popup: Option<Id>,
    config: CosmicCompConfig,
    config_helper: Config,
    current_workspace_layout_model: segmented_button::SingleSelectModel,
    current_workspace_layout_entities: LayoutEntities,
    new_workspace_behavior_model: segmented_button::SingleSelectModel,
    new_workspace_entity: Entity,
    /// may not match the config value if behavior is per-workspace
    autotiled: bool,
    layout_change_pending: bool,
    workspace_tx: Option<SyncSender<AppRequest>>,
}

#[derive(Clone, Debug)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    CurrentWorkspaceLayout(Entity),
    LayoutChanged(Result<LayoutTransition, String>),
    ToggleActiveHint(bool),
    MyConfigUpdate(Box<CosmicCompConfig>),
    WorkspaceUpdate(WorkspacesUpdate),
    NewWorkspace(Entity),
    OpenSettings,
}

impl cosmic::Application for Window {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let config_helper =
            Config::new("com.system76.CosmicComp", CosmicCompConfig::VERSION).unwrap();
        let mut config = CosmicCompConfig::get_entry(&config_helper).unwrap_or_else(|(errs, c)| {
            for err in errs {
                error!(?err, "Error loading config");
            }
            c
        });

        // Global is removed in favor of per-workspace
        if let Err(err) = config.set_autotile_behavior(&config_helper, TileBehavior::PerWorkspace) {
            error!(?err, "Failed to set autotile behavior to PerWorkspace");
        }

        let mut current_workspace_layout_model = SingleSelectModel::default();
        let current_workspace_layout_entities = LayoutEntities {
            floating: current_workspace_layout_model
                .insert()
                .text(fl!("floating"))
                .id(),
            tiling: current_workspace_layout_model
                .insert()
                .text(fl!("tiling"))
                .id(),
            scrolling: current_workspace_layout_model
                .insert()
                .text(fl!("scrolling"))
                .id(),
        };
        current_workspace_layout_model.activate(
            current_workspace_layout_entities
                .entity(derive_layout_mode(config.autotile, config.tiling_engine)),
        );

        let mut new_workspace_behavior_model = SingleSelectModel::default();
        let new_workspace_entity = new_workspace_behavior_model
            .insert()
            .text(fl!("tiled"))
            .id();
        let floating = new_workspace_behavior_model
            .insert()
            .text(fl!("floating"))
            .id();
        new_workspace_behavior_model.activate(if config.autotile {
            new_workspace_entity
        } else {
            floating
        });

        let window = Self {
            core,
            popup: None,
            autotiled: config.autotile,
            layout_change_pending: false,
            config,
            config_helper,
            current_workspace_layout_model,
            current_workspace_layout_entities,
            new_workspace_behavior_model,
            new_workspace_entity,
            workspace_tx: None,
        };
        (window, Task::none())
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch([
            self.core
                .watch_config::<CosmicCompConfig>("com.system76.CosmicComp")
                .map(|u| Message::MyConfigUpdate(Box::new(u.config))),
            wayland_subscription::workspaces().map(Message::WorkspaceUpdate),
        ])
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::WorkspaceUpdate(msg) => match msg {
                WorkspacesUpdate::State(state) => {
                    self.autotiled = matches!(state, TilingState::TilingEnabled);
                    self.sync_current_workspace_layout();
                }
                WorkspacesUpdate::Started(tx) => {
                    self.workspace_tx = Some(tx);
                }
                WorkspacesUpdate::Errored => {
                    self.workspace_tx = None;
                    error!("Workspaces subscription failed...");
                }
            },
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                        |_| Default::default(),
                        |app: &mut Self| {
                            let new_id = Id::unique();
                            app.popup = Some(new_id);
                            let mut settings = app.core.applet.get_popup_settings(
                                app.core.main_window_id().unwrap(),
                                new_id,
                                Some((1, 1)),
                                None,
                                None,
                            );
                            settings.positioner.size_limits = layout_popup_limits();
                            settings
                        },
                        None,
                    ));
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::CurrentWorkspaceLayout(entity) => {
                let Some(mode) = self.current_workspace_layout_entities.mode(entity) else {
                    error!("Unknown current-workspace layout entity");
                    return Task::none();
                };
                return self.set_current_workspace_layout(mode);
            }
            Message::LayoutChanged(result) => {
                self.layout_change_pending = false;
                match result {
                    Ok(transition) => {
                        if let Some(engine) = transition.tiling_engine {
                            self.config.tiling_engine = engine;
                        }
                        if let Some(tiled) = transition.workspace_tiled {
                            let state = if tiled {
                                TilingState::TilingEnabled
                            } else {
                                TilingState::FloatingOnly
                            };
                            if let Some(tx) = &self.workspace_tx {
                                if let Err(err) = tx.try_send(AppRequest::TilingState(state)) {
                                    error!(?err, "Failed to request workspace layout");
                                }
                            }
                        }
                    }
                    Err(err) => error!(%err, "Failed to change tiling engine"),
                }
                // Workspace state is confirmed by the protocol subscription.
                // A failed write must never leave an optimistic selection behind.
                self.sync_current_workspace_layout();
            }
            Message::ToggleActiveHint(toggled) => {
                self.config.active_hint = toggled;

                let helper = self.config_helper.clone();
                thread::spawn(move || {
                    if let Err(err) = helper.set("active_hint", toggled) {
                        error!(?err, "Failed to set active_hint {toggled}");
                    }
                });
            }
            Message::MyConfigUpdate(c) => {
                if c.autotile != self.config.autotile {
                    self.new_workspace_behavior_model
                        .activate_position(if c.autotile { 0 } else { 1 });
                }

                self.config = *c;
                self.sync_current_workspace_layout();
            }
            Message::NewWorkspace(e) => {
                let autotile_new = self.new_workspace_entity == e;
                self.config.autotile = autotile_new;
                self.new_workspace_behavior_model.activate(e);
                // set the config autotile behavior
                let helper = self.config_helper.clone();

                if let Some(tx) = self.workspace_tx.as_ref() {
                    let state = if autotile_new {
                        TilingState::TilingEnabled
                    } else {
                        TilingState::FloatingOnly
                    };

                    if let Err(err) = tx.try_send(AppRequest::DefaultBehavior(state)) {
                        error!("Failed to send the tiling state update. {err:?}");
                    }
                }

                thread::spawn(move || {
                    if let Err(err) = helper.set("autotile", autotile_new) {
                        error!(?err, "Failed to set autotile {autotile_new:?}");
                    }
                });
            }
            Message::OpenSettings => {
                let mut cmd = std::process::Command::new("cosmic-settings");
                cmd.arg("window-management");
                tokio::spawn(cosmic::process::spawn(cmd));
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let icon = match self.current_workspace_layout() {
            WorkspaceLayoutMode::Floating => OFF,
            WorkspaceLayoutMode::Tiling => ON,
            WorkspaceLayoutMode::Scrolling => SCROLLING,
        };
        self.core
            .applet
            .icon_button(icon)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let Spacing {
            space_xxxs,
            space_xxs,
            space_s,
            ..
        } = theme::active().cosmic().spacing;

        let new_workspace_behavior_button =
            segmented_control::horizontal(&self.new_workspace_behavior_model)
                .on_activate(Message::NewWorkspace);
        let mut current_workspace_layout_button =
            segmented_control::horizontal(&self.current_workspace_layout_model);
        if !self.layout_change_pending && self.workspace_tx.is_some() {
            current_workspace_layout_button =
                current_workspace_layout_button.on_activate(Message::CurrentWorkspaceLayout);
        }
        let content_list = column![
            padded_control(container(
                column![
                    text::body(fl!("current-workspace-layout")),
                    current_workspace_layout_button,
                    text::caption(fl!("tiling-engine-global")),
                ]
                .spacing(space_xxxs)
            )),
            padded_control(divider::horizontal::default()).padding([space_xxs, space_s]),
            padded_control(
                column![
                    text::body(fl!("new-workspace")),
                    new_workspace_behavior_button,
                ]
                .spacing(space_xxxs)
            ),
            padded_control(divider::horizontal::default()).padding([space_xxs, space_s]),
            padded_control(row!(
                text::body(fl!("navigate-windows")).width(Length::Fill),
                text::body(format!("{} + {}", fl!("super"), fl!("arrow-keys"))),
            )),
            padded_control(row!(
                text::body(fl!("move-window")).width(Length::Fill),
                text::body(format!(
                    "{} + {} + {}",
                    fl!("shift"),
                    fl!("super"),
                    fl!("arrow-keys")
                )),
            )),
            padded_control(row!(
                text::body(fl!("toggle-floating-window")).width(Length::Fill),
                text::body(format!("{} + G", fl!("super"))),
            )),
            padded_control(divider::horizontal::default()).padding([space_xxs, space_s]),
            padded_control(
                toggler(self.config.active_hint)
                    .on_toggle(Message::ToggleActiveHint)
                    .label(fl!("active-hint"))
                    .text_size(14)
                    .width(Length::Fill),
            ),
            padded_control(divider::horizontal::default()).padding([space_xxs, space_s]),
            menu_button(text::body(fl!("window-management-settings")))
                .on_press(Message::OpenSettings)
        ]
        .padding([8, 0]);

        self.core
            .applet
            .popup_container(content_list)
            .limits(layout_popup_limits())
            .into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl Window {
    fn current_workspace_layout(&self) -> WorkspaceLayoutMode {
        derive_layout_mode(self.autotiled, self.config.tiling_engine)
    }

    fn sync_current_workspace_layout(&mut self) {
        let entity = self
            .current_workspace_layout_entities
            .entity(self.current_workspace_layout());
        self.current_workspace_layout_model.activate(entity);
    }

    fn set_current_workspace_layout(
        &mut self,
        requested: WorkspaceLayoutMode,
    ) -> app::Task<Message> {
        // Serialize writes: detached threads can finish in reverse click order.
        if self.layout_change_pending || self.workspace_tx.is_none() {
            return Task::none();
        }
        let transition =
            plan_layout_transition(self.autotiled, self.config.tiling_engine, requested);
        if transition == LayoutTransition::default() {
            self.sync_current_workspace_layout();
            return Task::none();
        }

        self.layout_change_pending = true;
        let helper = self.config_helper.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if let Some(engine) = transition.tiling_engine {
                    helper
                        .set("tiling_engine", engine)
                        .map_err(|err| err.to_string())?;
                }
                Ok(transition)
            })
            .await
            .unwrap_or_else(|err| Err(err.to_string()));
            Message::LayoutChanged(result)
        })
    }
}

fn layout_popup_limits() -> Limits {
    // Three choices plus the selected checkmark need more room than the
    // standard two-choice applet popup; otherwise “Scrolling” is clipped.
    Limits::NONE
        .min_width(420.0)
        .max_width(420.0)
        .min_height(1.0)
        .max_height(1000.0)
}
