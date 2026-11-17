//! Call statistics for the "connection" panel: round-trip time, packet loss,
//! bitrates and codecs, summarised from WebRTC's stats reports.

use std::time::Duration;

use livekit::webrtc::stats::RtcStats;

/// Running totals from one stats report, kept to compute rates on the next.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub packets_lost: i64,
}

/// What the stats panel shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallStats {
    /// Round-trip time to the server in milliseconds.
    pub rtt_ms: Option<f64>,
    /// Packet loss of what this client receives since the last sample, in percent.
    pub loss_percent: f64,
    /// What this client sends and receives, in kbit/s.
    pub send_kbps: f64,
    pub receive_kbps: f64,
    /// Bandwidth the congestion controller estimates is available upstream, in kbit/s.
    pub available_send_kbps: Option<f64>,
    /// Codecs in use (e.g. "video/AV1", "audio/opus"), sorted, without duplicates.
    pub codecs: Vec<String>,
    /// The largest incoming video: width, height and frame rate.
    pub video_in: Option<(u32, u32, f64)>,
    /// Jitter of incoming audio in milliseconds (the worst stream).
    pub jitter_ms: f64,
}

/// Summarises the publisher and subscriber reports. `prev` and `elapsed` are
/// the totals from the previous sample and the time since; rates are zero on
/// the first sample.
pub fn summarize(
    publisher: &[RtcStats],
    subscriber: &[RtcStats],
    prev: Option<Totals>,
    elapsed: Duration,
) -> (CallStats, Totals) {
    let mut stats = CallStats::default();
    let mut totals = Totals::default();
    let mut rtts = Vec::new();

    for (report, upstream) in [(publisher, true), (subscriber, false)] {
        for s in report {
            match s {
                RtcStats::OutboundRtp(o) => totals.bytes_sent += o.sent.bytes_sent,
                RtcStats::InboundRtp(i) => {
                    totals.bytes_received += i.inbound.bytes_received;
                    totals.packets_received += i.received.packets_received;
                    totals.packets_lost += i.received.packets_lost.max(0);
                    if i.stream.kind == "video" && i.inbound.frame_width > 0 {
                        let size = (i.inbound.frame_width, i.inbound.frame_height, i.inbound.frames_per_second);
                        if stats.video_in.is_none_or(|(w, h, _)| w * h < size.0 * size.1) {
                            stats.video_in = Some(size);
                        }
                    }
                    if i.stream.kind == "audio" {
                        stats.jitter_ms = stats.jitter_ms.max(i.received.jitter * 1000.0);
                    }
                }
                RtcStats::CandidatePair(p) if p.candidate_pair.nominated => {
                    if p.candidate_pair.current_round_trip_time > 0.0 {
                        rtts.push(p.candidate_pair.current_round_trip_time * 1000.0);
                    }
                    if upstream && p.candidate_pair.available_outgoing_bitrate > 0.0 {
                        stats.available_send_kbps = Some(p.candidate_pair.available_outgoing_bitrate / 1000.0);
                    }
                }
                RtcStats::Codec(c) if !c.codec.mime_type.is_empty() => {
                    if !stats.codecs.contains(&c.codec.mime_type) {
                        stats.codecs.push(c.codec.mime_type.clone());
                    }
                }
                _ => {}
            }
        }
    }
    stats.codecs.sort();
    stats.rtt_ms = rtts.into_iter().reduce(f64::min);

    if let Some(prev) = prev {
        let secs = elapsed.as_secs_f64();
        if secs > 0.0 {
            stats.send_kbps = totals.bytes_sent.saturating_sub(prev.bytes_sent) as f64 * 8.0 / 1000.0 / secs;
            stats.receive_kbps = totals.bytes_received.saturating_sub(prev.bytes_received) as f64 * 8.0 / 1000.0 / secs;
        }
        let received = totals.packets_received.saturating_sub(prev.packets_received) as f64;
        let lost = (totals.packets_lost - prev.packets_lost).max(0) as f64;
        if received + lost > 0.0 {
            stats.loss_percent = lost * 100.0 / (received + lost);
        }
    }
    (stats, totals)
}

/// Takes one stats sample of a room. Pass the totals and time from the
/// previous sample to get rates (bitrates, loss since then).
pub async fn sample(room: &livekit::Room, prev: Option<(Totals, std::time::Instant)>) -> Option<(CallStats, Totals)> {
    let report = room.get_stats().await.ok()?;
    let elapsed = prev.map(|(_, at)| at.elapsed()).unwrap_or_default();
    Some(summarize(&report.publisher_stats, &report.subscriber_stats, prev.map(|(t, _)| t), elapsed))
}

/// A rough grade for a connection from its stats, for the tile indicator
/// when the server's own quality report is missing.
pub fn grade(stats: &CallStats) -> crate::ConnectionQuality {
    use crate::ConnectionQuality::*;
    let rtt = stats.rtt_ms.unwrap_or(0.0);
    if stats.loss_percent >= 15.0 || rtt >= 800.0 {
        Lost
    } else if stats.loss_percent >= 5.0 || rtt >= 300.0 {
        Poor
    } else if stats.loss_percent >= 1.0 || rtt >= 150.0 {
        Good
    } else {
        Excellent
    }
}

#[cfg(test)]
mod tests {
    use livekit::webrtc::stats::{CandidatePairStats, CodecStats, InboundRtpStats, OutboundRtpStats};

    use super::*;

    fn inbound(kind: &str, bytes: u64, received: u64, lost: i64, width: u32) -> RtcStats {
        let mut i = InboundRtpStats::default();
        i.stream.kind = kind.into();
        i.inbound.bytes_received = bytes;
        i.received.packets_received = received;
        i.received.packets_lost = lost;
        i.received.jitter = 0.02;
        i.inbound.frame_width = width;
        i.inbound.frame_height = width * 9 / 16;
        i.inbound.frames_per_second = 30.0;
        RtcStats::InboundRtp(i)
    }

    #[test]
    fn rates_and_loss_from_two_samples() {
        let mut out = OutboundRtpStats::default();
        out.sent.bytes_sent = 1000;
        let mut pair = CandidatePairStats::default();
        pair.candidate_pair.nominated = true;
        pair.candidate_pair.current_round_trip_time = 0.042;
        pair.candidate_pair.available_outgoing_bitrate = 2_500_000.0;
        let mut codec = CodecStats::default();
        codec.codec.mime_type = "video/AV1".into();
        let publisher = vec![RtcStats::OutboundRtp(out), RtcStats::CandidatePair(pair), RtcStats::Codec(codec.clone())];
        let first = vec![inbound("video", 10_000, 100, 0, 1280), RtcStats::Codec(codec)];
        let (s, t) = summarize(&publisher, &first, None, Duration::ZERO);
        assert_eq!(s.send_kbps, 0.0);
        assert_eq!(s.rtt_ms, Some(42.0));
        assert_eq!(s.available_send_kbps, Some(2500.0));
        assert_eq!(s.codecs, vec!["video/AV1".to_string()]);
        assert_eq!(s.video_in, Some((1280, 720, 30.0)));

        let mut out = OutboundRtpStats::default();
        out.sent.bytes_sent = 126_000;
        let second = vec![inbound("video", 260_000, 190, 10, 1920), inbound("audio", 0, 0, 0, 0)];
        let (s, _) = summarize(&[RtcStats::OutboundRtp(out)], &second, Some(t), Duration::from_secs(1));
        assert_eq!(s.send_kbps, 1000.0);
        assert_eq!(s.receive_kbps, 2000.0);
        assert_eq!(s.loss_percent, 10.0);
        assert_eq!(s.video_in, Some((1920, 1080, 30.0)));
        assert!((s.jitter_ms - 20.0).abs() < 1e-9);
        assert_eq!(grade(&s), crate::ConnectionQuality::Poor);
    }

    #[test]
    fn grades() {
        let ok = CallStats { rtt_ms: Some(30.0), ..Default::default() };
        assert_eq!(grade(&ok), crate::ConnectionQuality::Excellent);
        let far = CallStats { rtt_ms: Some(900.0), ..Default::default() };
        assert_eq!(grade(&far), crate::ConnectionQuality::Lost);
    }
}
