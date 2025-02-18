use std::net::{Ipv4Addr, Ipv6Addr};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};

use futures::future::poll_fn;
use lru::LruCache;
use tokio::sync::Semaphore;
use trust_dns_resolver::proto::op::{Message as DnsMessage, MessageType, ResponseCode};
use trust_dns_resolver::proto::rr::{RData, Record, RecordType};
use trust_dns_resolver::proto::serialize::binary::BinDecodable;

use crate::flow::*;

const CACHE_CAPAICTY: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(1024) };

pub struct DnsDatagramHandler {
    concurrency_limit: Arc<Semaphore>,
    resolver: Weak<dyn Resolver>,
    ttl: u32,
    pub(super) reverse_mapping_v4: Arc<Mutex<LruCache<Ipv4Addr, String>>>,
    pub(super) reverse_mapping_v6: Arc<Mutex<LruCache<Ipv6Addr, String>>>,
}

impl DnsDatagramHandler {
    pub fn new(concurrency_limit: usize, resolver: Weak<dyn Resolver>, ttl: u32) -> Self {
        let concurrency_limit = Arc::new(Semaphore::new(concurrency_limit));
        DnsDatagramHandler {
            concurrency_limit,
            resolver,
            ttl,
            reverse_mapping_v4: Arc::new(Mutex::new(LruCache::new(CACHE_CAPAICTY))),
            reverse_mapping_v6: Arc::new(Mutex::new(LruCache::new(CACHE_CAPAICTY))),
        }
    }
}

impl DatagramSessionHandler for DnsDatagramHandler {
    fn on_session(&self, mut session: Box<dyn DatagramSession>, _context: Box<FlowContext>) {
        let resolver = match self.resolver.upgrade() {
            Some(resolver) => resolver,
            None => return,
        };
        let concurrency_limit = self.concurrency_limit.clone();
        let ttl = self.ttl;
        let reverse_mapping_v4 = self.reverse_mapping_v4.clone();
        let reverse_mapping_v6 = self.reverse_mapping_v6.clone();
        tokio::spawn(async move {
            let mut send_ready = true;
            while let Some((dest, buf)) = poll_fn(|cx| {
                if !send_ready {
                    send_ready = session.as_mut().poll_send_ready(cx).is_ready()
                }
                session.as_mut().poll_recv_from(cx)
            })
            .await
            {
                let _concurrency_permit = match concurrency_limit.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };

                let mut msg = match DnsMessage::from_bytes(&buf) {
                    Ok(msg) => msg,
                    Err(_) => continue,
                };
                let mut res_code = ResponseCode::NoError;
                let mut ans_records = Vec::with_capacity(msg.queries().len());
                for query in msg.queries() {
                    let name = query.name();
                    let name_str = name.to_lowercase().to_ascii();
                    #[allow(unreachable_code)]
                    match query.query_type() {
                        RecordType::A => {
                            let ips = match resolver.resolve_ipv4(name_str.clone()).await {
                                Ok(addrs) => addrs,
                                Err(_) => (res_code = ResponseCode::NXDomain, continue).1,
                            };
                            let mut reverse_mapping = reverse_mapping_v4.lock().unwrap();
                            for ip in &ips {
                                reverse_mapping.get_or_insert(*ip, || name_str.clone());
                            }
                            ans_records.extend(
                                ips.into_iter().map(|addr| {
                                    Record::from_rdata(name.clone(), ttl, RData::A(addr))
                                }),
                            )
                        }
                        RecordType::AAAA => {
                            let ips = match resolver.resolve_ipv6(name_str.clone()).await {
                                Ok(addrs) => addrs,
                                Err(_) => (res_code = ResponseCode::NXDomain, continue).1,
                            };
                            let mut reverse_mapping = reverse_mapping_v6.lock().unwrap();
                            for ip in &ips {
                                reverse_mapping.get_or_insert(*ip, || name_str.clone());
                            }
                            ans_records.extend(ips.into_iter().map(|addr| {
                                Record::from_rdata(name.clone(), ttl, RData::AAAA(addr))
                            }))
                        }
                        // TODO: SRV
                        _ => (res_code = ResponseCode::NotImp, continue).1,
                    }
                }
                *msg.set_message_type(MessageType::Response)
                    .set_response_code(res_code)
                    .answers_mut() = ans_records;

                let response = match msg.to_vec() {
                    Ok(vec) => vec,
                    Err(_) => continue,
                };
                if !send_ready {
                    poll_fn(|cx| session.as_mut().poll_send_ready(cx)).await;
                }
                session.as_mut().send_to(dest, response);
                send_ready = false;
            }
            poll_fn(|cx| session.as_mut().poll_shutdown(cx)).await
        });
    }
}
