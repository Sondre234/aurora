use smithay::{
    backend::{
        renderer::{ImportDma, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::{Space, Window},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Transform},
};

use std::time::Duration;

use crate::{
    backend::BACKGROUND,
    capture::{self, Captures},
    scene::output_elements,
    state::Aurora,
    wm::outputs::rule_scale,
    wm::window::WindowElement,
};

/// Nested backend: renders into a window on the host compositor.
pub fn init(
    event_loop: &mut EventLoop<Aurora>,
    state: &mut Aurora,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Aurora".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    output.create_global::<Aurora>(&state.display_handle);
    let scale = rule_scale(state.output_rule("winit"));
    output.change_current_state(Some(mode), Some(Transform::Flipped180), Some(scale), None);
    output.set_preferred(mode);
    state.add_output(&output);

    state.init_dmabuf(
        crate::dmabuf::renderer_node(backend.renderer()),
        backend.renderer().dmabuf_formats(),
    );

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
                state.arrange_outputs();
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let size = backend.window_size();
                // The nested window redraws every frame, so the result needs no keep-alive.
                state.wm.tick(Duration::from(state.clock.now()));
                if let Err(err) = draw(
                    &mut backend,
                    &mut damage_tracker,
                    &state.space,
                    &state.xwayland.unmanaged,
                    &mut state.captures,
                    Duration::from(state.clock.now()),
                    &output,
                ) {
                    tracing::warn!(%err, "nested frame failed");
                } else if let Err(err) = backend.submit(Some(&[Rectangle::from_size(size)])) {
                    tracing::warn!(%err, "nested swap failed");
                }

                state.send_nested_frames(&output);
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => (),
        })?;

    Ok(())
}

/// Renders the scene of `output` into the window's back buffer.
fn draw(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: &mut OutputDamageTracker,
    space: &Space<WindowElement>,
    unmanaged: &Space<Window>,
    captures: &mut Captures,
    now: Duration,
    output: &Output,
) -> Result<(), Box<dyn std::error::Error>> {
    let (renderer, mut framebuffer) = backend.bind()?;
    let elements =
        output_elements(space, unmanaged, renderer, output).ok_or("output is not mapped")?;
    // The nested window draws no pointer; the host does.
    capture::serve(captures, renderer, output, &elements, 0, now);
    damage_tracker
        .render_output(renderer, &mut framebuffer, 0, &elements, BACKGROUND)
        .map_err(|err| format!("{err:?}"))?;
    Ok(())
}
