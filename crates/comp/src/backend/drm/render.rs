use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        SwapBuffersError,
        drm::{
            DrmAccessError, DrmError, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode,
            compositor::{FrameFlags, PrimaryPlaneElement, RenderFrameError},
        },
        renderer::{
            damage::Error as OutputDamageTrackerError,
            element::{
                Kind, RenderElementStates,
                memory::MemoryRenderBufferRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
        },
    },
    desktop::{Space, Window, utils::bbox_from_surface_tree},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::{
        calloop::{
            Idle, LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::crtc,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    utils::{IsAlive, Logical, Monotonic, Point, Rectangle, Scale, Time},
    wayland::{compositor::with_states, presentation::Refresh},
};

use super::{
    DrmBackend,
    cursor::{CursorCache, MAX_PLANE_SIZE},
    device::Device,
};
use crate::{
    backend::{BACKGROUND, Backend},
    scene::{OutputElement, output_elements},
    state::{Aurora, take_presentation_feedback, update_primary_scanout_output},
    wm::window::WindowElement,
};

/// Consecutive temporary render failures tolerated before waiting for the next damage.
const MAX_RETRIES: u32 = 60;

enum Scheduled {
    Idle(Idle<'static>),
    Timer(RegistrationToken),
}

/// Per-output repaint bookkeeping. A repaint runs only when `damaged` is set, so an idle
/// desktop schedules nothing at all.
#[derive(Default)]
pub struct RenderState {
    /// Something changed since the last render started.
    damaged: bool,
    /// A frame was queued and its vblank has not arrived yet.
    frame_pending: bool,
    /// The repaint that is queued on the event loop, if any.
    scheduled: Option<Scheduled>,
    throttle_timer: Option<RegistrationToken>,
    /// Stand-in for the vblank of an empty frame, which never flips a page. Sends the frame
    /// callbacks so clients stay paced at the refresh rate.
    estimated_vblank: Option<RegistrationToken>,
    last_presentation: Option<Time<Monotonic>>,
    /// When frame callbacks last went out; paces empty frames. Not `last_presentation`, which
    /// only a real flip updates and which is therefore stale during no-damage commits.
    last_frame_callback: Option<Time<Monotonic>>,
    failures: u32,
    /// Set by a session resume until the first frame lands; a failure then means stale buffers.
    after_resume: bool,
}

impl RenderState {
    /// Marks the output dirty and queues a repaint unless one is already coming: either a
    /// frame is in flight (its vblank will render) or a repaint is already scheduled.
    pub fn damage(
        &mut self,
        handle: &LoopHandle<'static, Aurora>,
        node: DrmNode,
        crtc: crtc::Handle,
    ) {
        self.damaged = true;
        if !self.frame_pending {
            self.schedule(handle, node, crtc, None);
        }
    }

    /// `None` runs on the next loop iteration, `Some` after the delay.
    fn schedule(
        &mut self,
        handle: &LoopHandle<'static, Aurora>,
        node: DrmNode,
        crtc: crtc::Handle,
        delay: Option<Duration>,
    ) {
        if self.scheduled.is_some() {
            return;
        }
        self.scheduled = match delay {
            None => Some(Scheduled::Idle(
                handle.insert_idle(move |state| state.render_surface(node, crtc)),
            )),
            Some(delay) => {
                match handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
                    state.render_surface(node, crtc);
                    TimeoutAction::Drop
                }) {
                    Ok(token) => Some(Scheduled::Timer(token)),
                    Err(err) => {
                        tracing::warn!(%err, "failed to schedule a repaint");
                        None
                    }
                }
            }
        };
    }

    /// Cancels everything pending; used when the output goes away or the session resumes.
    pub fn cancel(&mut self, handle: &LoopHandle<'static, Aurora>) {
        match self.scheduled.take() {
            Some(Scheduled::Idle(idle)) => idle.cancel(),
            Some(Scheduled::Timer(token)) => handle.remove(token),
            None => {}
        }
        if let Some(token) = self.throttle_timer.take() {
            handle.remove(token);
        }
        if let Some(token) = self.estimated_vblank.take() {
            handle.remove(token);
        }
        self.frame_pending = false;
        self.last_presentation = None;
        self.last_frame_callback = None;
        self.failures = 0;
    }
}

/// What a finished render produced.
struct Rendered {
    /// A frame was queued for scanout; its vblank must be awaited.
    queued: bool,
    states: RenderElementStates,
}

fn frame_duration(output: &Output) -> Option<Duration> {
    let mode = output.current_mode()?;
    Some(Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
}

impl Aurora {
    /// Marks one output dirty. Nothing happens on the winit backend.
    pub fn queue_redraw_output(&mut self, output: &Output) {
        let Some(id) = output
            .user_data()
            .get::<super::device::UdevOutputId>()
            .copied()
        else {
            return;
        };
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        if let Some(surface) = drm
            .devices
            .get_mut(&id.device_id)
            .and_then(|d| d.surfaces.get_mut(&id.crtc))
        {
            surface.render.damage(&handle, id.device_id, id.crtc);
        }
    }

    /// Marks every output dirty. Cheap to call often: repaints are coalesced per vblank.
    pub fn queue_redraw_all(&mut self) {
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        for (node, device) in &mut drm.devices {
            for (crtc, surface) in &mut device.surfaces {
                surface.render.damage(&handle, *node, *crtc);
            }
        }
    }

    /// Restarts every output's loop from scratch after the session comes back: frames
    /// queued before the pause will never see their vblank.
    pub fn resume_rendering(&mut self) {
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        for (node, device) in &mut drm.devices {
            for (crtc, surface) in &mut device.surfaces {
                // A flip in flight at the pause leaves a stale pending frame in the DRM
                // compositor that would block every later submit. Ok(None) means clean.
                while let Ok(Some(_)) = surface.drm_output.frame_submitted() {}
                surface.render.cancel(&handle);
                surface.render.after_resume = true;
                surface.render.damage(&handle, *node, *crtc);
            }
        }
    }

    /// Renders one output if it has damage and no frame is in flight, then queues the frame.
    pub fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let start = Instant::now();
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let DrmBackend {
            devices,
            renderer,
            session_active,
            cursors,
            ..
        } = &mut **drm;
        let Some(Device {
            surfaces,
            output_manager,
            ..
        }) = devices.get_mut(&node)
        else {
            return;
        };
        let Some(surface) = surfaces.get_mut(&crtc) else {
            return;
        };
        // This is the repaint that was scheduled, whatever happens next.
        surface.render.scheduled = None;
        if !*session_active || surface.render.frame_pending || !surface.render.damaged {
            return;
        }
        let Some(renderer) = renderer.as_mut() else {
            return;
        };
        surface.render.damaged = false;
        let output = surface.output.clone();
        let _span = tracing::debug_span!("render_surface", output = %output.name()).entered();

        self.space.refresh();
        self.xwayland.unmanaged.refresh();
        self.popups.cleanup();

        let mut result = None;
        for attempt in 0..2 {
            let attempted = render_output(
                surface,
                renderer,
                &self.space,
                &self.xwayland.unmanaged,
                self.pointer.current_location(),
                &mut self.cursor_status,
                cursors,
            );
            match attempted {
                // A foreign master (VT switch) changed the crtc bindings; wipe our view of
                // the hardware state and try once more from clean.
                Err(SwapBuffersError::ContextLost(err))
                    if attempt == 0
                        && matches!(
                            err.downcast_ref::<DrmError>(),
                            Some(DrmError::TestFailed(_))
                        ) =>
                {
                    tracing::warn!(%err, "atomic test failed, resetting drm state");
                    if let Err(err) = output_manager.device_mut().reset_state() {
                        tracing::error!(%err, "failed to reset the drm device");
                        result = Some(Err(SwapBuffersError::ContextLost(Box::new(err))));
                        break;
                    }
                }
                other => {
                    result = Some(other);
                    break;
                }
            }
        }

        let frame_time = frame_duration(&output);
        let elapsed = start.elapsed();
        match result {
            Some(Ok(Some(rendered))) => {
                surface.render.failures = 0;
                surface.render.after_resume = false;
                surface.render.frame_pending = rendered.queued;
                if let Some(frame) = frame_time
                    && elapsed > frame / 2
                {
                    tracing::debug!(?elapsed, budget = ?frame / 2, "render over budget");
                }
                tracing::trace!(?elapsed, queued = rendered.queued, "rendered");
                if rendered.queued {
                    surface.render.last_frame_callback = Some(self.clock.now());
                    if let Some(token) = surface.render.estimated_vblank.take() {
                        handle.remove(token);
                    }
                    let feedback = surface.dmabuf_feedback.clone();
                    self.post_repaint(
                        &output,
                        Duration::from(self.clock.now()),
                        feedback.as_ref(),
                        &rendered.states,
                    );
                    let _ = self.display_handle.flush_clients();
                } else if surface.render.estimated_vblank.is_none() {
                    // No page flip means no vblank; without pacing a client that commits on
                    // every callback would spin the compositor.
                    let frame = frame_time.unwrap_or(Duration::from_millis(16));
                    let delay = surface.render.last_frame_callback.map_or(frame, |last| {
                        frame.saturating_sub(Time::elapsed(&last, self.clock.now()))
                    });
                    let states = rendered.states;
                    let timer = Timer::from_duration(delay);
                    match handle.insert_source(timer, move |_, _, state| {
                        state.estimated_vblank(node, crtc, &states);
                        TimeoutAction::Drop
                    }) {
                        Ok(token) => surface.render.estimated_vblank = Some(token),
                        Err(err) => tracing::warn!(%err, "failed to arm the estimated vblank"),
                    }
                }
            }
            Some(Ok(None)) => {}
            Some(Err(err)) => {
                surface.render.damaged = true;
                // The first frame after a resume can fail on buffers the pause invalidated.
                let inactive = matches!(
                    &err,
                    SwapBuffersError::TemporaryFailure(e)
                        if matches!(e.downcast_ref::<DrmError>(), Some(DrmError::DeviceInactive))
                );
                if surface.render.after_resume
                    && !inactive
                    && !matches!(err, SwapBuffersError::AlreadySwapped)
                {
                    surface.render.after_resume = false;
                    tracing::warn!(%err, "first frame after resume failed, resetting buffers");
                    surface.drm_output.reset_buffers();
                    surface.render.schedule(&handle, node, crtc, frame_time);
                    return;
                }
                match err {
                    // A frame is already queued; its vblank renders the pending damage.
                    SwapBuffersError::AlreadySwapped => surface.render.frame_pending = true,
                    // Session resume redoes everything.
                    SwapBuffersError::TemporaryFailure(err)
                        if matches!(
                            err.downcast_ref::<DrmError>(),
                            Some(DrmError::DeviceInactive)
                        ) =>
                    {
                        tracing::debug!("drm device inactive, waiting for session resume");
                    }
                    SwapBuffersError::TemporaryFailure(err) => {
                        surface.render.failures += 1;
                        tracing::warn!(%err, failures = surface.render.failures, "temporary render failure");
                        if surface.render.failures <= MAX_RETRIES {
                            surface.render.schedule(&handle, node, crtc, frame_time);
                        }
                    }
                    SwapBuffersError::ContextLost(err) => {
                        tracing::error!(%err, "rendering lost, stopping");
                        crate::safety::arm_exit_deadline();
                        self.loop_signal.stop();
                    }
                }
            }
            None => {}
        }
    }

    /// Sends the frame callbacks of an empty frame at the time its vblank would have been.
    fn estimated_vblank(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        states: &RenderElementStates,
    ) {
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let Some(surface) = drm
            .devices
            .get_mut(&node)
            .and_then(|d| d.surfaces.get_mut(&crtc))
        else {
            return;
        };
        surface.render.estimated_vblank = None;
        if !drm.session_active {
            return;
        }
        surface.render.last_frame_callback = Some(self.clock.now());
        let output = surface.output.clone();
        let feedback = surface.dmabuf_feedback.clone();
        self.post_repaint(
            &output,
            Duration::from(self.clock.now()),
            feedback.as_ref(),
            states,
        );
        let _ = self.display_handle.flush_clients();
    }

    /// Handles a vblank: acknowledges the flipped frame, reports presentation, and renders
    /// again only if something changed meanwhile.
    pub fn frame_finish(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<DrmEventMetadata>,
    ) {
        let _span = tracing::debug_span!("frame_finish").entered();
        let handle = self.handle.clone();
        let Backend::Drm(drm) = &mut self.backend else {
            return;
        };
        let session_active = drm.session_active;
        let Some(surface) = drm
            .devices
            .get_mut(&node)
            .and_then(|d| d.surfaces.get_mut(&crtc))
        else {
            tracing::debug!(?crtc, "vblank for an output that is gone");
            return;
        };
        if !session_active {
            // Still acknowledge the flip, otherwise the compositor keeps a pending frame
            // that never clears and the output freezes after resume.
            let _ = surface.drm_output.frame_submitted();
            surface.render.frame_pending = false;
            return;
        }

        if let Some(token) = surface.render.throttle_timer.take() {
            handle.remove(token);
        }
        let Some(frame_duration) = frame_duration(&surface.output) else {
            return;
        };

        let tp = metadata.as_ref().and_then(|metadata| match metadata.time {
            DrmEventTime::Monotonic(tp) => (!tp.is_zero()).then_some(tp),
            DrmEventTime::Realtime(_) => None,
        });
        let seq = metadata.as_ref().map_or(0, |metadata| metadata.sequence);
        let (clock, flags) = match tp {
            Some(tp) => (
                Time::<Monotonic>::from(tp),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            ),
            None => (self.clock.now(), wp_presentation_feedback::Kind::Vsync),
        };

        // Some drivers deliver a vblank early or twice; re-deliver it at the right time
        // instead of acknowledging a frame that has not been shown yet.
        if let Some(last) = surface.render.last_presentation {
            let remaining = frame_duration.saturating_sub(Time::elapsed(&last, clock));
            if remaining > frame_duration / 2 {
                let throttled = DrmEventMetadata {
                    sequence: seq,
                    time: DrmEventTime::Monotonic(
                        tp.map_or(Duration::ZERO, |tp| tp.saturating_add(remaining)),
                    ),
                };
                match handle.insert_source(Timer::from_duration(remaining), move |_, _, state| {
                    state.frame_finish(node, crtc, &mut Some(throttled));
                    TimeoutAction::Drop
                }) {
                    Ok(token) => {
                        tracing::debug!(?remaining, "throttling an early vblank");
                        surface.render.throttle_timer = Some(token);
                        return;
                    }
                    Err(err) => tracing::warn!(%err, "failed to throttle a vblank"),
                }
            }
        }
        surface.render.last_presentation = Some(clock);

        let mut render_again = true;
        match surface.drm_output.frame_submitted() {
            Ok(feedback) => {
                surface.render.frame_pending = false;
                if let Some(mut feedback) = feedback.flatten() {
                    feedback.presented(clock, Refresh::fixed(frame_duration), seq as u64, flags);
                }
            }
            Err(err) => {
                let err = SwapBuffersError::from(err);
                tracing::warn!(%err, "frame_submitted failed");
                surface.render.frame_pending = false;
                render_again = match err {
                    SwapBuffersError::AlreadySwapped => true,
                    // Session resume redoes everything.
                    SwapBuffersError::TemporaryFailure(err) => matches!(
                        err.downcast_ref::<DrmError>(),
                        Some(DrmError::Access(DrmAccessError { source, .. }))
                            if source.kind() == std::io::ErrorKind::PermissionDenied
                    ),
                    SwapBuffersError::ContextLost(err) => {
                        tracing::error!(%err, "rendering lost, stopping");
                        crate::safety::arm_exit_deadline();
                        self.loop_signal.stop();
                        false
                    }
                };
            }
        }
        let _ = self.display_handle.flush_clients();

        if render_again && surface.render.damaged {
            // Clients paint off the frame callbacks sent at repaint; waiting part of the
            // frame first lets them land a buffer in this very repaint, which is about a
            // frame less latency than repainting straight away. The rest is left for the
            // compositor, which should need well under half a frame.
            surface
                .render
                .schedule(&handle, node, crtc, Some(frame_duration.mul_f64(0.6)));
        }
    }
}

/// Builds the element list and renders it; queues the frame for scanout if anything changed.
/// `Ok(None)` means the output has no place in the layout right now.
fn render_output(
    surface: &mut super::device::Surface,
    renderer: &mut GlesRenderer,
    space: &Space<WindowElement>,
    unmanaged: &Space<Window>,
    pointer_location: Point<f64, Logical>,
    cursor_status: &mut CursorImageStatus,
    cursors: &mut CursorCache,
) -> Result<Option<Rendered>, SwapBuffersError> {
    let output = surface.output.clone();
    let Some(output_geo) = space.output_geometry(&output) else {
        return Ok(None);
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    let mut elements = cursor_elements(
        renderer,
        cursors,
        cursor_status,
        pointer_location,
        output_geo,
        scale,
        output.current_scale().integer_scale(),
    );
    match output_elements(space, unmanaged, renderer, &output) {
        Some(scene) => elements.extend(scene),
        None => return Ok(None),
    }

    let result = surface
        .drm_output
        .render_frame(renderer, &elements, BACKGROUND, FrameFlags::DEFAULT)
        .map_err(|err| match err {
            RenderFrameError::PrepareFrame(err) => SwapBuffersError::from(err),
            RenderFrameError::RenderFrame(OutputDamageTrackerError::Rendering(err)) => {
                SwapBuffersError::from(err)
            }
            other => SwapBuffersError::TemporaryFailure(format!("{other:?}").into()),
        })?;

    // Only when the kernel cannot be handed a fence for the render.
    if result.needs_sync()
        && let PrimaryPlaneElement::Swapchain(element) = &result.primary_element
    {
        let _ = element.sync.wait();
    }
    let queued = !result.is_empty;
    let states = result.states;

    update_primary_scanout_output(space, unmanaged, &output, cursor_status, &states);
    if queued {
        let feedback = take_presentation_feedback(&output, space, unmanaged, &states);
        surface
            .drm_output
            .queue_frame(Some(feedback))
            .map_err(SwapBuffersError::from)?;
    }
    Ok(Some(Rendered { queued, states }))
}

/// The pointer as render elements, empty when it is hidden or on another output.
fn cursor_elements(
    renderer: &mut GlesRenderer,
    cursors: &mut CursorCache,
    status: &mut CursorImageStatus,
    pointer_location: Point<f64, Logical>,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
    integer_scale: i32,
) -> Vec<OutputElement> {
    if let CursorImageStatus::Surface(surface) = status
        && !surface.alive()
    {
        *status = CursorImageStatus::default_named();
    }
    if !output_geo.to_f64().contains(pointer_location) {
        return Vec::new();
    }
    let local = pointer_location - output_geo.loc.to_f64();

    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Named(icon) => {
            let image = cursors.get(*icon, integer_scale);
            let location = (local.to_physical(scale) - image.hotspot.to_f64())
                .to_i32_round::<i32>()
                .to_f64();
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                &image.buffer,
                None,
                None,
                None,
                Kind::Cursor,
            ) {
                Ok(element) => vec![OutputElement::Cursor(element)],
                Err(err) => {
                    tracing::warn!(%err, "failed to import the cursor image");
                    Vec::new()
                }
            }
        }
        CursorImageStatus::Surface(surface) => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .and_then(|attrs| attrs.lock().ok().map(|a| a.hotspot))
                    .unwrap_or_default()
            });
            let size = bbox_from_surface_tree(surface, (0, 0)).size;
            let kind = if size.w > MAX_PLANE_SIZE || size.h > MAX_PLANE_SIZE {
                Kind::Unspecified
            } else {
                Kind::Cursor
            };
            let location = (local - hotspot.to_f64()).to_physical(scale).to_i32_round();
            render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer, surface, location, scale, 1.0, kind,
            )
            .into_iter()
            .map(OutputElement::CursorSurface)
            .collect()
        }
    }
}

/// Registers the vblank handler for one device.
pub fn vblank_handler(
    node: DrmNode,
) -> impl FnMut(DrmEvent, &mut Option<DrmEventMetadata>, &mut Aurora) {
    move |event, metadata, state| match event {
        DrmEvent::VBlank(crtc) => state.frame_finish(node, crtc, metadata),
        DrmEvent::Error(err) => tracing::error!(%err, "drm event error"),
    }
}
