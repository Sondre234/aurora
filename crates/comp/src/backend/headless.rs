//! QA outputs with no display behind them (`--qa` debug actions), so output hotplug and window
//! rescue can be exercised without DRM. A timer stands in for vblank and sends frame callbacks.
use std::time::Duration;

use smithay::{
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::backend::GlobalId,
    },
};

use crate::{state::Aurora, wm::outputs::rule_scale};

pub struct HeadlessOutput {
    pub output: Output,
    global: GlobalId,
    timer: RegistrationToken,
}

impl Aurora {
    pub fn debug_add_output(
        &mut self,
        name: String,
        size: (i32, i32),
        refresh_mhz: u32,
        pos: Option<(i32, i32)>,
    ) {
        if self.wm.outputs.iter().any(|o| o.name() == name) {
            tracing::warn!("debug-add-output: {name} already exists");
            return;
        }
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Aurora".into(),
                model: "Headless".into(),
                serial_number: "Unknown".into(),
            },
        );
        let global = output.create_global::<Aurora>(&self.display_handle);
        let mode = Mode {
            size: size.into(),
            refresh: refresh_mhz.min(i32::MAX as u32) as i32,
        };
        output.set_preferred(mode);
        let scale = rule_scale(self.output_rule(&name));
        output.change_current_state(Some(mode), None, Some(scale), None);

        let interval = Duration::from_micros(1_000_000_000 / u64::from(refresh_mhz.max(1_000)));
        let ticking = output.clone();
        let timer =
            self.handle
                .insert_source(Timer::from_duration(interval), move |_, _, state| {
                    state.send_nested_frames(&ticking);
                    TimeoutAction::ToDuration(interval)
                });
        let Ok(timer) = timer else {
            tracing::warn!("debug-add-output: cannot register the frame timer");
            self.display_handle.remove_global::<Aurora>(global);
            return;
        };
        if let Some(pos) = pos {
            self.wm.debug_positions.insert(name.clone(), pos);
        }
        self.headless.insert(
            name,
            HeadlessOutput {
                output: output.clone(),
                global,
                timer,
            },
        );
        self.add_output(&output);
    }

    pub fn debug_remove_output(&mut self, name: &str) {
        let Some(headless) = self.headless.remove(name) else {
            tracing::warn!("debug-remove-output: {name} is not a headless output");
            return;
        };
        self.wm_output_removed(&headless.output);
        self.wm.debug_positions.remove(name);
        self.handle.remove(headless.timer);
        headless.output.leave_all();
        self.display_handle.remove_global::<Aurora>(headless.global);
    }
}
