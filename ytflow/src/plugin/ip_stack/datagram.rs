use std::task::Context;
use std::task::Poll;

use super::*;
use crate::flow::*;

pub(super) struct IpStackDatagramSession {
    pub(super) stack: Arc<Mutex<IpStackInner>>,
    pub(super) local_endpoint: IpAddress,
    pub(super) local_port: u16,
    pub(super) flow_label: u32,
}

impl MultiplexedDatagramSession for IpStackDatagramSession {
    fn on_close(&mut self) {
        let mut stack_guard = self.stack.lock().unwrap();
        stack_guard.udp_sockets.remove(&self.local_port);
    }
    fn poll_send_ready(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }
    fn send_to(&mut self, src: DestinationAddr, buf: Buffer) {
        let payload_len: u16 = match buf.len().try_into().ok().filter(|&l| l <= 1500 - 48) {
            Some(l) => l,
            // Ignore oversized packet
            None => return,
        };
        let _from_ip = match &src.host {
            HostName::Ip(ip) => ip,
            // TODO: print diagnostic message: Cannot send datagram to unresolved destination
            _ => return,
        };

        let mut stack_guard = self.stack.lock().unwrap();
        use smoltcp::phy::{Device, TxToken};
        let sender = stack_guard.netif.device_mut().transmit();
        let ip_buf = match sender {
            Some(b) => b,
            None => return,
        };
        match (&self.local_endpoint, &src.host) {
            (IpAddress::Ipv4(dst_v4), HostName::Ip(IpAddr::V4(src_ip))) => {
                let src_ip: Ipv4Address = (*src_ip).into();
                let _ = ip_buf.consume(
                    smoltcp::time::Instant::from_micros_const(0),
                    buf.len() + 48,
                    |ip_buf| {
                        let mut ip_packet = Ipv4Packet::new_unchecked(ip_buf);
                        ip_packet.set_version(4);
                        ip_packet.set_header_len(20);
                        ip_packet.set_total_len(20 + 8 + payload_len);
                        ip_packet.set_dont_frag(true);
                        ip_packet.set_frag_offset(0);
                        ip_packet.set_hop_limit(255);
                        ip_packet.set_protocol(IpProtocol::Udp);
                        ip_packet.set_dst_addr(*dst_v4);
                        ip_packet.set_src_addr(src_ip);
                        let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
                        udp_packet.set_dst_port(self.local_port);
                        udp_packet.set_src_port(src.port);
                        udp_packet.set_len(8 + payload_len);
                        udp_packet.payload_mut()[..buf.len()].copy_from_slice(&buf);
                        udp_packet.fill_checksum(&src_ip.into(), &(*dst_v4).into());
                        ip_packet.fill_checksum();
                        debug_log(format!("Sending v4 udp from {:?} to {:?}", src_ip, dst_v4));
                        Ok(())
                    },
                );
            }
            (IpAddress::Ipv6(dst_v6), HostName::Ip(IpAddr::V6(src_ip))) => {
                let src_ip: Ipv6Address = (*src_ip).into();
                let _ = ip_buf.consume(
                    smoltcp::time::Instant::from_micros_const(0),
                    buf.len() + 48,
                    |ip_buf| {
                        let mut ip_packet = Ipv6Packet::new_unchecked(ip_buf);
                        ip_packet.set_version(6);
                        ip_packet.set_hop_limit(255);
                        ip_packet.set_next_header(IpProtocol::Udp);
                        ip_packet.set_dst_addr(*dst_v6);
                        ip_packet.set_src_addr(src_ip);
                        ip_packet.set_payload_len(8 + payload_len);
                        ip_packet.set_flow_label(self.flow_label);
                        let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
                        udp_packet.set_dst_port(self.local_port);
                        udp_packet.set_src_port(src.port);
                        udp_packet.set_len(8 + payload_len);
                        udp_packet.payload_mut()[..buf.len()].copy_from_slice(&buf);
                        udp_packet.fill_checksum(&src_ip.into(), &(*dst_v6).into());
                        debug_log(format!("Sending v6 udp from {:?} to {:?}", src_ip, dst_v6));
                        Ok(())
                    },
                );
            }
            // Ignore unmatched IP version
            _ => {}
        }
    }
}
