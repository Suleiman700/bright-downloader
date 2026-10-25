//! MPEG-DASH (`.mpd`) manifests, turned into the same per-track segment
//! lists as HLS so [`crate::hls`]'s downloader can fetch them.
//!
//! Supported: `SegmentTemplate` (with `$Number$` or a `SegmentTimeline`),
//! `SegmentList`, and single-file representations (`SegmentBase` / plain
//! `BaseURL`); static (on-demand) and dynamic (live) manifests. Content with
//! `ContentProtection` (DRM) is refused. Multi-period manifests use the first
//! period (on demand) or the latest one (live).

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::hls::{MediaPlaylist, MediaSegment, Resource, Variant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Other,
}

#[derive(Clone, Debug)]
pub struct Representation {
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub protected: bool,
    pub media: MediaPlaylist,
}

impl Representation {
    pub fn label(&self) -> String {
        Variant {
            url: self.id.clone(),
            bandwidth: self.bandwidth,
            resolution: self.width.zip(self.height).map(|(w, h)| format!("{w}x{h}")),
            audio_group: None,
        }
        .label()
    }
}

#[derive(Clone, Debug)]
pub struct AdaptationSet {
    pub kind: Kind,
    pub reps: Vec<Representation>,
}

#[derive(Clone, Debug)]
pub struct Mpd {
    pub dynamic: bool,
    /// Seconds, for on-demand content.
    pub duration: Option<f64>,
    pub sets: Vec<AdaptationSet>,
}

fn err(msg: impl Into<String>) -> Error {
    Error::Stream(msg.into())
}

pub fn looks_like_mpd(text: &str) -> bool {
    text.trim_start_matches('\u{feff}').trim_start().starts_with('<') && text.contains("<MPD")
}

/// ISO 8601 duration (`PT1H2M3.5S`, `P1DT2H`) → seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim().strip_prefix('P')?;
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    let mut num = String::new();
    for (part, units) in [(date, [('Y', 31_536_000.0), ('M', 2_592_000.0), ('W', 604_800.0), ('D', 86_400.0)]), (time, [('H', 3600.0), ('M', 60.0), ('S', 1.0), ('S', 1.0)])] {
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let factor = units.iter().find(|(u, _)| *u == c)?.1;
                total += num.parse::<f64>().ok()? * factor;
                num.clear();
            }
        }
    }
    Some(total)
}

/// `2024-03-01T12:00:00Z` (or with `.fff` / `+hh:mm`) → Unix seconds.
pub fn parse_datetime(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (time, offset) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0.0)
    } else if let Some(i) = rest.rfind(['+', '-']).filter(|&i| i > 0) {
        let (t, o) = rest.split_at(i);
        let sign = if o.starts_with('-') { -1.0 } else { 1.0 };
        let mut hm = o[1..].split(':').map(|p| p.parse::<f64>().ok());
        let (h, mi) = (hm.next()??, hm.next().flatten().unwrap_or(0.0));
        (t, sign * (h * 3600.0 + mi * 60.0))
    } else {
        (rest, 0.0)
    };
    let mut t = time.split(':').map(|p| p.parse::<f64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next().flatten().unwrap_or(0.0));
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + hh * 3600.0 + mm * 60.0 + ss - offset)
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|c| c.has_tag_name(name))
}

fn base_url(node: roxmltree::Node, parent: &url::Url) -> url::Url {
    child(node, "BaseURL").and_then(|b| b.text()).and_then(|t| parent.join(t.trim()).ok()).unwrap_or_else(|| parent.clone())
}

/// `SegmentTemplate` attributes merged from Period → AdaptationSet →
/// Representation (inner levels win), plus the nearest `SegmentTimeline`.
#[derive(Clone, Default)]
struct Template<'a, 'i> {
    attrs: HashMap<String, String>,
    timeline: Option<roxmltree::Node<'a, 'i>>,
}

impl<'a, 'i> Template<'a, 'i> {
    fn merge(&self, node: roxmltree::Node<'a, 'i>) -> Self {
        let mut t = self.clone();
        if let Some(st) = child(node, "SegmentTemplate") {
            for a in st.attributes() {
                t.attrs.insert(a.name().to_string(), a.value().to_string());
            }
            if let Some(tl) = child(st, "SegmentTimeline") {
                t.timeline = Some(tl);
            }
        }
        t
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(String::as_str)
    }

    fn num(&self, name: &str) -> Option<u64> {
        self.get(name).and_then(|v| v.parse().ok())
    }
}

/// Fill in `$RepresentationID$`, `$Bandwidth$`, `$Number%05d$`, `$Time$`.
fn expand(template: &str, id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::new();
    let mut parts = template.split('$');
    out.push_str(parts.next().unwrap_or(""));
    let mut inside = true;
    for part in parts {
        if inside {
            let (name, fmt) = part.split_once('%').unwrap_or((part, ""));
            let width: usize = fmt.trim_start_matches('0').trim_end_matches('d').parse().unwrap_or(0);
            let value = match name {
                "" => "$".to_string(),
                "RepresentationID" => id.to_string(),
                "Bandwidth" => bandwidth.to_string(),
                "Number" => number.to_string(),
                "Time" => time.to_string(),
                other => format!("${other}$"),
            };
            out.push_str(&format!("{value:0>width$}"));
        } else {
            out.push_str(part);
        }
        inside = !inside;
    }
    out
}

fn range_of(s: Option<&str>) -> Option<(u64, u64)> {
    let (a, b) = s?.split_once('-')?;
    let (a, b): (u64, u64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (b >= a).then_some((a, b - a + 1))
}

struct Ctx {
    dynamic: bool,
    now: f64,
    availability_start: f64,
    time_shift: Option<f64>,
    period_start: f64,
    period_duration: Option<f64>,
}

fn template_segments(t: &Template, base: &url::Url, id: &str, bandwidth: u64, cx: &Ctx) -> Result<MediaPlaylist> {
    let timescale = t.num("timescale").unwrap_or(1).max(1) as f64;
    let start_number = t.num("startNumber").unwrap_or(1);
    let pto = t.num("presentationTimeOffset").unwrap_or(0);
    let media = t.get("media").ok_or_else(|| err("DASH template without a media address"))?;
    let resolve = |u: String| base.join(&u).map(|u| u.to_string()).map_err(|e| err(format!("bad DASH address: {e}")));
    let init = match t.get("initialization") {
        Some(i) => Some(Resource { url: resolve(expand(i, id, bandwidth, 0, 0))?, range: None }),
        None => None,
    };
    let mut segments = Vec::new();
    if let Some(tl) = t.timeline {
        let mut time = 0u64;
        let mut number = start_number;
        let entries: Vec<_> = tl.children().filter(|c| c.has_tag_name("S")).collect();
        let end_of_period = cx.period_duration.map(|d| (d * timescale) as u64 + pto);
        for (k, s) in entries.iter().enumerate() {
            if let Some(t0) = s.attribute("t").and_then(|v| v.parse().ok()) {
                time = t0;
            }
            let d: u64 = s.attribute("d").and_then(|v| v.parse().ok()).filter(|d| *d > 0).ok_or_else(|| err("DASH timeline entry without a duration"))?;
            let r: i64 = s.attribute("r").and_then(|v| v.parse().ok()).unwrap_or(0);
            let next_t = entries.get(k + 1).and_then(|n| n.attribute("t")).and_then(|v| v.parse::<u64>().ok());
            let repeats = if r >= 0 {
                r as u64
            } else {
                // -1: repeat until the next entry, the period's end, or (live) now.
                let until = next_t.or(end_of_period).unwrap_or_else(|| ((cx.now - cx.availability_start - cx.period_start) * timescale) as u64 + pto);
                (until.saturating_sub(time) / d).saturating_sub(1)
            };
            for _ in 0..=repeats {
                segments.push(MediaSegment {
                    sequence: time,
                    duration: d as f64 / timescale,
                    resource: Resource { url: resolve(expand(media, id, bandwidth, number, time))?, range: None },
                    key: None,
                });
                time += d;
                number += 1;
            }
        }
    } else {
        let d = t.num("duration").filter(|d| *d > 0).ok_or_else(|| err("DASH template without a segment duration"))? as f64;
        let seg_secs = d / timescale;
        let (first, last) = if cx.dynamic {
            // Live: from the start of the time-shift window (or the last few
            // segments) up to the newest one that is already available.
            let elapsed = cx.now - cx.availability_start - cx.period_start;
            let newest = (elapsed / seg_secs).floor() as i64 - 1;
            let window = cx.time_shift.map_or(3, |w| (w / seg_secs).floor() as i64).max(1);
            ((newest - window + 1).max(0) as u64, newest.max(0) as u64)
        } else {
            let total = cx.period_duration.ok_or_else(|| err("DASH manifest without a duration"))?;
            (0, ((total / seg_secs).ceil() as u64).saturating_sub(1))
        };
        for k in first..=last {
            let number = start_number + k;
            segments.push(MediaSegment {
                sequence: number,
                duration: seg_secs,
                resource: Resource { url: resolve(expand(media, id, bandwidth, number, k * d as u64))?, range: None },
                key: None,
            });
        }
    }
    let target = segments.iter().map(|s| s.duration).fold(0.0, f64::max);
    Ok(MediaPlaylist { segments, target_duration: target, ended: !cx.dynamic, init })
}

fn list_segments(list: roxmltree::Node, base: &url::Url, cx: &Ctx) -> Result<MediaPlaylist> {
    let timescale = list.attribute("timescale").and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0).max(1.0);
    let duration = list.attribute("duration").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / timescale;
    let resolve = |u: Option<&str>| match u {
        Some(u) => base.join(u).map(|u| u.to_string()).map_err(|e| err(format!("bad DASH address: {e}"))),
        None => Ok(base.to_string()),
    };
    let init = match child(list, "Initialization") {
        Some(i) => Some(Resource { url: resolve(i.attribute("sourceURL"))?, range: range_of(i.attribute("range")) }),
        None => None,
    };
    let segments = list
        .children()
        .filter(|c| c.has_tag_name("SegmentURL"))
        .enumerate()
        .map(|(k, s)| {
            Ok(MediaSegment {
                sequence: k as u64,
                duration,
                resource: Resource { url: resolve(s.attribute("media"))?, range: range_of(s.attribute("mediaRange")) },
                key: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(MediaPlaylist { segments, target_duration: duration, ended: !cx.dynamic, init })
}

fn kind_of(set: roxmltree::Node, rep: Option<roxmltree::Node>) -> Kind {
    let mime = |n: roxmltree::Node| n.attribute("contentType").or(n.attribute("mimeType")).unwrap_or("").to_ascii_lowercase();
    let text = format!("{} {}", mime(set), rep.map(mime).unwrap_or_default());
    if text.contains("video") || set.attribute("maxHeight").is_some() || rep.and_then(|r| r.attribute("height")).is_some() {
        Kind::Video
    } else if text.contains("audio") {
        Kind::Audio
    } else {
        Kind::Other
    }
}

/// Parse a manifest. `now` (Unix seconds) places live `$Number$` segments.
pub fn parse(text: &str, manifest_url: &url::Url, now: f64) -> Result<Mpd> {
    let doc = roxmltree::Document::parse_with_options(text, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() })
        .map_err(|e| err(format!("invalid DASH manifest: {e}")))?;
    let mpd = doc.root_element();
    if !mpd.has_tag_name("MPD") {
        return Err(err("the address is not a DASH manifest"));
    }
    let dynamic = mpd.attribute("type") == Some("dynamic");
    let duration = mpd.attribute("mediaPresentationDuration").and_then(parse_duration);
    let base = base_url(mpd, manifest_url);
    let periods: Vec<_> = mpd.children().filter(|c| c.has_tag_name("Period")).collect();
    let period = if dynamic { periods.last() } else { periods.first() }.ok_or_else(|| err("the DASH manifest has no periods"))?;
    let period_start = period.attribute("start").and_then(parse_duration).unwrap_or(0.0);
    let cx = Ctx {
        dynamic,
        now,
        availability_start: mpd.attribute("availabilityStartTime").and_then(parse_datetime).unwrap_or(0.0),
        time_shift: mpd.attribute("timeShiftBufferDepth").and_then(parse_duration),
        period_start,
        period_duration: period.attribute("duration").and_then(parse_duration).or(duration.map(|d| d - period_start)),
    };
    let period_base = base_url(*period, &base);
    let period_tpl = Template::default().merge(*period);

    let mut sets = Vec::new();
    for set in period.children().filter(|c| c.has_tag_name("AdaptationSet")) {
        let set_base = base_url(set, &period_base);
        let set_tpl = period_tpl.merge(set);
        let set_protected = child(set, "ContentProtection").is_some();
        let mut reps = Vec::new();
        let mut kind = kind_of(set, None);
        for rep in set.children().filter(|c| c.has_tag_name("Representation")) {
            if kind == Kind::Other {
                kind = kind_of(set, Some(rep));
            }
            let id = rep.attribute("id").unwrap_or("").to_string();
            let bandwidth = rep.attribute("bandwidth").and_then(|v| v.parse().ok()).unwrap_or(0);
            let rep_base = base_url(rep, &set_base);
            let tpl = set_tpl.merge(rep);
            let list = child(rep, "SegmentList").or_else(|| child(set, "SegmentList"));
            let media = if tpl.get("media").is_some() {
                template_segments(&tpl, &rep_base, &id, bandwidth, &cx)?
            } else if let Some(list) = list {
                list_segments(list, &rep_base, &cx)?
            } else {
                // One self-contained file per representation.
                MediaPlaylist {
                    segments: vec![MediaSegment {
                        sequence: 0,
                        duration: cx.period_duration.unwrap_or(0.0),
                        resource: Resource { url: rep_base.to_string(), range: None },
                        key: None,
                    }],
                    target_duration: 0.0,
                    ended: !dynamic,
                    init: None,
                }
            };
            reps.push(Representation {
                id,
                bandwidth,
                width: rep.attribute("width").or(set.attribute("width")).and_then(|v| v.parse().ok()),
                height: rep.attribute("height").or(set.attribute("height")).and_then(|v| v.parse().ok()),
                protected: set_protected || child(rep, "ContentProtection").is_some(),
                media,
            });
        }
        reps.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
        if !reps.is_empty() {
            sets.push(AdaptationSet { kind, reps });
        }
    }
    Ok(Mpd { dynamic, duration, sets })
}

impl Mpd {
    /// Every video quality, best first.
    pub fn video(&self) -> Vec<&Representation> {
        let mut v: Vec<_> = self.sets.iter().filter(|s| s.kind == Kind::Video).flat_map(|s| &s.reps).collect();
        v.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
        v
    }

    /// The tracks to download: the chosen (or best) video and, if audio
    /// comes separately, the best audio. Refuses DRM-protected content.
    pub fn pick(&self, wanted: Option<&str>) -> Result<Vec<&Representation>> {
        let videos = self.video();
        let video = wanted
            .and_then(|w| videos.iter().find(|r| r.id == w))
            .or(videos.first())
            .copied()
            // Audio-only manifests.
            .or_else(|| self.sets.first().and_then(|s| s.reps.first()))
            .ok_or_else(|| err("the DASH manifest lists no streams"))?;
        let audio = self.sets.iter().filter(|s| s.kind == Kind::Audio).flat_map(|s| s.reps.first()).max_by_key(|r| r.bandwidth);
        let picked: Vec<&Representation> = std::iter::once(video).chain(audio.filter(|a| a.id != video.id)).collect();
        if picked.iter().any(|r| r.protected) {
            return Err(err("this stream is DRM-protected and can't be downloaded"));
        }
        Ok(picked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> url::Url {
        url::Url::parse("https://cdn.example/show/manifest.mpd").unwrap()
    }

    const VOD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT0M10S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" duration="4000" startNumber="1" initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/seg-$Number%03d$.m4s"/>
      <Representation id="720" bandwidth="3000000" width="1280" height="720"/>
      <Representation id="360" bandwidth="800000" width="640" height="360"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" lang="en">
      <BaseURL>audio/</BaseURL>
      <Representation id="a1" bandwidth="128000">
        <SegmentTemplate timescale="48000" initialization="init.mp4" media="t$Time$.m4s">
          <SegmentTimeline><S t="0" d="192000" r="1"/><S d="96000"/></SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn parses_templates_and_timelines() {
        let mpd = parse(VOD, &url(), 0.0).unwrap();
        assert!(!mpd.dynamic);
        assert_eq!(mpd.duration, Some(10.0));
        let picked = mpd.pick(None).unwrap();
        assert_eq!(picked.len(), 2);
        let v = &picked[0].media;
        assert_eq!(picked[0].label(), "720p · 3.0 Mbps");
        assert_eq!(v.segments.len(), 3, "10 s in 4 s pieces");
        assert_eq!(v.segments[0].resource.url, "https://cdn.example/show/720/seg-001.m4s");
        assert_eq!(v.init.as_ref().unwrap().url, "https://cdn.example/show/720/init.mp4");
        let a = &picked[1].media;
        assert_eq!(a.segments.iter().map(|s| s.resource.url.rsplit('/').next().unwrap()).collect::<Vec<_>>(), ["t0.m4s", "t192000.m4s", "t384000.m4s"]);
        assert_eq!(mpd.pick(Some("360")).unwrap()[0].id, "360");
    }

    #[test]
    fn live_number_template_follows_the_clock() {
        let live = r#"<MPD type="dynamic" availabilityStartTime="2026-01-01T00:00:00Z" timeShiftBufferDepth="PT10S">
          <Period start="PT0S"><AdaptationSet mimeType="video/mp4">
            <SegmentTemplate duration="2" media="s$Number$.m4s" startNumber="0"/>
            <Representation id="v" bandwidth="1" height="480"/>
          </AdaptationSet></Period></MPD>"#;
        let start = parse_datetime("2026-01-01T00:00:00Z").unwrap();
        let mpd = parse(live, &url(), start + 61.0).unwrap();
        let segs = &mpd.pick(None).unwrap()[0].media.segments;
        // 61 s in: segments 0..=29 are complete; the window holds 5 of them.
        assert_eq!(segs.iter().map(|s| s.sequence).collect::<Vec<_>>(), [25, 26, 27, 28, 29]);
        assert!(!mpd.pick(None).unwrap()[0].media.ended);
    }

    #[test]
    fn segment_lists_single_files_and_drm() {
        let list = r#"<MPD type="static" mediaPresentationDuration="PT6S"><Period><AdaptationSet mimeType="video/mp4">
          <Representation id="v" bandwidth="1" height="240"><BaseURL>video.mp4</BaseURL>
            <SegmentList duration="3"><Initialization range="0-99"/>
              <SegmentURL mediaRange="100-199"/><SegmentURL mediaRange="200-299"/></SegmentList>
          </Representation></AdaptationSet></Period></MPD>"#;
        let mpd = parse(list, &url(), 0.0).unwrap();
        let m = &mpd.pick(None).unwrap()[0].media;
        assert_eq!(m.init.as_ref().unwrap().range, Some((0, 100)));
        assert_eq!(m.segments[1].resource.range, Some((200, 100)));
        assert_eq!(m.segments[1].resource.url, "https://cdn.example/show/video.mp4");

        let single = r#"<MPD type="static" mediaPresentationDuration="PT6S"><Period><AdaptationSet mimeType="audio/mp4">
          <Representation id="a" bandwidth="1"><BaseURL>https://media.example/a.m4a</BaseURL><SegmentBase indexRange="0-500"/></Representation>
          </AdaptationSet></Period></MPD>"#;
        let mpd = parse(single, &url(), 0.0).unwrap();
        assert_eq!(mpd.pick(None).unwrap()[0].media.segments[0].resource.url, "https://media.example/a.m4a");

        let drm = VOD.replace(r#"<AdaptationSet contentType="video" mimeType="video/mp4">"#, r#"<AdaptationSet contentType="video" mimeType="video/mp4"><ContentProtection schemeIdUri="urn:uuid:edef8ba9"/>"#);
        assert!(parse(&drm, &url(), 0.0).unwrap().pick(None).unwrap_err().to_string().contains("DRM"));
    }

    #[test]
    fn durations_and_dates() {
        assert_eq!(parse_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_duration("P1DT1S"), Some(86_401.0));
        assert_eq!(parse_datetime("1970-01-02T00:00:00Z"), Some(86_400.0));
        assert_eq!(parse_datetime("2000-01-01T01:00:00+01:00"), parse_datetime("2000-01-01T00:00:00Z"));
        assert_eq!(expand("a$Number%05d$-$Time$-$$-$RepresentationID$", "v1", 9, 42, 7), "a00042-7-$-v1");
    }
}
