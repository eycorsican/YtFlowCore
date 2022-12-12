mod bind;
mod dns;
mod ffi;

pub use bind::{bind_socket_v4, bind_socket_v6};
pub use dns::Resolver;

use std::ffi::{c_char, CStr, CString};
use std::io;
use std::sync::Mutex;

use block2::ConcreteBlock;
use fruity::core::Arc as ObjcArc;
use fruity::dispatch::{DispatchQueue, DispatchQueueAttributes, DispatchQueueBuilder};
use fruity::objc::NSObject;
use serde::Serialize;

use self::ffi::nw_interface_get_name;

use super::super::*;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Netif {
    pub name: String,
    /// Always None because we rely on its BSD name to bind a socket, not IP address
    pub ipv4_addr: Option<SocketAddrV4>,
    /// Always None because we rely on its BSD name to bind a socket, not IP address
    pub ipv6_addr: Option<SocketAddrV6>,
    /// Only has values on Windows. On Linux and macOS we just forward DNS requests to systemd-resolved and
    /// DNSServiceGetAddrInfo since we can specify which interface to query DNS on.
    #[serde(serialize_with = "serialize_ipaddrs")]
    pub dns_servers: Vec<IpAddr>,

    pub bsd_name: CString,
}

impl Netif {
    fn get_idx(&self) -> io::Result<libc::c_uint> {
        let idx: libc::c_uint = unsafe { libc::if_nametoindex(self.bsd_name.as_ptr()) };
        if idx == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(idx)
    }
}

pub struct NetifProvider {
    best_if_bsd_name: Arc<Mutex<CString>>,
    _dispatch_queue: ObjcArc<DispatchQueue>,
    _monitor: ObjcArc<NSObject<'static>>,
}

impl NetifProvider {
    pub fn new<C: Fn() + Clone + Send + 'static>(callback: C) -> NetifProvider {
        let dispatch_queue = DispatchQueueBuilder::new()
            .label(CStr::from_bytes_with_nul(b"com.bdbai.ytflow.core.netifprovider\0").unwrap())
            .attr(DispatchQueueAttributes::SERIAL) // Ensure race-free access to context pointer
            .build();
        let best_if_bsd_name = Arc::new(Mutex::new(CString::new("").unwrap()));
        let monitor = unsafe { ObjcArc::from_raw(ffi::nw_path_monitor_create()) };
        unsafe {
            let monitor_ptr = &*monitor as *const _ as _;
            let best_if_bsd_name = best_if_bsd_name.clone();
            let block = block2::ConcreteBlock::new(move |path_ptr: usize| {
                unsafe {
                    let best_if_bsd_name = best_if_bsd_name.clone();
                    let enum_block = ConcreteBlock::new(move |if_ptr: usize| -> c_char {
                        unsafe {
                            let name_ptr = nw_interface_get_name(if_ptr as _);
                            *best_if_bsd_name.lock().unwrap() = CStr::from_ptr(name_ptr).to_owned();
                        }
                        false as _
                    })
                    .copy();
                    ffi::nw_path_enumerate_interfaces(
                        path_ptr as *mut _,
                        &*enum_block as *const _ as _,
                    );
                }

                callback();
            })
            .copy();
            // TODO: set forbid type
            ffi::nw_path_monitor_prohibit_interface_type(monitor_ptr, ffi::nw_interface_type_other);
            ffi::nw_path_monitor_prohibit_interface_type(
                monitor_ptr,
                ffi::nw_interface_type_loopback,
            );
            ffi::nw_path_monitor_set_update_handler(monitor_ptr, &*block as *const _ as _);
            ffi::nw_path_monitor_set_queue(monitor_ptr, &*dispatch_queue as *const _ as _);
            ffi::nw_path_monitor_start(monitor_ptr);
        };
        Self {
            best_if_bsd_name,
            _dispatch_queue: dispatch_queue,
            _monitor: monitor,
        }
    }

    fn select_bsd(name: CString) -> Netif {
        // TODO: get localized name on macOS
        Netif {
            name: name.to_string_lossy().to_string(),
            ipv4_addr: None,
            ipv6_addr: None,
            dns_servers: vec![],
            bsd_name: name,
        }
    }

    pub fn select(&self, name: &str) -> Option<Netif> {
        Some(Self::select_bsd(CString::new(name).ok()?))
    }

    pub fn select_best(&self) -> Option<Netif> {
        Some(Self::select_bsd(
            self.best_if_bsd_name.lock().unwrap().clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let _provider = Arc::new_cyclic(|this| {
                let this: Weak<NetifProvider> = this.clone();
                NetifProvider::new(move || {
                    if let Some(this) = this.upgrade() {
                        println!("{:?}", this.select("en0"));
                        println!("{:?}", this.select_best());
                        let _ = tx.try_send(());
                    }
                })
            });
            let _ = rx.recv().await;
        })
    }
}
