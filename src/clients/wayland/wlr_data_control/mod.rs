pub mod device;
pub mod manager;
pub mod offer;
pub mod source;

use self::device::{DataControlDeviceDataExt, DataControlDeviceHandler};
use self::offer::{DataControlDeviceOffer, DataControlOfferHandler, SelectionOffer};
use self::source::DataControlSourceHandler;
use crate::clients::wayland::Environment;
use crate::unique_id::get_unique_usize;
use crate::{lock, send};
use device::DataControlDevice;
use glib::Bytes;
use nix::fcntl::{fcntl, F_GETPIPE_SZ, F_SETPIPE_SZ};
use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::reexports::calloop::RegistrationToken;
use std::cmp::min;
use std::fmt::{Debug, Formatter};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::{fs, io};
use tracing::{debug, error, trace};
use wayland_client::{Connection, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;

const INTERNAL_MIME_TYPE: &str = "x-ironbar-internal";

pub struct SelectionOfferItem {
    offer: SelectionOffer,
    token: Option<RegistrationToken>,
}

#[derive(Debug, Clone, Eq)]
pub struct ClipboardItem {
    pub id: usize,
    pub value: ClipboardValue,
    pub mime_type: String,
}

impl PartialEq<Self> for ClipboardItem {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ClipboardValue {
    Text(String),
    Image(Bytes),
    Other,
}

impl Debug for ClipboardValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Text(text) => text.clone(),
                Self::Image(bytes) => {
                    format!("[{} Bytes]", bytes.len())
                }
                Self::Other => "[Unknown]".to_string(),
            }
        )
    }
}

#[derive(Debug)]
struct MimeType {
    value: String,
    category: MimeTypeCategory,
}

#[derive(Debug)]
enum MimeTypeCategory {
    Text,
    Image,
}

impl MimeType {
    fn parse(mime_type: &str) -> Option<Self> {
        match mime_type.to_lowercase().as_str() {
            "text"
            | "string"
            | "utf8_string"
            | "text/plain"
            | "text/plain;charset=utf-8"
            | "text/plain;charset=iso-8859-1"
            | "text/plain;charset=us-ascii"
            | "text/plain;charset=unicode" => Some(Self {
                value: mime_type.to_string(),
                category: MimeTypeCategory::Text,
            }),
            "image/png" | "image/jpg" | "image/jpeg" | "image/tiff" | "image/bmp"
            | "image/x-bmp" | "image/icon" => Some(Self {
                value: mime_type.to_string(),
                category: MimeTypeCategory::Image,
            }),
            _ => None,
        }
    }

    fn parse_multiple(mime_types: &[String]) -> Option<Self> {
        mime_types.iter().find_map(|mime| Self::parse(mime))
    }
}

impl Environment {
    pub fn copy_to_clipboard(&mut self, item: Arc<ClipboardItem>, qh: &QueueHandle<Self>) {
        debug!("Copying item to clipboard: {item:?}");

        // TODO: Proper device tracking
        let device = self.data_control_devices.first();
        if let Some(device) = device {
            let source = self
                .data_control_device_manager_state
                .create_copy_paste_source(qh, [INTERNAL_MIME_TYPE, item.mime_type.as_str()]);

            source.set_selection(&device.device);
            self.copy_paste_sources.push(source);

            lock!(self.clipboard).replace(item);
        }
    }

    fn read_file(mime_type: &MimeType, file: &mut File) -> io::Result<ClipboardItem> {
        let value = match mime_type.category {
            MimeTypeCategory::Text => {
                let mut txt = String::new();
                file.read_to_string(&mut txt)?;

                ClipboardValue::Text(txt)
            }
            MimeTypeCategory::Image => {
                let mut bytes = vec![];
                file.read_to_end(&mut bytes)?;
                let bytes = Bytes::from(&bytes);

                ClipboardValue::Image(bytes)
            }
        };

        Ok(ClipboardItem {
            id: get_unique_usize(),
            value,
            mime_type: mime_type.value.clone(),
        })
    }
}

impl DataControlDeviceHandler for Environment {
    fn selection(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        data_device: DataControlDevice,
    ) {
        debug!("Handler received selection event");

        let mime_types = data_device.selection_mime_types();

        if mime_types.contains(&INTERNAL_MIME_TYPE.to_string()) {
            return;
        }

        if let Some(offer) = data_device.selection_offer() {
            self.selection_offers
                .push(SelectionOfferItem { offer, token: None });

            let cur_offer = self
                .selection_offers
                .last_mut()
                .expect("Failed to get current offer");

            let Some(mime_type) = MimeType::parse_multiple(&mime_types) else {
                lock!(self.clipboard).take();
                // send an event so the clipboard module is aware it's changed
                send!(
                    self.clipboard_tx,
                    Arc::new(ClipboardItem {
                        id: usize::MAX,
                        mime_type: String::new(),
                        value: ClipboardValue::Other
                    })
                );
                return;
            };

            if let Ok(read_pipe) = cur_offer.offer.receive(mime_type.value.clone()) {
                let offer_clone = cur_offer.offer.clone();

                let tx = self.clipboard_tx.clone();
                let clipboard = self.clipboard.clone();

                let token = self
                    .loop_handle
                    .insert_source(read_pipe, move |_, file, state| {
                        let item = state
                            .selection_offers
                            .iter()
                            .position(|o| o.offer == offer_clone)
                            .map(|p| state.selection_offers.remove(p))
                            .expect("Failed to find selection offer item");

                        match Self::read_file(&mime_type, file) {
                            Ok(item) => {
                                let item = Arc::new(item);
                                lock!(clipboard).replace(item.clone());
                                send!(tx, item);
                            }
                            Err(err) => error!("{err:?}"),
                        }

                        state
                            .loop_handle
                            .remove(item.token.expect("Missing item token"));
                    });

                match token {
                    Ok(token) => {
                        cur_offer.token.replace(token);
                    }
                    Err(err) => error!("{err:?}"),
                }
            }
        }
    }
}

impl DataControlOfferHandler for Environment {
    fn offer(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _offer: &mut DataControlDeviceOffer,
        _mime_type: String,
    ) {
        debug!("Handler received offer");
    }
}

impl DataControlSourceHandler for Environment {
    fn accept_mime(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &ZwlrDataControlSourceV1,
        mime: Option<String>,
    ) {
        debug!("Accepted mime type: {mime:?}");
    }

    fn send_request(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        source: &ZwlrDataControlSourceV1,
        mime: String,
        write_pipe: WritePipe,
    ) {
        debug!("Handler received source send request event ({mime})");

        if let Some(item) = lock!(self.clipboard).clone() {
            let fd = OwnedFd::from(write_pipe);
            if self
                .copy_paste_sources
                .iter_mut()
                .any(|s| s.inner() == source && MimeType::parse(&mime).is_some())
            {
                trace!("Source found, writing to file");

                let mut bytes = match &item.value {
                    ClipboardValue::Text(text) => text.as_bytes(),
                    ClipboardValue::Image(bytes) => bytes.as_ref(),
                    ClipboardValue::Other => panic!(
                        "{:?}",
                        io::Error::new(ErrorKind::Other, "Attempted to copy unsupported mime type",)
                    ),
                };

                let pipe_size = set_pipe_size(fd.as_raw_fd(), bytes.len())
                    .expect("Failed to increase pipe size");
                let mut file = File::from(fd.try_clone().expect("Failed to clone fd"));

                trace!("Num bytes: {}", bytes.len());

                let mut events = (0..16).map(|_| EpollEvent::empty()).collect::<Vec<_>>();
                let mut epoll_event = EpollEvent::new(EpollFlags::EPOLLOUT, 0);

                let epoll_fd = epoll_create().unwrap();
                epoll_ctl(
                    epoll_fd,
                    EpollOp::EpollCtlAdd,
                    fd.as_raw_fd(),
                    &mut epoll_event,
                )
                .unwrap();

                while !bytes.is_empty() {
                    let chunk = &bytes[..min(pipe_size as usize, bytes.len())];

                    trace!("Writing {} bytes ({} remain)", chunk.len(), bytes.len());

                    epoll_wait(epoll_fd, &mut events, 100).expect("Failed to wait to epoll");

                    match file.write(chunk) {
                        Ok(_) => bytes = &bytes[chunk.len()..],
                        Err(err) => {
                            error!("{err:?}");
                            break;
                        }
                    }
                }

                // for chunk in bytes.chunks(pipe_size as usize) {
                //     trace!("Writing chunk");
                //     file.write(chunk).expect("Failed to write chunk to buffer");
                //     file.flush().expect("Failed to flush to file");
                // }

                // match file.write_vectored(&bytes.chunks(pipe_size as usize).map(IoSlice::new).collect::<Vec<_>>()) {
                //     Ok(_) => debug!("Copied item"),
                //     Err(err) => error!("{err:?}"),
                // }

                // match file.write_all(bytes) {
                //     Ok(_) => debug!("Copied item"),
                //     Err(err) => error!("{err:?}"),
                // }
            } else {
                error!("Failed to find source");
            }
        }
    }

    fn cancelled(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        source: &ZwlrDataControlSourceV1,
    ) {
        debug!("Handler received source cancelled event");

        self.copy_paste_sources
            .iter()
            .position(|s| s.inner() == source)
            .map(|pos| self.copy_paste_sources.remove(pos));
        source.destroy();
    }
}

/// Attempts to increase the fd pipe size to the requested number of bytes.
/// The kernel will automatically round this up to the nearest page size.
/// If the requested size is larger than the kernel max (normally 1MB),
/// it will be clamped at this.
///
/// Returns the new size if succeeded
fn set_pipe_size(fd: RawFd, size: usize) -> io::Result<i32> {
    // clamp size at kernel max
    let max_pipe_size = fs::read_to_string("/proc/sys/fs/pipe-max-size")
        .expect("Failed to find pipe-max-size virtual kernel file")
        .trim()
        .parse::<usize>()
        .expect("Failed to parse pipe-max-size contents");

    let size = min(size, max_pipe_size);

    let curr_size = fcntl(fd, F_GETPIPE_SZ)? as usize;

    trace!("Current pipe size: {curr_size}");

    let new_size = if size > curr_size {
        trace!("Requesting pipe size increase to (at least): {size}");
        let res = fcntl(fd, F_SETPIPE_SZ(size as i32))?;
        trace!("New pipe size: {res}");
        if res < size as i32 {
            return Err(io::Error::last_os_error());
        }
        res
    } else {
        size as i32
    };

    Ok(new_size)
}
