use std::convert::TryInto;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use futures::{future::poll_fn, ready};
use tokio::time::timeout;

use crate::flow::*;

enum ForwardState {
    AwatingSizeHint,
    PollingTxBuf(SizeHint),
    PollingRxBuf { offset: usize },
    Closing,
    Done,
}

struct StreamForward<'l, 'r> {
    stream_local: Pin<&'l mut dyn Stream>,
    stream_remote: Pin<&'r mut dyn Stream>,
    uplink_state: ForwardState,
    downlink_state: ForwardState,
}

fn poll_forward_oneway(
    cx: &mut Context<'_>,
    mut rx: Pin<&mut dyn Stream>,
    mut tx: Pin<&mut dyn Stream>,
    state: &mut ForwardState,
) -> Poll<FlowResult<()>> {
    loop {
        *state = match state {
            ForwardState::AwatingSizeHint => {
                let size_hint = ready!(rx.as_mut().poll_request_size(cx))?;
                ForwardState::PollingTxBuf(size_hint)
            }
            ForwardState::PollingTxBuf(size_hint) => {
                let (buf, offset) = ready!(tx
                    .as_mut()
                    .poll_tx_buffer(cx, size_hint.with_min_content(4096).try_into().unwrap()))?;
                if let Err((mut buf, e)) = rx.as_mut().commit_rx_buffer(buf, offset) {
                    // Return buffer
                    buf.resize(offset, 0);
                    let _ = tx.as_mut().commit_tx_buffer(buf);
                    Err(e)?;
                }
                ForwardState::PollingRxBuf { offset }
            }
            ForwardState::PollingRxBuf { offset } => match ready!(rx.as_mut().poll_rx_buffer(cx)) {
                Ok(buf) => {
                    tx.as_mut().commit_tx_buffer(buf)?;
                    ForwardState::AwatingSizeHint
                }
                Err((mut buf, FlowError::Eof)) => {
                    // Return buffer
                    buf.resize(*offset, 0);
                    tx.as_mut().commit_tx_buffer(buf)?;
                    ForwardState::Closing
                }
                Err((mut buf, e)) => {
                    // Return buffer
                    buf.resize(*offset, 0);
                    let _ = tx.as_mut().commit_tx_buffer(buf);
                    return Poll::Ready(Err(e));
                }
            },
            ForwardState::Closing => {
                ready!(tx.as_mut().poll_close_tx(cx))?;
                ForwardState::Done
            }
            ForwardState::Done => return Poll::Ready(Ok(())),
        }
    }
}

impl<'l, 'r> Future for StreamForward<'l, 'r> {
    type Output = FlowResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            stream_local,
            stream_remote,
            uplink_state,
            downlink_state,
        } = &mut *self;
        match (
            poll_forward_oneway(
                cx,
                stream_remote.as_mut(),
                stream_local.as_mut(),
                downlink_state,
            ),
            poll_forward_oneway(
                cx,
                stream_local.as_mut(),
                stream_remote.as_mut(),
                uplink_state,
            ),
        ) {
            (Poll::Ready(Ok(())), Poll::Ready(Ok(()))) => Poll::Ready(Ok(())),
            (Poll::Ready(Err(e)), _) | (_, Poll::Ready(Err(e))) => Poll::Ready(Err(e)),
            _ => Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct StreamForwardHandler {
    pub outbound: Weak<dyn StreamOutboundFactory>,
}

pub struct DatagramForwardHandler {
    pub outbound: Weak<dyn DatagramSessionFactory>,
}

impl StreamForwardHandler {
    async fn handle_stream(
        outbound_factory: Arc<dyn StreamOutboundFactory>,
        mut lower: Pin<Box<dyn Stream>>,
        context: Box<FlowContext>,
    ) -> FlowResult<()> {
        let mut initial_uplink_state = ForwardState::AwatingSizeHint;
        let initial_data = timeout(tokio::time::Duration::from_millis(100), async {
            let size = crate::get_request_size_boxed!(lower)?;
            initial_uplink_state = ForwardState::PollingTxBuf(size);
            let buf = vec![0; size.with_min_content(1500)];
            lower
                .as_mut()
                .commit_rx_buffer(buf, 0)
                .map_err(|(_, e)| e)?;
            let initial_data = crate::get_rx_buffer_boxed!(lower).map_err(|(_, e)| e);
            initial_uplink_state = ForwardState::AwatingSizeHint;
            initial_data
        })
        .await
        .ok()
        .transpose()?;

        // TODO: outbound handshake timeout
        let initial_data_ref = initial_data.as_deref().unwrap_or(&[]);
        let outbound = outbound_factory
            .create_outbound(context, initial_data_ref)
            .await;
        drop(initial_data);
        let mut outbound = match outbound {
            Ok(outbound) => outbound,
            Err(e) => {
                // TODO: log error
                // Shutdown inbound normally since it is the outbound that faults.
                // Be careful not to trigger drainage etc. for the inbound in this case.
                return crate::close_tx_boxed!(lower).and_then(|()| Err(e))?;
            }
        };

        let mut initial_downlink_state = ForwardState::AwatingSizeHint;
        if let ForwardState::PollingTxBuf(_) = initial_uplink_state {
            // If lower failed to fill initial data, try to extract the temporary
            // buffer out, and forward downlink at the same time.
            poll_fn(|cx| {
                match lower.as_mut().poll_rx_buffer(cx) {
                    Poll::Ready(Ok(_)) => return Poll::Ready(Ok(())),
                    Poll::Ready(Err((_, e))) => return Poll::Ready(Err(e)),
                    _ => {}
                }
                if let r @ Poll::Ready(Err(_)) = poll_forward_oneway(
                    cx,
                    outbound.as_mut(),
                    lower.as_mut(),
                    &mut initial_downlink_state,
                ) {
                    return r;
                };
                Poll::Pending
            })
            .await?;
        }

        // Drop earlier to prevent StreamForward outliving outbound
        let _ = StreamForward {
            stream_local: lower.as_mut(),
            stream_remote: outbound.as_mut(),
            downlink_state: initial_downlink_state,
            uplink_state: initial_uplink_state,
        }
        .await?;
        Ok(())
    }
}

impl StreamHandler for StreamForwardHandler {
    fn on_stream(&self, lower: Pin<Box<dyn Stream>>, context: Box<FlowContext>) {
        if let Some(outbound) = self.outbound.upgrade() {
            tokio::spawn(Self::handle_stream(outbound, lower, context));
        }
    }
}

impl DatagramSessionHandler for DatagramForwardHandler {
    fn on_session(&self, mut session: Pin<Box<dyn DatagramSession>>, context: Box<FlowContext>) {
        let outbound = match self.outbound.upgrade() {
            Some(o) => o,
            None => return,
        };
        tokio::spawn(async move {
            let mut lower = outbound.bind(context).await?;
            let mut uplink_buf = None;
            let mut downlink_buf = None;
            poll_fn(|cx| {
                #[allow(unreachable_code)]
                loop {
                    if let Some((addr, buf)) = uplink_buf.take() {
                        match lower.as_mut().poll_send_ready(cx) {
                            Poll::Ready(()) => (lower.as_mut().send_to(addr, buf), continue).1,
                            Poll::Pending => uplink_buf = Some((addr, buf)),
                        }
                    } else {
                        match session.as_mut().poll_recv_from(cx) {
                            Poll::Ready(b @ Some(_)) => (uplink_buf = b, continue).1,
                            Poll::Ready(None) => return Poll::Ready(()),
                            Poll::Pending => {}
                        }
                    }
                    if let Some((addr, buf)) = downlink_buf.take() {
                        match session.as_mut().poll_send_ready(cx) {
                            Poll::Ready(()) => (session.as_mut().send_to(addr, buf), continue).1,
                            Poll::Pending => downlink_buf = Some((addr, buf)),
                        }
                    } else {
                        match lower.as_mut().poll_recv_from(cx) {
                            Poll::Ready(b @ Some(_)) => (downlink_buf = b, continue).1,
                            Poll::Ready(None) => return Poll::Ready(()),
                            Poll::Pending => break,
                        }
                    }
                }
                Poll::Pending
            })
            .await;
            futures::future::try_join(
                poll_fn(|cx| lower.as_mut().poll_shutdown(cx)),
                poll_fn(|cx| session.as_mut().poll_shutdown(cx)),
            )
            .await?;
            FlowResult::Ok(())
        });
    }
}
