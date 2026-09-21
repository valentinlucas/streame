//! Intercepteur qui remonte à l'application les demandes d'image-clé (PLI, FIR) reçues du Mac.
//!
//! Dans webrtc-rs 0.21 le RTCP entrant s'arrête en bout de chaîne, là où sont les
//! intercepteurs qui agissent dessus (NACK, TWCC, rapports) ; un paquet n'atteint la piste
//! locale (`TrackLocalEvent::OnRtcpPacket`) que si un intercepteur l'a marqué
//! `DeliverToApplication`. C'est le rôle de celui-ci, pour que l'encodeur VideoToolbox force
//! une image-clé quand le Mac en demande une.

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use std::collections::VecDeque;
use std::time::Instant;

/// Position dans la chaîne (après tous les intercepteurs par défaut, dont le dernier est le
/// jitter buffer à 13 000).
pub const SLOT: usize = 14_000;

#[derive(Default)]
pub struct KeyframeRequests {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

fn is_keyframe_request(packet: &Box<dyn rtc::rtcp::Packet>) -> bool {
    let p = packet.as_any();
    p.is::<PictureLossIndication>() || p.is::<FullIntraRequest>()
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequests {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let requests: Vec<Box<dyn rtc::rtcp::Packet>> =
                packets.iter().filter(|p| is_keyframe_request(p)).cloned().collect();
            if requests.is_empty() {
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for KeyframeRequests {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
