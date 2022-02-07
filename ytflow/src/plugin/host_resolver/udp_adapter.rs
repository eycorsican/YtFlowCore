use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;

use std::sync::Weak;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt};
use futures::ready;
use parking_lot::{const_mutex, const_rwlock, Mutex, RwLock};
use trust_dns_resolver::name_server::{RuntimeProvider, TokioRuntime};
use trust_dns_resolver::proto::udp::UdpSocket;

use crate::flow::*;

pub static UDP_FACTORIES: RwLock<(u32, BTreeMap<u32, Weak<dyn DatagramSessionFactory>>)> =
    const_rwlock((0, BTreeMap::new()));

enum SessionState {
    Binding(BoxFuture<'static, FlowResult<Box<dyn DatagramSession>>>),
    Ready(Box<dyn DatagramSession>),
}

pub struct FlowDatagramSocket(Mutex<Option<(u32, SessionState)>>);

#[async_trait]
impl UdpSocket for FlowDatagramSocket {
    /// Time implementation used for this type
    type Time = <TokioRuntime as RuntimeProvider>::Timer;

    /// UdpSocket
    async fn bind(_addr: SocketAddr) -> io::Result<Self> {
        Ok(FlowDatagramSocket(const_mutex(None)))
    }

    /// Poll once Receive data from the socket and returns the number of bytes read and the address from
    /// where the data came on success.
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let mut guard = self.0.lock();
        let (index, session) = loop {
            match &mut *guard {
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "UDP socket is not connect for DNS recv",
                    )))
                }
                Some((index, SessionState::Binding(fut))) => {
                    let fut_res = match ready!(fut.as_mut().poll(cx)) {
                        Ok(r) => r,
                        Err(_) => Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "Cannot bind UDP socket for DNS",
                        ))?,
                    };
                    *guard = Some((*index, SessionState::Ready(fut_res)));
                    continue;
                }
                Some((index, SessionState::Ready(session))) => break (index, session),
            }
        };

        let (_dest, chunk) = ready!(session.as_mut().poll_recv_from(cx)).ok_or(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "UDP recv error",
        ))?;
        buf[..chunk.len()].copy_from_slice(&chunk);
        // Cheat trust_dns_resolver as if the packet comes from the remote peer
        let dest = SocketAddr::new(index.to_ne_bytes().into(), 53);
        Poll::Ready(Ok((chunk.len(), dest)))
    }

    /// Poll once to send data to the given address.
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let mut guard = self.0.lock();
        if let (None, SocketAddr::V4(addrv4)) = (&*guard, target) {
            let index = u32::from_ne_bytes(addrv4.ip().octets());
            let factories_guard = UDP_FACTORIES.read();
            let factory = factories_guard
                .1
                .get(&index)
                .ok_or(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Cannot find UDP factory for DNS",
                ))?
                .upgrade()
                .ok_or(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Cannot get UDP factory for DNS",
                ))?;
            drop(factories_guard);
            *guard = Some((
                index,
                SessionState::Binding(
                    async move {
                        factory
                            .bind(Box::new(FlowContext {
                                local_peer: target,
                                // 让下一层 redirect，这里直接摆烂
                                remote_peer: DestinationAddr {
                                    host: HostName::Ip((*addrv4.ip()).into()),
                                    port: 53,
                                },
                            }))
                            .await
                    }
                    .boxed(),
                ),
            ));
        }
        let session = loop {
            match &mut *guard {
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "UDP socket is not connect for DNS recv",
                    )))
                }
                Some((index, SessionState::Binding(fut))) => {
                    let fut_res = match ready!(fut.as_mut().poll(cx)) {
                        Ok(r) => r,
                        Err(_) => Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "Cannot bind UDP socket for DNS",
                        ))?,
                    };
                    *guard = Some((*index, SessionState::Ready(fut_res)));
                    continue;
                }
                Some((_, SessionState::Ready(session))) => break session,
            }
        };
        ready!(session.as_mut().poll_send_ready(cx));
        session.as_mut().send_to(
            DestinationAddr {
                host: HostName::Ip(target.ip()),
                port: target.port(),
            },
            buf.to_vec(),
        );
        Poll::Ready(Ok(buf.len()))
    }
}

impl Drop for FlowDatagramSocket {
    fn drop(&mut self) {
        let sess = {
            let mut guard = self.0.lock();
            match guard.take() {
                Some((_, sess)) => sess,
                None => return,
            }
        };
        if let SessionState::Ready(mut s) = sess {
            tokio::spawn(async move {
                let _ = futures::future::poll_fn(|cx| s.as_mut().poll_shutdown(cx)).await;
            });
        }
    }
}
