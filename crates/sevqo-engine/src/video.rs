use crate::config::Settings;

/// Clip windows for one video, in seconds.
///
/// The encoder samples at most 32 frames per input, so `window_s` and `fps` are
/// clamped together: a 32 s window at 1 fps is the longest span that keeps every
/// second, and anything longer would silently drop the tail of the window.
pub fn plan_clips(duration_s: f64, settings: &Settings) -> Vec<(f64, f64)> {
    if duration_s <= 0.0 || !duration_s.is_finite() {
        return Vec::new();
    }
    let window = settings.video_window_s.clamp(2.0, 32.0 / settings.video_fps.max(0.1));
    let step = settings.video_step_s.clamp(1.0, window);
    let mut out = Vec::new();
    let mut start = 0.0f64;
    while start < duration_s {
        let end = (start + window).min(duration_s);
        // a sliver at the tail is not worth an embedding; extend the previous clip
        if duration_s - end < 1.0 && end < duration_s {
            out.push((start, duration_s));
            break;
        }
        out.push((start, end));
        if end >= duration_s {
            break;
        }
        start += step;
    }
    out
}

pub fn clip_count_for(duration_s: f64, settings: &Settings) -> usize {
    plan_clips(duration_s, settings).len()
}

/// Seconds of media per inference call, for progress estimates.
pub fn estimate_video_seconds(duration_s: f64, settings: &Settings) -> f64 {
    plan_clips(duration_s, settings).iter().map(|(a, b)| b - a).sum()
}

pub fn format_ms(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(window: f64, step: f64, fps: f64) -> Settings {
        let mut st = Settings::default();
        st.video_window_s = window;
        st.video_step_s = step;
        st.video_fps = fps;
        st.validate();
        st
    }

    #[test]
    fn covers_whole_video_without_losing_tail() {
        let st = s(16.0, 12.0, 1.0);
        let clips = plan_clips(60.0, &st);
        assert_eq!(clips.first().unwrap().0, 0.0);
        assert_eq!(clips.last().unwrap().1, 60.0);
        // contiguous coverage: each step advances by exactly `step`
        for w in clips.windows(2) {
            assert!((w[1].0 - w[0].0 - st.video_step_s).abs() < 0.001);
        }
    }

    #[test]
    fn tail_sliver_is_merged_not_dropped() {
        let st = s(10.0, 10.0, 1.0);
        let clips = plan_clips(25.0, &st);
        assert_eq!(clips.last().unwrap().1, 25.0);
        assert!(clips.iter().all(|(a, b)| b - a >= 4.0), "{clips:?}");
    }

    #[test]
    fn short_video_is_single_clip() {
        let st = s(16.0, 12.0, 1.0);
        assert_eq!(plan_clips(5.0, &st), vec![(0.0, 5.0)]);
    }

    #[test]
    fn zero_or_bad_duration_yields_nothing() {
        let st = s(16.0, 12.0, 1.0);
        assert!(plan_clips(0.0, &st).is_empty());
        assert!(plan_clips(-3.0, &st).is_empty());
    }

    #[test]
    fn window_respects_frame_cap() {
        // 32 frames at 1 fps => 32 s is the widest window that keeps full coverage
        let st = s(60.0, 30.0, 1.0);
        assert!(st.video_window_s <= 32.0, "validate() must clamp: {:?}", st.video_window_s);
        let clips = plan_clips(300.0, &st);
        assert!(clips.iter().all(|(a, b)| b - a <= 32.0 + f64::EPSILON));
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(format_ms(0), "0:00");
        assert_eq!(format_ms(138_000), "2:18");
        assert_eq!(format_ms(3_723_000), "1:02:03");
    }
}
