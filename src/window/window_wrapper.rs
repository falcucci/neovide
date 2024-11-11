use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use log::trace;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rustc_hash::FxHashMap;
use winit::{
    dpi,
    event::{Ime, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    window::{Fullscreen, Theme, Window, WindowId},
};

use super::{
    application::FocusedState, EventPayload, KeyboardManager, MouseManager, UserEvent,
    WindowCommand, WindowSettings, WindowSettingsChanged,
};

#[cfg(target_os = "macos")]
use {
    crate::{error_msg, window::settings},
    winit::platform::macos::{self, WindowExtMacOS},
};

#[cfg(windows)]
use crate::windows_utils::{register_right_click, unregister_right_click};
use crate::{
    bridge::{send_ui, ParallelCommand, SerialCommand},
    profiling::{tracy_frame, tracy_gpu_collect, tracy_gpu_zone, tracy_plot, tracy_zone},
    renderer::{
        create_skia_renderer, DrawCommand, Renderer, RendererSettingsChanged, SkiaRenderer, VSync,
    },
    settings::{
        clamped_grid_size, Config, FontSettings, HotReloadConfigs, Settings, SettingsChanged,
        DEFAULT_GRID_SIZE, MIN_GRID_SIZE,
    },
    units::{GridRect, GridSize, PixelPos, PixelSize},
    window::{create_window, PhysicalSize, ShouldRender, WindowSize},
    CmdLineSettings,
};

#[cfg(target_os = "macos")]
use super::macos::MacosWindowFeature;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WindowPadding {
    pub top: u32,
    pub left: u32,
    pub right: u32,
    pub bottom: u32,
}

pub fn set_background(background: &str) {
    send_ui(ParallelCommand::SetBackground(background.to_string()));
}

#[derive(PartialEq, PartialOrd)]
pub enum UIState {
    Initing, // Running init.vim/lua
    WaitingForWindowCreate,
    FirstFrame,
    Showing, // No pending resizes
}

pub struct RouteWindow {
    pub previous_frame_start: Instant,
    pub last_dt: f32,
    pub should_render: ShouldRender,
    pub num_consecutive_rendered: u32,
    pub focused: FocusedState,
    pub pending_render: bool, // We should render as soon as the compositor/vsync allows
    pub pending_draw_commands: Vec<Vec<DrawCommand>>,
    pub animation_start: Instant, // When the last animation started (went from idle to animating)
    pub animation_time: Duration, // How long the current animation has been simulated, will usually be in the future
    pub create_window_allowed: bool,
    pub winit_window: Rc<Window>,
    pub skia_renderer: Rc<RefCell<Box<dyn SkiaRenderer>>>,
    pub renderer: Renderer,
    pub keyboard_manager: KeyboardManager,
    pub mouse_manager: MouseManager,
    pub title: String,
    pub font_changed_last_frame: bool,
    pub saved_inner_size: dpi::PhysicalSize<u32>,
    pub saved_grid_size: Option<GridSize<u32>>,
    pub requested_columns: Option<u32>,
    pub requested_lines: Option<u32>,
    pub ui_state: UIState,
    pub is_minimized: bool,
    pub vsync: Option<VSync>,
}

pub struct Route {
    pub window: RouteWindow,
}

impl Route {
    #[inline]
    pub fn new(window: RouteWindow) -> Self {
        Self { window }
    }
}

pub struct WinitWindowWrapper {
    // Don't rearrange this, unless you have a good reason to do so
    // The destruction order has to be correct
    // pub skia_renderer: Option<Box<dyn SkiaRenderer>>,
    window_padding: WindowPadding,
    initial_window_size: WindowSize,
    ime_enabled: bool,
    pub routes: FxHashMap<WindowId, Route>,
    ime_area: (dpi::PhysicalPosition<u32>, dpi::PhysicalSize<u32>),
    #[cfg(target_os = "macos")]
    pub macos_feature: Option<MacosWindowFeature>,
    settings: Arc<Settings>,
}

impl WinitWindowWrapper {
    pub fn new(
        initial_window_size: WindowSize,
        initial_font_settings: Option<FontSettings>,
        settings: Arc<Settings>,
    ) -> Self {
        // let saved_inner_size = Default::default();
        // let renderer = Renderer::new(1.0, initial_font_settings, settings.clone());

        Self {
            routes: FxHashMap::default(),
            window_padding: WindowPadding {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
            },
            initial_window_size,
            ime_enabled: false,
            ime_area: Default::default(),
            #[cfg(target_os = "macos")]
            macos_feature: None,
            settings,
        }
    }

    pub fn exit(&mut self) {
        // self.vsync = None;
        // self.skia_renderer = None;
    }

    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        let window_id = *self.routes.keys().next().unwrap();
        if let Some(route) = &self.routes.get(&window_id) {
            let window = route.window.skia_renderer.borrow_mut().window();
            if fullscreen {
                let handle = window.current_monitor();
                window.set_fullscreen(Some(Fullscreen::Borderless(handle)));
            } else {
                window.set_fullscreen(None);
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub fn set_macos_option_as_meta(&mut self, option: settings::OptionAsMeta) {
        let winit_option = match option {
            settings::OptionAsMeta::OnlyLeft => macos::OptionAsAlt::OnlyLeft,
            settings::OptionAsMeta::OnlyRight => macos::OptionAsAlt::OnlyRight,
            settings::OptionAsMeta::Both => macos::OptionAsAlt::Both,
            settings::OptionAsMeta::None => macos::OptionAsAlt::None,
        };

        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let window = route.window.winit_window.clone();
        if winit_option != window.option_as_alt() {
            window.set_option_as_alt(winit_option);
        }
    }

    pub fn minimize_window(&mut self) {
        let window_id = *self.routes.keys().next().unwrap();
        if let Some(route) = &self.routes.get(&window_id) {
            let window = route.window.skia_renderer.borrow_mut().window();

            window.set_minimized(true);
        }
    }

    pub fn set_ime(&mut self, ime_enabled: bool) {
        let window_id = *self.routes.keys().next().unwrap();
        if let Some(route) = &self.routes.get(&window_id) {
            route
                .window
                .skia_renderer
                .borrow()
                .window()
                .set_ime_allowed(ime_enabled);
        }
    }

    pub fn handle_window_command(&mut self, command: WindowCommand) {
        tracy_zone!("handle_window_commands", 0);
        let window_id = *self.routes.keys().next().unwrap();
        match command {
            WindowCommand::TitleChanged(new_title) => self.handle_title_changed(new_title),
            WindowCommand::SetMouseEnabled(mouse_enabled) => {
                if let Some(route) = self.routes.get_mut(&window_id) {
                    route.window.mouse_manager.enabled = mouse_enabled;
                }
            }
            WindowCommand::ListAvailableFonts => self.send_font_names(),
            WindowCommand::FocusWindow => {
                if let Some(route) = &self.routes.get(&window_id) {
                    route
                        .window
                        .skia_renderer
                        .borrow_mut()
                        .window()
                        .focus_window();
                }
            }
            WindowCommand::Minimize => {
                self.minimize_window();
                if let Some(route) = &mut self.routes.get_mut(&window_id) {
                    route.window.is_minimized = true;
                }
            }
            WindowCommand::ThemeChanged(new_theme) => {
                self.handle_theme_changed(new_theme);
            }
            #[cfg(windows)]
            WindowCommand::RegisterRightClick => register_right_click(),
            #[cfg(windows)]
            WindowCommand::UnregisterRightClick => unregister_right_click(),
        }
    }

    pub fn handle_window_settings_changed(&mut self, changed_setting: WindowSettingsChanged) {
        tracy_zone!("handle_window_settings_changed");
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        match changed_setting {
            WindowSettingsChanged::ObservedColumns(columns) => {
                log::info!("columns changed");
                route.window.requested_columns = columns.map(|v| v.try_into().unwrap());
            }
            WindowSettingsChanged::ObservedLines(lines) => {
                log::info!("lines changed");
                route.window.requested_lines = lines.map(|v| v.try_into().unwrap());
            }
            WindowSettingsChanged::Fullscreen(fullscreen) => {
                self.set_fullscreen(fullscreen);
            }
            WindowSettingsChanged::InputIme(ime_enabled) => {
                self.set_ime(ime_enabled);
            }
            WindowSettingsChanged::ScaleFactor(user_scale_factor) => {
                let renderer = &mut route.window.renderer;
                renderer.user_scale_factor = user_scale_factor.into();
                renderer.grid_renderer.handle_scale_factor_update(
                    renderer.os_scale_factor * renderer.user_scale_factor,
                );
                route.window.font_changed_last_frame = true;
            }
            WindowSettingsChanged::WindowBlurred(blur) => {
                if let Some(route) = &self.routes.get_mut(&window_id) {
                    let WindowSettings { transparency, .. } = self.settings.get::<WindowSettings>();
                    let transparent = transparency < 1.0;
                    route
                        .window
                        .skia_renderer
                        .borrow_mut()
                        .window()
                        .set_blur(blur && transparent);
                }
            }
            #[cfg(target_os = "macos")]
            WindowSettingsChanged::InputMacosOptionKeyIsMeta(option) => {
                self.set_macos_option_as_meta(option);
            }
            #[cfg(target_os = "macos")]
            WindowSettingsChanged::InputMacosAltIsMeta(enabled) => {
                if enabled {
                    error_msg!(concat!(
                        "neovide_input_macos_alt_is_meta has now been removed. ",
                        "Use neovide_input_macos_option_key_is_meta instead. ",
                        "Please check https://neovide.dev/configuration.html#macos-option-key-is-meta for more information.",
                    ));
                }
            }
            _ => {}
        };
        #[cfg(target_os = "macos")]
        if let Some(macos_feature) = &self.macos_feature {
            macos_feature.handle_settings_changed(changed_setting);
        }
    }

    fn handle_render_settings_changed(&mut self, changed_setting: RendererSettingsChanged) {
        let window_id = *self.routes.keys().next().unwrap();
        match changed_setting {
            RendererSettingsChanged::TextGamma(..) | RendererSettingsChanged::TextContrast(..) => {
                if let Some(route) = &mut self.routes.get_mut(&window_id) {
                    route.window.skia_renderer.borrow_mut().resize();
                    route.window.font_changed_last_frame = true;
                }
            }
            _ => {}
        }
    }

    pub fn handle_title_changed(&mut self, new_title: String) {
        let window_id = *self.routes.keys().next().unwrap();
        if let Some(route) = self.routes.get_mut(&window_id) {
            route.window.title = new_title.clone();
            route
                .window
                .skia_renderer
                .borrow_mut()
                .window()
                .set_title(new_title.as_str());
        }
    }

    pub fn handle_theme_changed(&mut self, new_theme: Option<Theme>) {
        let window_id = *self.routes.keys().next().unwrap();
        if let Some(route) = &self.routes.get(&window_id) {
            route
                .window
                .skia_renderer
                .borrow_mut()
                .window()
                .set_theme(new_theme);
        }
    }

    pub fn send_font_names(&mut self) {
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let font_names = route.window.renderer.font_names();
        send_ui(ParallelCommand::DisplayAvailableFonts(font_names));
    }

    pub fn handle_quit(&mut self) {
        send_ui(ParallelCommand::Quit);
    }

    pub fn handle_focus_lost(&mut self) {
        send_ui(ParallelCommand::FocusLost);
    }

    pub fn handle_focus_gained(&mut self) {
        send_ui(ParallelCommand::FocusGained);
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        // Got focus back after being minimized previously
        if route.window.is_minimized {
            // Sending <NOP> after suspend triggers the `VimResume` AutoCmd
            send_ui(SerialCommand::Keyboard("<NOP>".into()));

            route.window.is_minimized = false;
        }
    }

    pub fn handle_window_event(&mut self, event: WindowEvent, window_id: WindowId) -> bool {
        // The renderer and vsync should always be created when a window event is received
        let route = self.routes.get_mut(&window_id).unwrap();
        let vsync = route.window.vsync.as_mut().unwrap();

        route.window.mouse_manager.handle_event(
            &event,
            &route.window.keyboard_manager,
            &route.window.renderer,
            &route.window.winit_window,
        );

        route.window.keyboard_manager.handle_event(&event);
        route.window.renderer.handle_event(&event);
        let mut should_render = true;

        match event {
            WindowEvent::CloseRequested => {
                tracy_zone!("CloseRequested");
                self.handle_quit();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                tracy_zone!("ScaleFactorChanged");
                self.handle_scale_factor_update(scale_factor, window_id);
            }
            WindowEvent::Resized { .. } => {
                route.window.skia_renderer.borrow_mut().resize();
                #[cfg(target_os = "macos")]
                self.macos_feature.as_mut().unwrap().handle_size_changed();
            }
            WindowEvent::DroppedFile(path) => {
                tracy_zone!("DroppedFile");
                let file_path = path.into_os_string().into_string().unwrap();
                send_ui(ParallelCommand::FileDrop(file_path));
            }
            WindowEvent::Focused(focus) => {
                tracy_zone!("Focused");
                if focus {
                    self.handle_focus_gained();
                } else {
                    self.handle_focus_lost();
                }
            }
            WindowEvent::ThemeChanged(theme) => {
                tracy_zone!("ThemeChanged");
                let settings = self.settings.get::<WindowSettings>();
                if settings.theme.as_str() == "auto" {
                    let background = match theme {
                        Theme::Light => "light",
                        Theme::Dark => "dark",
                    };
                    set_background(background);
                }
            }
            WindowEvent::Moved(_) => {
                tracy_zone!("Moved");
                vsync.update(route.window.skia_renderer.borrow_mut().window().as_ref());
            }
            WindowEvent::Ime(Ime::Enabled) => {
                log::info!("Ime enabled");
                self.ime_enabled = true;
                self.update_ime_position(true);
            }
            WindowEvent::Ime(Ime::Disabled) => {
                log::info!("Ime disabled");
                self.ime_enabled = false;
            }
            _ => {
                tracy_zone!("Unknown WindowEvent");
                should_render = false;
            }
        }
        self.routes.get(&window_id).unwrap().window.ui_state >= UIState::FirstFrame && should_render
    }

    pub fn handle_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::DrawCommandBatch(batch) => {
                self.handle_draw_commands(batch);
            }
            UserEvent::WindowCommand(e) => {
                self.handle_window_command(e);
            }
            UserEvent::SettingsChanged(SettingsChanged::Window(e)) => {
                self.handle_window_settings_changed(e);
            }
            UserEvent::SettingsChanged(SettingsChanged::Renderer(e)) => {
                self.handle_render_settings_changed(e);
            }
            UserEvent::ConfigsChanged(config) => {
                self.handle_config_changed(*config);
            }
            _ => {}
        }
    }

    pub fn draw_frame(&mut self, dt: f32) {
        tracy_zone!("draw_frame");
        if self.routes.is_empty() {
            return;
        }
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let vsync = route.window.vsync.as_mut().unwrap();

        if route.window.font_changed_last_frame {
            route.window.font_changed_last_frame = false;
            route.window.renderer.prepare_lines(true);
        }

        route
            .window
            .renderer
            .draw_frame(route.window.skia_renderer.borrow_mut().canvas(), dt);

        route.window.skia_renderer.borrow_mut().flush();

        {
            tracy_gpu_zone!("wait for vsync");
            vsync.wait_for_vsync();
        }

        route.window.skia_renderer.borrow_mut().swap_buffers();

        if route.window.ui_state == UIState::FirstFrame {
            route
                .window
                .skia_renderer
                .borrow_mut()
                .window()
                .set_visible(true);
            route.window.ui_state = UIState::Showing;
        }
        tracy_frame();
        tracy_gpu_collect();
    }

    pub fn animate_frame(&mut self, dt: f32) -> bool {
        tracy_zone!("animate_frame", 0);
        let grid_rect = self.get_grid_rect_from_window(GridSize::default());
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let res = route.window.renderer.animate_frame(&grid_rect, dt);
        tracy_plot!("animate_frame", res as u8 as f64);

        route.window.renderer.prepare_lines(false);
        #[allow(clippy::let_and_return)]
        res
    }

    pub fn try_create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        proxy: &EventLoopProxy<EventPayload>,
    ) {
        tracy_zone!("create_window");

        let maximized = matches!(self.initial_window_size, WindowSize::Maximized);

        let window_config = create_window(event_loop, maximized, "Neovide", &self.settings);
        let window = Rc::new(window_config.window.clone());

        let WindowSettings {
            input_ime,
            theme,
            transparency,
            window_blurred,
            fullscreen,
            #[cfg(target_os = "macos")]
            input_macos_option_key_is_meta,
            ..
        } = self.settings.get::<WindowSettings>();

        window.set_ime_allowed(input_ime);

        // It's important that this is created before the window is resized, since it can change the padding and affect the size
        #[cfg(target_os = "macos")]
        {
            self.macos_feature = Some(MacosWindowFeature::from_winit_window(
                &window,
                self.settings.clone(),
            ));
        }

        let scale_factor = window.scale_factor();

        let config = Config::init();
        let mut renderer = Renderer::new(1.0, config.font, self.settings.clone());
        renderer.handle_os_scale_factor_change(scale_factor);

        let mut size = PhysicalSize::default();
        match self.initial_window_size {
            WindowSize::Maximized => {}
            WindowSize::Grid(grid_size) => {
                let window_size = self.get_window_size_from_grid(&grid_size);
                size = PhysicalSize::new(window_size.width, window_size.height);
            }
            WindowSize::NeovimGrid => {
                let grid_size = renderer.get_grid_size();
                let window_size = self.get_window_size_from_grid(&grid_size);
                size = PhysicalSize::new(window_size.width, window_size.height);
            }
            WindowSize::Size(window_size) => {
                size = window_size;
            }
        };
        if !maximized {
            tracy_zone!("request_inner_size");
            let _ = window.request_inner_size(size);
        }

        // Check that window is visible in some monitor, and reposition it if not.
        if let Ok(previous_position) = window.outer_position() {
            if let Some(current_monitor) = window.current_monitor() {
                let monitor_position = current_monitor.position();
                let monitor_size = current_monitor.size();
                let monitor_width = monitor_size.width as i32;
                let monitor_height = monitor_size.height as i32;

                let window_position = previous_position;

                let window_size = window.outer_size();
                let window_width = window_size.width as i32;
                let window_height = window_size.height as i32;

                if window_position.x + window_width < monitor_position.x
                    || window_position.y + window_height < monitor_position.y
                    || window_position.x > monitor_position.x + monitor_width
                    || window_position.y > monitor_position.y + monitor_height
                {
                    window.set_outer_position(monitor_position);
                };
            };
        }
        log::info!("Showing window size: {:#?}, maximized: {}", size, maximized);
        let is_wayland = matches!(
            window.window_handle().unwrap().as_raw(),
            RawWindowHandle::Wayland(_)
        );
        // On Wayland we can show the window now, since internally it's only shown after the first rendering
        // On the other platforms the window is shown after rendering to avoid flickering
        if is_wayland {
            window.set_visible(true);
        }

        let cmd_line_settings = self.settings.get::<CmdLineSettings>();
        let srgb = cmd_line_settings.srgb;
        let vsync_enabled = cmd_line_settings.vsync;
        // let skia_renderer: Rc<Box<dyn SkiaRenderer>> = Rc::new(create_skia_renderer(
        let skia_renderer: Rc<RefCell<Box<dyn SkiaRenderer>>> = Rc::new(RefCell::new(
            create_skia_renderer(&window_config, srgb, vsync_enabled, self.settings.clone()),
        ));
        let window = skia_renderer.borrow_mut().window();
        // Wrap the window in an Rc
        // let window_rc = Rc::new(window);

        // self.saved_inner_size = window.inner_size();

        log::info!(
            "window created (scale_factor: {:.4}, font_dimensions: {:?})",
            scale_factor,
            renderer.grid_renderer.grid_scale
        );

        window.set_blur(window_blurred && transparency < 1.0);
        if fullscreen {
            let handle = window.current_monitor();
            window.set_fullscreen(Some(Fullscreen::Borderless(handle)));
        }

        match theme.as_str() {
            "light" => set_background("light"),
            "dark" => set_background("dark"),
            "auto" => match window.theme() {
                Some(Theme::Light) => set_background("light"),
                Some(Theme::Dark) => set_background("dark"),
                None => {}
            },
            _ => {}
        }

        let skia_renderer_ref: &dyn SkiaRenderer = &**skia_renderer.borrow();
        let vsync = Some(VSync::new(
            vsync_enabled,
            skia_renderer_ref,
            proxy.clone(),
            self.settings.clone(),
        ));

        {
            tracy_zone!("request_redraw");
            window.request_redraw();
        }

        let route = Route {
            window: RouteWindow {
                previous_frame_start: Instant::now(),
                last_dt: 0.0,
                should_render: ShouldRender::Wait,
                num_consecutive_rendered: 0,
                focused: FocusedState::Unfocused,
                pending_render: false,
                pending_draw_commands: Vec::new(),
                animation_start: Instant::now(),
                animation_time: Duration::from_secs(0),
                create_window_allowed: true,
                winit_window: window.clone(),
                skia_renderer: skia_renderer.clone(),
                renderer,
                keyboard_manager: KeyboardManager::new(self.settings.clone()),
                mouse_manager: MouseManager::new(self.settings.clone()),
                title: String::from("Neovide"),
                font_changed_last_frame: false,
                saved_inner_size: window.inner_size(),
                saved_grid_size: None,
                requested_columns: None,
                requested_lines: None,
                ui_state: UIState::Initing,
                is_minimized: false,
                vsync,
            },
        };

        // self.routes.insert(window.id(), Rc::clone(&window));
        // self.routes.insert(window.id(), Rc::clone(&skia_renderer));
        self.routes.insert(window.id(), route);
        if let Some(route) = self.routes.get_mut(&window.id()) {
            route.window.ui_state = UIState::FirstFrame;
        }
        // self.skia_renderer = Some(skia_renderer);
        #[cfg(target_os = "macos")]
        self.set_macos_option_as_meta(input_macos_option_key_is_meta);
    }

    pub fn handle_draw_commands(&mut self, batch: Vec<DrawCommand>) {
        tracy_zone!("handle_draw_commands");
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let handle_draw_commands_result = route.window.renderer.handle_draw_commands(batch);

        route.window.font_changed_last_frame |= handle_draw_commands_result.font_changed;

        if route.window.ui_state == UIState::Initing && handle_draw_commands_result.should_show {
            log::info!("Showing the Window");
            route.window.ui_state = UIState::WaitingForWindowCreate;
        };
    }

    fn handle_config_changed(&mut self, config: HotReloadConfigs) {
        tracy_zone!("handle_config_changed");
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        route.window.renderer.handle_config_changed(config);
        route.window.font_changed_last_frame = true;
    }

    fn calculate_window_padding(&self) -> WindowPadding {
        let window_settings = self.settings.get::<WindowSettings>();
        #[cfg(not(target_os = "macos"))]
        let window_padding_top = window_settings.padding_top;

        #[cfg(target_os = "macos")]
        let window_padding_top = {
            let mut padding_top = window_settings.padding_top;
            if let Some(macos_feature) = &self.macos_feature {
                padding_top += macos_feature.extra_titlebar_height_in_pixels();
            }
            padding_top
        };

        WindowPadding {
            top: window_padding_top,
            left: window_settings.padding_left,
            right: window_settings.padding_right,
            bottom: window_settings.padding_bottom,
        }
    }

    pub fn prepare_frame(&mut self) -> ShouldRender {
        tracy_zone!("prepare_frame", 0);
        let mut should_render = ShouldRender::Wait;
        let window_id = *self.routes.keys().next().unwrap();
        let window_padding = self.calculate_window_padding();
        let padding_changed = window_padding != self.window_padding;
        if let Some(route) = self.routes.get_mut(&window_id) {
            // route.window.should_render = ShouldRender::Wait;
            // Don't render until the UI is fully entered and the window is shown
            if route.window.ui_state < UIState::FirstFrame {
                return ShouldRender::Wait;
            } else if route.window.ui_state == UIState::FirstFrame {
                should_render = ShouldRender::Immediately;
            }

            // The skia renderer shuld always be created when this point is reached, since the < UIState::FirstFrame check will return true
            let skia_renderer = route.window.skia_renderer.as_ref();
            let resize_requested =
                route.window.requested_columns.is_some() || route.window.requested_lines.is_some();

            if resize_requested {
                // Resize requests (columns/lines) have priority over normal window sizing.
                // So, deal with them first and resize the window programmatically.
                // The new window size will then be processed in the following frame.
                self.update_window_size_from_grid();
            } else if skia_renderer.borrow_mut().window().is_minimized() != Some(true) {
                // NOTE: Only actually resize the grid when the window is not minimized
                // Some platforms return a zero size when that is the case, so we should not try to resize to that.
                let new_size = skia_renderer.borrow_mut().window().inner_size();
                if route.window.saved_inner_size != new_size
                    || route.window.font_changed_last_frame
                    || padding_changed
                {
                    self.window_padding = window_padding;
                    route.window.saved_inner_size = new_size;

                    self.update_grid_size_from_window();
                    should_render = ShouldRender::Immediately;
                }
            }
        }

        self.update_ime_position(false);

        if let Some(route) = self.routes.get_mut(&window_id) {
            should_render.update(route.window.renderer.prepare_frame());
        }

        should_render
    }

    pub fn get_grid_size(&self) -> GridSize<u32> {
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get(&window_id).unwrap();
        route.window.renderer.get_grid_size()
    }

    fn get_window_size_from_grid(&self, grid_size: &GridSize<u32>) -> PixelSize<u32> {
        let window_padding = self.calculate_window_padding();

        let window_padding_size = PixelSize::new(
            window_padding.left + window_padding.right,
            window_padding.top + window_padding.bottom,
        );

        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get(&window_id).unwrap();

        let window_size = (*grid_size * route.window.renderer.grid_renderer.grid_scale)
            .floor()
            .try_cast()
            .unwrap()
            + window_padding_size;

        log::info!(
            "get_window_size_from_grid: Grid Size: {:?}, Window Size {:?}",
            grid_size,
            window_size
        );
        window_size
    }

    fn update_window_size_from_grid(&mut self) {
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let skia_renderer = route.window.skia_renderer.clone();
        let window = skia_renderer.borrow_mut().window();

        let grid_size = clamped_grid_size(&GridSize::new(
            route.window.requested_columns.take().unwrap_or(
                route
                    .window
                    .saved_grid_size
                    .map_or(DEFAULT_GRID_SIZE.width, |v| v.width),
            ),
            route.window.requested_lines.take().unwrap_or(
                route
                    .window
                    .saved_grid_size
                    .map_or(DEFAULT_GRID_SIZE.height, |v| v.height),
            ),
        ));
        let new_size = self.get_window_size_from_grid(&grid_size);

        let new_size = winit::dpi::PhysicalSize {
            width: new_size.width,
            height: new_size.height,
        };
        let _ = window.request_inner_size(new_size);
    }

    fn get_grid_size_from_window(&self, min: GridSize<u32>) -> GridSize<u32> {
        let window_padding = self.window_padding;
        let window_padding_size: PixelSize<u32> = PixelSize::new(
            window_padding.left + window_padding.right,
            window_padding.top + window_padding.bottom,
        );

        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get(&window_id).unwrap();

        let content_size = PixelSize::new(
            route.window.saved_inner_size.width,
            route.window.saved_inner_size.height,
        ) - window_padding_size;

        println!("content_size: {:?}", content_size);

        let grid_size = (content_size / route.window.renderer.grid_renderer.grid_scale)
            .floor()
            .try_cast()
            .unwrap();
        println!("grid_size: {:?}", grid_size);
        println!("min: {:?}", min);

        grid_size.max(min)
    }

    fn get_grid_rect_from_window(&self, min: GridSize<u32>) -> GridRect<f32> {
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get(&window_id).unwrap();
        let size = self.get_grid_size_from_window(min).try_cast().unwrap();
        let pos = PixelPos::new(self.window_padding.left, self.window_padding.top).cast()
            / route.window.renderer.grid_renderer.grid_scale;
        GridRect::<f32>::from_origin_and_size(pos, size)
    }

    fn update_grid_size_from_window(&mut self) {
        let grid_size = self.get_grid_size_from_window(MIN_GRID_SIZE);

        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();

        if route.window.saved_grid_size.as_ref() == Some(&grid_size) {
            trace!("Grid matched saved size, skip update.");
            return;
        }
        route.window.saved_grid_size = Some(grid_size);
        log::info!(
            "Resizing grid based on window size. Grid Size: {:?}, Window Size {:?}",
            grid_size,
            route.window.saved_inner_size
        );
        send_ui(ParallelCommand::Resize {
            width: grid_size.width.into(),
            height: grid_size.height.into(),
        });
    }

    fn update_ime_position(&mut self, force: bool) {
        if !self.ime_enabled || self.routes.is_empty() {
            return;
        }
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get(&window_id).unwrap();
        let skia_renderer = route.window.skia_renderer.clone();
        let grid_scale = route.window.renderer.grid_renderer.grid_scale;
        let font_dimensions = GridSize::new(1.0, 1.0) * grid_scale;
        let position = route.window.renderer.get_cursor_destination();
        let position = position.try_cast::<u32>().unwrap();
        let position = dpi::PhysicalPosition {
            x: position.x,
            y: position.y,
        };
        // NOTE: some compositors don't like excluding too much and try to render popup at the
        // bottom right corner of the provided area, so exclude just the full-width char to not
        // obscure the cursor and not render popup at the end of the window.
        let width = (font_dimensions.width * 2.0).ceil() as u32;
        let height = font_dimensions.height.ceil() as u32;
        let size = dpi::PhysicalSize::new(width, height);
        let area = (position, size);
        if force || self.ime_area != area {
            self.ime_area = (position, size);
            skia_renderer
                .borrow_mut()
                .window()
                .set_ime_cursor_area(position, size);
        }
    }

    fn handle_scale_factor_update(&mut self, scale_factor: f64, window_id: WindowId) {
        if self.routes.is_empty() {
            return;
        }
        let window_id = *self.routes.keys().next().unwrap();
        let route = self.routes.get_mut(&window_id).unwrap();
        let skia_renderer = route.window.skia_renderer.as_ref();
        #[cfg(target_os = "macos")]
        self.macos_feature
            .as_mut()
            .unwrap()
            .handle_scale_factor_update(scale_factor);
        route
            .window
            .renderer
            .handle_os_scale_factor_change(scale_factor);

        skia_renderer.borrow_mut().resize();
    }
}
