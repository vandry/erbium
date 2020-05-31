use netlink_packet_route::AddressHeader;
use netlink_packet_route::NetlinkPayload::InnerMessage;
use netlink_packet_route::RtnlMessage::*;
use netlink_packet_route::{
    constants::*, AddressMessage, LinkMessage, NetlinkHeader, NetlinkMessage, NetlinkPayload,
    RtnlMessage,
};
use netlink_sys::constants::*;
use netlink_sys::{Protocol, Socket, SocketAddr};

#[derive(Debug)]
pub enum LinkLayer {
    Ethernet([u8; 6]),
    None,
}

#[derive(Debug)]
struct IfInfo {
    name: String,
    addresses: Vec<(std::net::IpAddr, u8)>,
    lladdr: LinkLayer,
    llbroadcast: LinkLayer,
    mtu: u32,
    //operstate: netlink_packet_route::rtnl::link::nlas::link_state::State, // Is private
}

#[derive(Debug)]
struct NetInfo {
    name2idx: std::collections::HashMap<String, u32>,
    intf: std::collections::HashMap<u32, IfInfo>,
}

impl NetInfo {
    fn new() -> Self {
        NetInfo {
            name2idx: std::collections::HashMap::new(),
            intf: std::collections::HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct SharedNetInfo(std::sync::Arc<tokio::sync::RwLock<NetInfo>>);

fn convert_address(addr: &[u8], family: u16) -> std::net::IpAddr {
    match family {
        AF_INET => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]))
        }
        AF_INET6 => std::net::IpAddr::V6(
            [
                addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], addr[6], addr[7], addr[8],
                addr[9], addr[10], addr[11], addr[12], addr[13], addr[14], addr[15],
            ]
            .into(),
        ),
        x => panic!("Unknown address family {:?}", x),
    }
}

impl SharedNetInfo {
    fn parse_addr(&self, addr: AddressMessage) -> (std::net::IpAddr, u8) {
        use netlink_packet_route::address::nlas::Nla::*;
        let mut ifaddr = None;
        //let mut iflocal = None;
        let iffamily = addr.header.family;
        let ifprefixlen = addr.header.prefix_len;
        for i in addr.nlas {
            match i {
                Address(a) => ifaddr = Some(convert_address(&a, iffamily.into())),
                //Local(a) => iflocal = Some(convert_address(&a, iffamily.into())),
                _ => (),
            }
        }
        return (ifaddr.unwrap(), ifprefixlen);
    }
    async fn process_newaddr(&self, addr: AddressMessage) {
        let ifindex = addr.header.index;
        let ifaddr = self.parse_addr(addr);
        let mut ni = self.0.write().await;
        let ii = ni.intf.get_mut(&ifindex).unwrap(); // TODO: Error?
        ii.addresses.push(ifaddr)
    }
    async fn process_deladdr(&self, addr: AddressMessage) {
        let ifindex = addr.header.index;
        let ifaddr = self.parse_addr(addr);
        let mut ni = self.0.write().await;
        let ii = ni.intf.get_mut(&ifindex).unwrap(); // TODO: Error?
        ii.addresses.retain(|&x| x != ifaddr);
    }
    fn decode_linklayer(&self, linktype: u16, addr: &[u8]) -> LinkLayer {
        match linktype {
            ARPHRD_ETHER => {
                LinkLayer::Ethernet([addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]])
            }
            ARPHRD_LOOPBACK => LinkLayer::None,
            ARPHRD_SIT => LinkLayer::None, // Actually this is a IpAddr, but we don't do DHCP over it, so...
            l => {
                println!("Unknown Linklayer: {:?}", l);
                LinkLayer::None
            }
        }
    }
    async fn process_newlink(&self, link: LinkMessage) {
        use netlink_packet_route::link::nlas::Nla::*;
        let mut ifname: Option<String> = None;
        let mut ifmtu: Option<u32> = None;
        let mut ifaddr = None;
        let mut ifbrd = None;
        let ifidx = link.header.index;
        for i in &link.nlas {
            match i {
                IfName(name) => ifname = Some(name.clone()),
                Mtu(mtu) => ifmtu = Some(mtu.clone()),
                Address(addr) => ifaddr = Some(addr.clone()),
                Broadcast(addr) => ifbrd = Some(addr.clone()),
                _ => (),
            }
        }
        let ifaddr = ifaddr.map_or(LinkLayer::None, |x| {
            self.decode_linklayer(link.header.link_layer_type, &x)
        });
        let ifbrd = ifbrd.map_or(LinkLayer::None, |x| {
            self.decode_linklayer(link.header.link_layer_type, &x)
        });
        let ifinfo = IfInfo {
            name: ifname.unwrap(),
            mtu: ifmtu.unwrap(),
            addresses: vec![],
            lladdr: ifaddr,
            llbroadcast: ifbrd,
        };

        let mut netinfo = self.0.write().await;
        netinfo.name2idx.insert(ifinfo.name.clone(), ifidx);
        netinfo.intf.insert(ifidx, ifinfo);
    }

    async fn send_linkdump(&self, socket: &mut Socket, seq: &mut u32) {
        let mut packet = NetlinkMessage {
            header: NetlinkHeader {
                flags: NLM_F_REQUEST | NLM_F_DUMP,
                sequence_number: *seq,
                ..Default::default()
            },
            payload: NetlinkPayload::from(RtnlMessage::GetLink(LinkMessage::default())),
        };
        *seq += 1;

        packet.finalize();

        let mut buf = vec![0; packet.header.length as usize];

        // Before calling serialize, it is important to check that the buffer in which we're emitting is big
        // enough for the packet, other `serialize()` panics.

        assert!(buf.len() == packet.buffer_len());

        packet.serialize(&mut buf[..]);

        socket.add_membership(RTNLGRP_LINK).unwrap();

        if let Err(e) = socket.send(&buf[..]).await {
            println!("SEND ERROR {}", e);
        }
    }

    async fn send_addrdump(&self, socket: &mut Socket, seq: &mut u32) {
        let mut packet = NetlinkMessage {
            header: NetlinkHeader {
                flags: NLM_F_REQUEST | NLM_F_DUMP,
                sequence_number: *seq,
                ..Default::default()
            },
            payload: NetlinkPayload::from(RtnlMessage::GetAddress(AddressMessage {
                header: AddressHeader {
                    family: AF_PACKET as u8,
                    ..Default::default()
                },
                ..Default::default()
            })),
        };

        *seq += 1;

        packet.finalize();

        let mut buf = vec![0; packet.header.length as usize];

        // Before calling serialize, it is important to check that the buffer in which we're emitting is big
        // enough for the packet, other `serialize()` panics.

        assert!(buf.len() == packet.buffer_len());

        packet.serialize(&mut buf[..]);

        socket.add_membership(RTNLGRP_IPV4_IFADDR).unwrap();
        socket.add_membership(RTNLGRP_IPV6_IFADDR).unwrap();

        if let Err(e) = socket.send(&buf[..]).await {
            println!("SEND ERROR {}", e);
        }
    }

    async fn run(self, mut chan: tokio::sync::mpsc::Sender<()>) {
        let mut socket = Socket::new(Protocol::Route).unwrap();
        let _port_number = socket.bind_auto().unwrap().port_number();
        let mut seq = 1;
        socket.connect(&SocketAddr::new(0, 0)).unwrap();

        let mut receive_buffer = vec![0; 4096];
        let mut offset = 0;

        self.send_linkdump(&mut socket, &mut seq).await;
        let mut sent_addrdump = false;
        // we set the NLM_F_DUMP flag so we expect a multipart rx_packet in response.
        while let Ok(size) = socket.recv(&mut receive_buffer[..]).await {
            loop {
                let bytes = &receive_buffer[offset..];
                let rx_packet = <NetlinkMessage<RtnlMessage>>::deserialize(bytes).unwrap();

                match rx_packet.payload {
                    InnerMessage(NewLink(link)) => self.process_newlink(link).await,
                    InnerMessage(NewAddress(addr)) => self.process_newaddr(addr).await,
                    InnerMessage(DelAddress(addr)) => self.process_deladdr(addr).await,
                    NetlinkPayload::Done => {
                        if !sent_addrdump {
                            self.send_addrdump(&mut socket, &mut seq).await;
                            sent_addrdump = true;
                        } else {
                            chan.try_send(());
                        }
                    }
                    e => println!("Unknown: {:?}", e),
                }

                offset += rx_packet.header.length as usize;
                if offset == size || rx_packet.header.length == 0 {
                    offset = 0;
                    break;
                }
            }
        }
    }

    pub async fn new() -> Self {
        let (mut s, mut c) = tokio::sync::mpsc::channel::<()>(1);
        let shared = SharedNetInfo(std::sync::Arc::new(
            tokio::sync::RwLock::new(NetInfo::new()),
        ));
        tokio::spawn(shared.clone().run(s));
        // We want to block and wait until all the data is loaded, otherwise we'll cause confusion.
        c.recv().await;
        shared
    }

    pub async fn get_interfaces(&self) -> Vec<String> {
        self.0
            .read()
            .await
            .intf
            .values()
            .map(|x| x.name.clone())
            .collect()
    }
}
