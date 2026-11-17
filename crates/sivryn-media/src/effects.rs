//! Camera effects applied to captured frames before encoding.
//!
//! - Low-light boost: lifts the brightness of dark pictures with a gamma
//!   curve chosen from the frame's average brightness, so a dim room looks
//!   lit without blowing out a bright one.

/// Average luma below which the boost starts to act (0–255).
const DARK: f32 = 110.0;
/// Average luma the boost aims for.
const TARGET: f32 = 120.0;
/// Strongest correction (gamma 0.45 lifts deep shadows a lot).
const MIN_GAMMA: f32 = 0.45;

/// Mean of a Y plane, sampling every 4th pixel of every 4th row.
pub fn mean_luma(y: &[u8], stride: usize, width: usize, height: usize) -> f32 {
    let (mut sum, mut n) = (0u64, 0u64);
    for row in (0..height).step_by(4) {
        let line = &y[row * stride..row * stride + width];
        for px in line.iter().step_by(4) {
            sum += u64::from(*px);
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { sum as f32 / n as f32 }
}

/// The gamma for a frame of this average brightness: 1.0 (no change) for
/// bright frames, lower for darker ones.
pub fn gamma_for(mean: f32) -> f32 {
    if mean >= DARK || mean <= 0.0 {
        return 1.0;
    }
    // Solve mean' = 255 * (mean/255)^g = TARGET for g.
    let g = (TARGET / 255.0).ln() / (mean / 255.0).ln();
    g.clamp(MIN_GAMMA, 1.0)
}

/// Lookup table for a gamma curve over 0–255 (limited-range video luma).
pub fn gamma_lut(gamma: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (255.0 * (i as f32 / 255.0).powf(gamma)).round().clamp(0.0, 255.0) as u8;
    }
    lut
}

/// Smooths the gamma from frame to frame so the picture doesn't flicker.
#[derive(Debug, Default)]
pub struct LowLight {
    gamma: Option<f32>,
}

impl LowLight {
    /// Brightens a Y plane in place if it is dark.
    pub fn apply(&mut self, y: &mut [u8], stride: usize, width: usize, height: usize) {
        let target = gamma_for(mean_luma(y, stride, width, height));
        let gamma = match self.gamma {
            Some(g) => g + (target - g) * 0.1,
            None => target,
        };
        self.gamma = Some(gamma);
        if gamma > 0.99 {
            return;
        }
        let lut = gamma_lut(gamma);
        for row in 0..height {
            for px in &mut y[row * stride..row * stride + width] {
                *px = lut[*px as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bright_frames_are_untouched() {
        assert_eq!(gamma_for(160.0), 1.0);
        let mut y = vec![160u8; 64 * 64];
        LowLight::default().apply(&mut y, 64, 64, 64);
        assert!(y.iter().all(|v| *v == 160));
    }

    #[test]
    fn dark_frames_get_brighter_up_to_a_limit() {
        let g = gamma_for(40.0);
        assert!(g < 1.0 && g >= MIN_GAMMA);
        assert!(gamma_for(5.0) == MIN_GAMMA);
        let mut y = vec![40u8; 64 * 64];
        LowLight::default().apply(&mut y, 64, 64, 64);
        let after = mean_luma(&y, 64, 64, 64);
        assert!(after > 90.0, "mean after boost {after}");
        // Black stays black, white stays white.
        let lut = gamma_lut(g);
        assert_eq!((lut[0], lut[255]), (0, 255));
    }

    #[test]
    fn respects_stride_padding() {
        // 4x4 picture in rows of 8 bytes; padding must not change.
        let mut y = vec![20u8; 8 * 4];
        for row in 0..4 {
            for b in &mut y[row * 8 + 4..row * 8 + 8] {
                *b = 7;
            }
        }
        LowLight::default().apply(&mut y, 8, 4, 4);
        assert!((0..4).all(|r| y[r * 8 + 4..r * 8 + 8].iter().all(|b| *b == 7)));
        assert!(y[0] > 20);
    }
}
