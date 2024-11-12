use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    window::WindowId,
};

use super::{
    save_window_size, CmdLineSettings, EventPayload, UserEvent, WindowSettings, WinitWindowWrapper,
};
use crate::{
    profiling::{tracy_plot, tracy_zone},
    renderer::DrawCommand,
    settings::Settings,
    FontSettings, WindowSize,
};

pub enum FocusedState {
    Focused,
    UnfocusedNotDrawn,
    Unfocused,
}

#[derive(Debug, PartialEq)]
pub enum ShouldRender {
    Immediately,
    Wait,
    Deadline(Instant),
}

impl ShouldRender {
    pub fn update(&mut self, rhs: ShouldRender) {
        let lhs = &self;
        match (lhs, rhs) {
            (ShouldRender::Immediately, _) => {}
            (_, ShouldRender::Immediately) => {
                *self = ShouldRender::Immediately;
            }
            (ShouldRender::Deadline(lhs), ShouldRender::Deadline(rhs)) => {
                if rhs < *lhs {
                    *self = ShouldRender::Deadline(rhs);
                }
            }
            (ShouldRender::Deadline(_), ShouldRender::Wait) => {}
            (ShouldRender::Wait, ShouldRender::Deadline(instant)) => {
                *self = ShouldRender::Deadline(instant);
            }
            (ShouldRender::Wait, ShouldRender::Wait) => {}
        }
    }

    #[cfg(feature = "profiling")]
    fn plot_tracy(&self) {
        match &self {
            ShouldRender::Immediately => {
                tracy_plot!("should_render", 0.0);
            }
            ShouldRender::Wait => {
                tracy_plot!("should_render", -1.0);
            }
            ShouldRender::Deadline(instant) => {
                tracy_plot!(
                    "should_render",
                    instant
                        .saturating_duration_since(Instant::now())
                        .as_secs_f64()
                );
            }
        }
    }
}

const MAX_ANIMATION_DT: f64 = 1.0 / 120.0;

pub struct Application {
    idle: bool,
    window_wrapper: WinitWindowWrapper,
    proxy: EventLoopProxy<EventPayload>,
    settings: Arc<Settings>,
}

impl Application {
    pub fn new(
        initial_window_size: WindowSize,
        initial_font_settings: Option<FontSettings>,
        proxy: EventLoopProxy<EventPayload>,
        settings: Arc<Settings>,
    ) -> Self {
        let cmd_line_settings = settings.get::<CmdLineSettings>();
        let idle = cmd_line_settings.idle;

        let window_wrapper =
            WinitWindowWrapper::new(initial_window_size, initial_font_settings, settings.clone());

        Self {
            idle,
            window_wrapper,
            proxy,
            settings,
        }
    }

    fn get_refresh_rate(&self) -> f32 {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get(&window_id).unwrap();
        match route.window.focused {
            // NOTE: Always wait for the idle refresh rate when winit throttling is used to avoid waking up too early
            // The winit redraw request will likely happen much before that and wake it up anyway
            FocusedState::Focused | FocusedState::UnfocusedNotDrawn => {
                self.settings.get::<WindowSettings>().refresh_rate as f32
            }
            _ => self.settings.get::<WindowSettings>().refresh_rate_idle as f32,
        }
        .max(1.0)
    }

    fn get_frame_deadline(&self) -> Instant {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get(&window_id).unwrap();
        let refresh_rate = self.get_refresh_rate();
        let expected_frame_duration = Duration::from_secs_f32(1.0 / refresh_rate);
        route.window.previous_frame_start + expected_frame_duration
    }

    fn get_event_deadline(&self) -> Instant {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get(&window_id).unwrap();
        // When there's a pending render we don't need to wait for anything else than the render event
        if route.window.pending_render {
            return route.window.animation_start + route.window.animation_time;
        }

        match route.window.should_render {
            ShouldRender::Immediately => Instant::now(),
            ShouldRender::Deadline(old_deadline) => old_deadline.min(self.get_frame_deadline()),
            _ => self.get_frame_deadline(),
        }
    }

    fn schedule_next_event(&mut self, event_loop: &ActiveEventLoop) {
        // #[cfg(feature = "profiling")]
        // self.should_render.plot_tracy();
        // if self.create_window_allowed {
        //     self.window_wrapper
        //         .try_create_window(event_loop, &self.proxy);
        // }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.get_event_deadline()));
    }

    fn animate(&mut self) {
        if self.window_wrapper.routes.is_empty() {
            return;
        }

        // Scope the mutable borrow of routes
        let should_render_immediately = {
            let window_id = *self.window_wrapper.routes.keys().next().unwrap();
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            let vsync = route.window.vsync.as_ref().unwrap();

            let dt = Duration::from_secs_f32(vsync.get_refresh_rate(
                &route.window.skia_renderer.as_ref().borrow().window(),
                &self.settings,
            ));

            let now = Instant::now();
            let target_animation_time = now - route.window.animation_start;
            let mut delta = target_animation_time.saturating_sub(route.window.animation_time);
            if delta > Duration::from_millis(1000) {
                route.window.animation_start = now;
                route.window.animation_time = Duration::ZERO;
                delta = dt;
            }

            // Catchup immediately if the delta is more than one frame, otherwise smooth it over 10 frames
            let catchup = if delta >= dt {
                delta
            } else {
                delta.div_f64(10.0)
            };

            let dt = dt + catchup;
            tracy_plot!("Simulation dt", dt.as_secs_f64());
            route.window.animation_time += dt;

            let num_steps = (dt.as_secs_f64() / MAX_ANIMATION_DT).ceil() as u32;
            let step = dt / num_steps;

            let mut should_render_immediately = false;
            let animate_frame = self.window_wrapper.animate_frame(step.as_secs_f32());
            for _ in 0..num_steps {
                if animate_frame {
                    should_render_immediately = true;
                }
            }

            should_render_immediately
        };

        // Update should_render status outside the mutable borrow scope
        if should_render_immediately {
            let window_id = *self.window_wrapper.routes.keys().next().unwrap();
            if let Some(route) = self.window_wrapper.routes.get_mut(&window_id) {
                route.window.should_render = ShouldRender::Immediately;
            }
        }
    }

    fn render(&mut self) {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();

        {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            route.window.pending_render = false;
            tracy_plot!("pending_render", route.window.pending_render as u8 as f64);
        }

        {
            let route = self.window_wrapper.routes.get(&window_id).unwrap();
            self.window_wrapper.draw_frame(route.window.last_dt);
        }

        {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            if let FocusedState::UnfocusedNotDrawn = route.window.focused {
                route.window.focused = FocusedState::Unfocused;
            }

            route.window.num_consecutive_rendered += 1;
            tracy_plot!(
                "num_consecutive_rendered",
                route.window.num_consecutive_rendered as f64
            );
            route.window.last_dt = route.window.previous_frame_start.elapsed().as_secs_f32();
            route.window.previous_frame_start = Instant::now();
        }
    }

    fn process_buffered_draw_commands(&mut self) {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let pending_draw_commands = {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            route
                .window
                .pending_draw_commands
                .drain(..)
                .collect::<Vec<_>>()
        };

        if pending_draw_commands.is_empty() {
            return;
        }

        for command in pending_draw_commands {
            self.window_wrapper.handle_draw_commands(command);
        }

        let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
        route.window.should_render = ShouldRender::Immediately;
    }

    fn reset_animation_period(&mut self) {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
        route.window.should_render = ShouldRender::Wait;
        if route.window.num_consecutive_rendered == 0 {
            route.window.animation_start = Instant::now();
            route.window.animation_time = Duration::ZERO;
        }
    }

    fn schedule_render(&mut self, skipped_frame: bool) {
        if self.window_wrapper.routes.is_empty() {
            return;
        }
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
        let window = route.window.winit_window.clone();
        let vsync = route.window.vsync.as_mut().unwrap();

        // There's really no point in trying to render if the frame is skipped
        // (most likely due to the compositor being busy). The animated frame will
        // be rendered at an appropriate time anyway.
        if !skipped_frame {
            // When winit throttling is used, request a redraw and wait for the render event
            // Otherwise render immediately
            if vsync.uses_winit_throttling() {
                vsync.request_redraw(window.as_ref());
                route.window.pending_render = true;
                // tracy_plot!("pending_render", self.pending_render as u8 as f64);
            } else {
                self.render();
            }
        }
    }

    fn prepare_and_animate(&mut self) {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let should_prepare = {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();

            // Determine if we should prepare
            let skipped_frame = route.window.pending_render
                && Instant::now() > (route.window.animation_start + route.window.animation_time);

            !route.window.pending_render || skipped_frame
        };

        if !should_prepare {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            route
                .window
                .renderer
                .grid_renderer
                .shaper
                .cleanup_font_cache();
            return;
        }

        // Now we can call prepare_frame without overlapping mutable borrows
        let res = self.window_wrapper.prepare_frame();

        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
        route.window.should_render.update(res);

        let skipped_frame = route.window.pending_render
            && Instant::now() > (route.window.animation_start + route.window.animation_time);
        let should_animate =
            route.window.should_render == ShouldRender::Immediately || !self.idle || skipped_frame;

        if should_animate {
            self.reset_animation_period();
            self.animate();
            self.schedule_render(skipped_frame);
        } else {
            route.window.num_consecutive_rendered = 0;
            tracy_plot!(
                "num_consecutive_rendered",
                route.window.num_consecutive_rendered as f64
            );
            route.window.last_dt = route.window.previous_frame_start.elapsed().as_secs_f32();
            route.window.previous_frame_start = Instant::now();
        }
    }

    fn redraw_requested(&mut self) {
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
        if route.window.pending_render {
            println!("render (redraw requested)");
            tracy_zone!("render (redraw requested)");
            self.render();
            // We should process all buffered draw commands as soon as the rendering has finished
            self.process_buffered_draw_commands();
        } else {
            tracy_zone!("redraw requested");
            // The OS itself asks us to redraw, so we need to prepare first
            route.window.should_render = ShouldRender::Immediately;
        }
    }
}

impl ApplicationHandler<EventPayload> for Application {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        tracy_zone!("new_events");
        match cause {
            StartCause::Init => {
                // self.create_window_allowed = true;
                self.window_wrapper
                    .try_create_window(event_loop, &self.proxy.clone());
                // let routes = self.window_wrapper.routes.clone();
                // println!("{:?}", routes);
            }
            StartCause::ResumeTimeReached { .. } => {
                // self.create_window_allowed = false;
            }
            StartCause::WaitCancelled { .. } => {
                // self.create_window_allowed = false;
            }
            StartCause::Poll => {
                // self.create_window_allowed = false;
            }
            StartCause::CreateWindow => {
                // self.create_window_allowed = true;
                self.window_wrapper
                    .try_create_window(event_loop, &self.proxy.clone());
                // let routes = self.window_wrapper.routes.clone();
                // println!("{:?}", routes);
            }
        }
        self.schedule_next_event(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        tracy_zone!("window_event");
        {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            match event {
                WindowEvent::RedrawRequested => {
                    self.redraw_requested();
                }
                WindowEvent::Focused(focused_event) => {
                    route.window.focused = if focused_event {
                        FocusedState::Focused
                    } else {
                        FocusedState::UnfocusedNotDrawn
                    };
                    #[cfg(target_os = "macos")]
                    self.window_wrapper
                        .macos_feature
                        .as_mut()
                        .expect("MacosWindowFeature should already be created here.")
                        .ensure_app_initialized();
                }
                _ => {}
            }
        }

        if self.window_wrapper.handle_window_event(event, window_id) {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            route.window.should_render = ShouldRender::Immediately;
        }
        self.schedule_next_event(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EventPayload) {
        tracy_zone!("user_event");
        // let window_id = event.window_id;
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let event_payload = event.payload;
        {
            let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
            match &event_payload {
                UserEvent::NeovimExited => {
                    save_window_size(&self.window_wrapper, &self.settings);
                    event_loop.exit();
                }
                UserEvent::RedrawRequested => {
                    self.redraw_requested();
                }
                UserEvent::DrawCommandBatch(batch) if route.window.pending_render => {
                    // Buffer the draw commands if we have a pending render, we have already decided what to
                    // draw, so it's not a good idea to process them now.
                    // They will be processed immediately after the rendering.
                    route.window.pending_draw_commands.push(batch.clone());
                }
                _ => {
                    route.window.should_render = ShouldRender::Immediately;
                }
            }
        }
        self.window_wrapper.handle_user_event(event_payload);
        self.schedule_next_event(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        tracy_zone!("about_to_wait");
        self.prepare_and_animate();
        self.schedule_next_event(event_loop);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        tracy_zone!("resumed");
        let window_id = *self.window_wrapper.routes.keys().next().unwrap();
        let route = self.window_wrapper.routes.get_mut(&window_id).unwrap();
        route.window.create_window_allowed = true;
        self.schedule_next_event(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        tracy_zone!("exiting");
        self.window_wrapper.exit();
        self.schedule_next_event(event_loop);
    }
}
