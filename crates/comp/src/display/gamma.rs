//! `zwlr_gamma_control_manager_v1`, what gammastep and wlsunset use for night light. One
//! control per output, exclusive: a second one for the same output fails. The ramp goes to
//! the hardware (DRM `GAMMA_LUT` or the legacy ramp, see `backend/drm/display.rs`); when the
//! control goes away, its client included, the output gets the driver default back. An
//! output without hardware gamma (or the nested backend) fails the control right away.
use std::{
    io,
    os::{fd::OwnedFd, unix::fs::FileExt},
};

use smithay::{
    output::Output,
    reexports::{
        rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl},
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource,
            backend::{ClientId, GlobalId},
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{backend::Backend, state::Aurora};

/// The client's table: red, green, then blue, `size` native-endian u16 each. `None` unless
/// the length is exactly that.
pub fn parse_ramp(bytes: &[u8], size: usize) -> Option<Vec<u16>> {
    if size == 0 || bytes.len() != size.checked_mul(6)? {
        return None;
    }
    Some(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_ne_bytes(*b))
            .collect(),
    )
}

/// A ramp as the kernel's `GAMMA_LUT` blob: one `drm_color_lut { red, green, blue,
/// reserved }` per entry.
pub fn lut_bytes(ramp: &[u16], size: usize) -> Option<Vec<u8>> {
    if size == 0 || ramp.len() != size.checked_mul(3)? {
        return None;
    }
    let (red, rest) = ramp.split_at(size);
    let (green, blue) = rest.split_at(size);
    let mut out = Vec::with_capacity(size * 8);
    for i in 0..size {
        for v in [red[i], green[i], blue[i], 0] {
            out.extend_from_slice(&v.to_ne_bytes());
        }
    }
    Some(out)
}

/// The identity ramp, for the legacy interface which has no "default".
pub fn linear_ramp(size: usize) -> Vec<u16> {
    let channel: Vec<u16> = (0..size)
        .map(|i| match size {
            0 | 1 => u16::MAX,
            _ => (i as u64 * u64::from(u16::MAX) / (size as u64 - 1)) as u16,
        })
        .collect();
    channel.repeat(3)
}

/// Reads `len` bytes from the start of the client's file, without ever blocking (a pipe or
/// a short file is the client's fault, like in wlroots).
fn read_table(fd: OwnedFd, len: usize) -> io::Result<Vec<u8>> {
    let flags = fcntl_getfl(&fd)?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?;
    let file = std::fs::File::from(fd);
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read_at(&mut buf[filled..], filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

pub struct GammaState {
    _global: GlobalId,
    /// Live controls and their output; at most one per output.
    controls: Vec<(ZwlrGammaControlV1, Output)>,
}

impl GammaState {
    pub fn new(dh: &DisplayHandle) -> Self {
        Self {
            _global: dh.create_global::<Aurora, ZwlrGammaControlManagerV1, _>(1, ManagerGlobal),
            controls: Vec::new(),
        }
    }
}

impl Aurora {
    /// Entries per channel of the output's hardware ramp, `None` when there is none.
    pub fn gamma_size(&self, output: &Output) -> Option<u32> {
        match self.backend {
            Backend::Drm(_) => self.drm_gamma_size(output),
            Backend::Winit => None,
        }
    }

    fn set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> Result<(), String> {
        match self.backend {
            Backend::Drm(_) => self.drm_set_gamma(output, ramp),
            Backend::Winit => Err("the nested backend has no gamma".into()),
        }
    }

    /// Ends a control: it fails, and the output's default ramp comes back.
    fn gamma_fail(&mut self, control: &ZwlrGammaControlV1, reason: &str) {
        let Some(index) = self
            .display
            .gamma
            .controls
            .iter()
            .position(|(c, _)| c == control)
        else {
            control.failed();
            return;
        };
        let (control, output) = self.display.gamma.controls.remove(index);
        tracing::info!("gamma: output={} control failed: {reason}", output.name());
        control.failed();
        if let Err(err) = self.set_gamma(&output, None) {
            tracing::debug!("gamma: reset of {}: {err}", output.name());
        }
    }

    /// The output disappears: its control fails (the hardware ramp goes with the CRTC).
    pub(super) fn gamma_output_removed(&mut self, output: &Output) {
        self.display.gamma.controls.retain(|(control, o)| {
            if o == output {
                control.failed();
                false
            } else {
                true
            }
        });
    }
}

pub struct ManagerGlobal;
pub struct Manager;
pub struct Control;

impl GlobalDispatch2<ZwlrGammaControlManagerV1, Aurora> for ManagerGlobal {
    fn bind(
        &self,
        _state: &mut Aurora,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        data_init.init(resource, Manager);
    }
}

impl Dispatch2<ZwlrGammaControlManagerV1, Aurora> for Manager {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Aurora>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };
        let control = data_init.init(id, Control);
        let Some(output) = Output::from_resource(&output).filter(|o| state.wm.outputs.contains(o))
        else {
            control.failed();
            return;
        };
        let name = output.name();
        if state
            .display
            .gamma
            .controls
            .iter()
            .any(|(_, o)| *o == output)
        {
            tracing::info!("gamma: output={name} already has a control, refusing another");
            control.failed();
            return;
        }
        let Some(size) = state.gamma_size(&output) else {
            tracing::info!("gamma: output={name} has no hardware gamma, refusing");
            control.failed();
            return;
        };
        tracing::info!("gamma: output={name} control size={size}");
        control.gamma_size(size);
        state.display.gamma.controls.push((control, output));
    }
}

impl Dispatch2<ZwlrGammaControlV1, Aurora> for Control {
    fn request(
        &self,
        state: &mut Aurora,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Aurora>,
    ) {
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = request else {
            return;
        };
        // A failed control is inert.
        let Some(output) = state
            .display
            .gamma
            .controls
            .iter()
            .find(|(c, _)| c == resource)
            .map(|(_, o)| o.clone())
        else {
            return;
        };
        let Some(size) = state.gamma_size(&output) else {
            state.gamma_fail(resource, "no hardware gamma any more");
            return;
        };
        let size = size as usize;
        let bytes = match read_table(fd, size * 6) {
            Ok(bytes) => bytes,
            Err(err) => {
                state.gamma_fail(resource, &format!("cannot read the table: {err}"));
                return;
            }
        };
        let Some(ramp) = parse_ramp(&bytes, size) else {
            resource.post_error(
                zwlr_gamma_control_v1::Error::InvalidGamma,
                format!(
                    "the gamma table has {} bytes, expected {}",
                    bytes.len(),
                    size * 6
                ),
            );
            return;
        };
        if let Err(err) = state.set_gamma(&output, Some(ramp)) {
            state.gamma_fail(resource, &err);
        }
    }

    fn destroyed(&self, state: &mut Aurora, _client: ClientId, resource: &ZwlrGammaControlV1) {
        let gamma = &mut state.display.gamma;
        let Some(index) = gamma.controls.iter().position(|(c, _)| c == resource) else {
            return;
        };
        let (_, output) = gamma.controls.remove(index);
        tracing::info!("gamma: output={} control gone, default ramp", output.name());
        if let Err(err) = state.set_gamma(&output, None) {
            tracing::warn!("gamma: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(values: &[u16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    #[test]
    fn ramp_must_be_exactly_three_tables() {
        let table = [1, 2, 3, 4, 5, 6];
        assert_eq!(parse_ramp(&bytes(&table), 2), Some(table.to_vec()));
        assert_eq!(parse_ramp(&bytes(&table[..5]), 2), None, "short");
        assert_eq!(parse_ramp(&bytes(&[0; 7]), 2), None, "long");
        assert_eq!(parse_ramp(&bytes(&table)[..11], 2), None, "odd bytes");
        assert_eq!(parse_ramp(&[], 0), None, "no size");
        assert_eq!(parse_ramp(&[], usize::MAX), None, "overflow");
    }

    #[test]
    fn lut_interleaves_channels_with_a_reserved_word() {
        let lut = lut_bytes(&[1, 2, 10, 20, 100, 200], 2).expect("valid ramp");
        assert_eq!(lut, bytes(&[1, 10, 100, 0, 2, 20, 200, 0]));
        assert_eq!(lut_bytes(&[1, 2, 3], 2), None);
    }

    #[test]
    fn linear_ramp_spans_the_full_range_per_channel() {
        let ramp = linear_ramp(256);
        assert_eq!(ramp.len(), 768);
        for channel in ramp.chunks(256) {
            assert_eq!((channel[0], channel[255]), (0, u16::MAX));
            assert!(channel.windows(2).all(|w| w[0] < w[1]));
        }
        assert_eq!(linear_ramp(1), [u16::MAX; 3]);
    }
}
