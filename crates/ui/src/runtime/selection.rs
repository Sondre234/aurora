//! Clipboard (`wl_data_device`) and primary selection (`zwp_primary_selection_v1`).
//!
//! Offering: [`Runtime::set_text`] / [`Runtime::set_selection`] keep the bytes in the
//! runtime and serve every `send` request from a calloop source that writes at most one
//! pipe-buffer per wakeup, so a slow or dead reader never blocks the loop.
//!
//! Receiving: [`Runtime::read_selection`] asks the current offer for a mime type and
//! collects the answer on a calloop source; [`Event::SelectionData`] arrives once the
//! sender closed the pipe (capped at [`MAX_PASTE`]). Nothing here blocks.
//!
//! Setting a selection needs the serial of a recent input event of this client, which the
//! runtime tracks; call it from the key or pointer handler that triggered the copy.

use std::io::{self, Read, Write};
use std::sync::Arc;

use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::data_device_manager::data_device::DataDeviceHandler;
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{
    CopyPasteSource, DataSourceHandler,
};
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDeviceHandler;
use smithay_client_toolkit::primary_selection::selection::{
    PrimarySelectionSource, PrimarySelectionSourceHandler,
};
use smithay_client_toolkit::reexports::calloop::PostAction;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device::WlDataDevice;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device_manager::DndAction;
use smithay_client_toolkit::reexports::client::protocol::wl_data_source::WlDataSource;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};

use super::{App, Error, Event, Runtime, State};

/// Largest selection the runtime accepts from another client.
pub const MAX_PASTE: usize = 16 << 20;

/// Bytes written to a pipe per wakeup: a pipe accepts this much without blocking.
const WRITE_CHUNK: usize = 4096;

/// Which selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Selection {
    /// Ctrl+C / Ctrl+V.
    Clipboard,
    /// Selecting text, pasted with the middle button.
    Primary,
}

/// One representation of the data being offered: a mime type and its bytes.
pub type Offer = (String, Arc<[u8]>);

/// Text mime types in order of preference when reading.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

/// The best text mime type among `offered`, if any.
pub fn pick_text_mime(offered: &[String]) -> Option<&str> {
    TEXT_MIMES
        .iter()
        .find_map(|want| offered.iter().find(|m| m.eq_ignore_ascii_case(want)))
        .map(String::as_str)
}

/// Offers carrying `text` under every common text mime type.
pub fn text_offers(text: &str) -> Vec<Offer> {
    let bytes: Arc<[u8]> = text.as_bytes().into();
    TEXT_MIMES
        .iter()
        .map(|m| (m.to_string(), bytes.clone()))
        .collect()
}

/// The bytes to send for a requested mime type (case-insensitive).
fn find_offer<'a>(offers: &'a [Offer], mime: &str) -> Option<&'a Arc<[u8]>> {
    offers
        .iter()
        .find(|(m, _)| m.eq_ignore_ascii_case(mime))
        .map(|(_, d)| d)
}

/// What this client currently offers on one selection.
pub(super) enum Owned {
    Clipboard(CopyPasteSource, Vec<Offer>),
    Primary(PrimarySelectionSource, Vec<Offer>),
}

impl Owned {
    fn offers(&self) -> &[Offer] {
        match self {
            Owned::Clipboard(_, o) | Owned::Primary(_, o) => o,
        }
    }
}

impl<A: App> Runtime<A> {
    fn owned_slot(&mut self, sel: Selection) -> &mut Option<Owned> {
        match sel {
            Selection::Clipboard => &mut self.owned_clipboard,
            Selection::Primary => &mut self.owned_primary,
        }
    }

    /// Offer plain text on a selection.
    pub fn set_text(&mut self, sel: Selection, text: &str) -> Result<(), Error> {
        self.set_selection(sel, text_offers(text))
    }

    /// Take ownership of a selection and offer `offers` (one entry per mime type).
    pub fn set_selection(&mut self, sel: Selection, offers: Vec<Offer>) -> Result<(), Error> {
        let serial = self.serial;
        let mimes: Vec<String> = offers.iter().map(|(m, _)| m.clone()).collect();
        let owned = match sel {
            Selection::Clipboard => {
                let (Some(mgr), Some(dev)) = (&self.data_mgr, &self.data_device) else {
                    return Err(Error::Global("wl_data_device_manager"));
                };
                let src = mgr.create_copy_paste_source(&self.qh, mimes);
                src.set_selection(dev, serial);
                Owned::Clipboard(src, offers)
            }
            Selection::Primary => {
                let (Some(mgr), Some(dev)) = (&self.primary_mgr, &self.primary_device) else {
                    return Err(Error::Global("zwp_primary_selection_device_manager_v1"));
                };
                let src = mgr.create_selection_source(&self.qh, mimes);
                src.set_selection(dev, serial);
                Owned::Primary(src, offers)
            }
        };
        *self.owned_slot(sel) = Some(owned);
        Ok(())
    }

    /// Give up a selection this client owns (nothing happens if it does not own it).
    pub fn clear_selection(&mut self, sel: Selection) {
        let serial = self.serial;
        if self.owned_slot(sel).take().is_none() {
            return;
        }
        match sel {
            Selection::Clipboard => {
                if let Some(d) = &self.data_device {
                    d.unset_selection(serial);
                }
            }
            Selection::Primary => {
                if let Some(d) = &self.primary_device {
                    d.unset_selection(serial);
                }
            }
        }
    }

    /// Mime types of the selection currently on offer (ours included), empty when none.
    pub fn selection_mimes(&self, sel: Selection) -> Vec<String> {
        match sel {
            Selection::Clipboard => self
                .data_device
                .as_ref()
                .and_then(|d| d.data().selection_offer())
                .map(|o| o.with_mime_types(<[String]>::to_vec)),
            Selection::Primary => self
                .primary_device
                .as_ref()
                .and_then(|d| d.data().selection_offer())
                .map(|o| o.with_mime_types(<[String]>::to_vec)),
        }
        .unwrap_or_default()
    }

    /// Ask for the data of the current selection in `mime`. The answer arrives as
    /// [`Event::SelectionData`] carrying the same `tag`; `data` is `None` when the sender
    /// failed or the selection exceeded [`MAX_PASTE`].
    pub fn read_selection(&mut self, sel: Selection, mime: &str, tag: u64) -> Result<(), Error> {
        let pipe = match sel {
            Selection::Clipboard => self
                .data_device
                .as_ref()
                .and_then(|d| d.data().selection_offer())
                .ok_or(Error::State("no clipboard offer"))?
                .receive(mime.to_string())
                .map_err(|e| Error::Shm(io::Error::other(e.to_string())))?,
            Selection::Primary => self
                .primary_device
                .as_ref()
                .and_then(|d| d.data().selection_offer())
                .ok_or(Error::State("no primary offer"))?
                .receive(mime.to_string())
                .map_err(Error::Shm)?,
        };
        // The receive request must reach the sender before it can answer.
        let _ = self.conn.flush();
        let mime = mime.to_string();
        let mut buf: Vec<u8> = Vec::new();
        let mut failed = false;
        self.loop_handle
            .insert_source(pipe, move |(), file, state| {
                let mut chunk = [0u8; 16 * 1024];
                // Readable (level-triggered): one read returns what is there or EOF.
                match (&**file).read(&mut chunk) {
                    Ok(0) => {}
                    Ok(n) => {
                        if buf.len() + n > MAX_PASTE {
                            failed = true;
                            buf = Vec::new();
                        } else if !failed {
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        return PostAction::Continue;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return PostAction::Continue;
                    }
                    Err(_) => failed = true,
                }
                let data = (!failed).then(|| std::mem::take(&mut buf));
                state.emit(Event::SelectionData {
                    selection: sel,
                    tag,
                    mime: mime.clone(),
                    data,
                });
                PostAction::Remove
            })
            .map_err(|e| Error::Loop(e.to_string()))?;
        Ok(())
    }
}

impl<A: App> State<A> {
    /// Serve a `send` request for whatever we offer on `sel`.
    fn serve(&mut self, sel: Selection, mime: &str, pipe: WritePipe) {
        let owned = match sel {
            Selection::Clipboard => &self.rt.owned_clipboard,
            Selection::Primary => &self.rt.owned_primary,
        };
        let Some(data) = owned
            .as_ref()
            .and_then(|o| find_offer(o.offers(), mime))
            .cloned()
        else {
            return; // dropping the pipe closes it: the reader sees an empty selection
        };
        let mut off = 0usize;
        let res = self.rt.loop_handle.insert_source(pipe, move |(), file, _| {
            let end = (off + WRITE_CHUNK).min(data.len());
            match (&**file).write(&data[off..end]) {
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return PostAction::Remove,
            }
            if off >= data.len() {
                PostAction::Remove
            } else {
                PostAction::Continue
            }
        });
        if let Err(e) = res {
            tracing::warn!(target: "ui", "selection send: {e}");
        }
    }

    fn lost(&mut self, sel: Selection) {
        *self.rt.owned_slot(sel) = None;
        self.emit(Event::SelectionLost { selection: sel });
    }
}

impl<A: App> DataDeviceHandler for State<A> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &WlSurface,
    ) {
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        self.emit(Event::SelectionChanged {
            selection: Selection::Clipboard,
        });
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

/// Drag and drop is not supported; the offers are never touched.
impl<A: App> DataOfferHandler for State<A> {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl<A: App> DataSourceHandler for State<A> {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        if matches!(&self.rt.owned_clipboard, Some(Owned::Clipboard(s, _)) if s.inner() == source) {
            self.serve(Selection::Clipboard, &mime, fd);
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if matches!(&self.rt.owned_clipboard, Some(Owned::Clipboard(s, _)) if s.inner() == source) {
            self.lost(Selection::Clipboard);
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl<A: App> PrimarySelectionDeviceHandler for State<A> {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
        self.emit(Event::SelectionChanged {
            selection: Selection::Primary,
        });
    }
}

impl<A: App> PrimarySelectionSourceHandler for State<A> {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        fd: WritePipe,
    ) {
        if matches!(&self.rt.owned_primary, Some(Owned::Primary(s, _)) if s.inner() == source) {
            self.serve(Selection::Primary, &mime, fd);
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        if matches!(&self.rt.owned_primary, Some(Owned::Primary(s, _)) if s.inner() == source) {
            self.lost(Selection::Primary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn text_mime_preference_order() {
        let offered = v(&["image/png", "STRING", "text/plain", "UTF8_STRING"]);
        assert_eq!(pick_text_mime(&offered), Some("text/plain"));
        let offered = v(&["text/plain;charset=UTF-8", "text/plain"]);
        assert_eq!(pick_text_mime(&offered), Some("text/plain;charset=UTF-8"));
        assert_eq!(pick_text_mime(&v(&["image/png", "text/uri-list"])), None);
        assert_eq!(pick_text_mime(&[]), None);
    }

    #[test]
    fn text_is_offered_under_every_text_mime() {
        let o = text_offers("héllo");
        assert_eq!(o.len(), TEXT_MIMES.len());
        assert!(o.iter().all(|(_, d)| &**d == "héllo".as_bytes()));
        assert!(find_offer(&o, "utf8_string").is_some());
        assert!(find_offer(&o, "text/uri-list").is_none());
    }
}
