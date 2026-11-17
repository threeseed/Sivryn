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
        assert!((MIN_GAMMA..1.0).contains(&g));
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

/// One plane of an I420 picture (Y, U or V).
pub struct Plane<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

/// How much smaller the blurred background is computed (then smoothly
/// enlarged), and the box blur radius at that size.
const BLUR_SHRINK: usize = 8;
const BLUR_RADIUS: usize = 2;

/// Samples the frame into the segmentation model's input: `size`×`size`
/// RGB values in 0–1 (BT.601, limited range, like cameras send).
pub fn model_input(y: &Plane, u: &Plane, v: &Plane, size: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(size * size * 3);
    for my in 0..size {
        let py = (my * y.height) / size;
        for mx in 0..size {
            let px = (mx * y.width) / size;
            let l = f32::from(y.data[py * y.stride + px]) - 16.0;
            let cb = f32::from(u.data[(py / 2) * u.stride + px / 2]) - 128.0;
            let cr = f32::from(v.data[(py / 2) * v.stride + px / 2]) - 128.0;
            let r = 1.164 * l + 1.596 * cr;
            let g = 1.164 * l - 0.392 * cb - 0.813 * cr;
            let b = 1.164 * l + 2.017 * cb;
            out.extend([r, g, b].map(|c| (c / 255.0).clamp(0.0, 1.0)));
        }
    }
    out
}

/// A plane shrunk by `factor` (box average) and blurred.
fn shrink_and_blur(p: &Plane, factor: usize) -> (Vec<f32>, usize, usize) {
    let (w, h) = (p.width.div_ceil(factor), p.height.div_ceil(factor));
    let mut small = vec![0.0f32; w * h];
    let mut count = vec![0u32; w * h];
    for y in 0..p.height {
        let row = &p.data[y * p.stride..y * p.stride + p.width];
        let sy = y / factor;
        for (x, v) in row.iter().enumerate() {
            let i = sy * w + x / factor;
            small[i] += f32::from(*v);
            count[i] += 1;
        }
    }
    for (s, n) in small.iter_mut().zip(&count) {
        *s /= (*n).max(1) as f32;
    }
    // Two box passes per direction approximate a Gaussian.
    for _ in 0..2 {
        small = box_pass(&small, w, h, BLUR_RADIUS, true);
        small = box_pass(&small, w, h, BLUR_RADIUS, false);
    }
    (small, w, h)
}

fn box_pass(src: &[f32], w: usize, h: usize, r: usize, horizontal: bool) -> Vec<f32> {
    let mut out = vec![0.0f32; src.len()];
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    let at = |o: usize, i: usize| if horizontal { o * w + i } else { i * w + o };
    for o in 0..outer {
        for i in 0..inner {
            let (lo, hi) = (i.saturating_sub(r), (i + r).min(inner - 1));
            let sum: f32 = (lo..=hi).map(|k| src[at(o, k)]).sum();
            out[at(o, i)] = sum / (hi - lo + 1) as f32;
        }
    }
    out
}

/// Bilinear sample of a small map at fractional coordinates.
fn sample(map: &[f32], w: usize, h: usize, x: f32, y: f32) -> f32 {
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let top = map[y0 * w + x0] + (map[y0 * w + x1] - map[y0 * w + x0]) * fx;
    let bottom = map[y1 * w + x0] + (map[y1 * w + x1] - map[y1 * w + x0]) * fx;
    top + (bottom - top) * fy
}

/// Blurs a plane where `mask` (person probability on a `mask_size` square
/// stretched over the picture) says background.
pub fn blur_plane(p: &mut Plane, mask: &[f32], mask_size: usize) {
    let (small, sw, sh) = shrink_and_blur(p, BLUR_SHRINK);
    let (mx, my) = (mask_size as f32 / p.width as f32, mask_size as f32 / p.height as f32);
    let (bx, by) = (1.0 / BLUR_SHRINK as f32, 1.0 / BLUR_SHRINK as f32);
    for y in 0..p.height {
        let row = &mut p.data[y * p.stride..y * p.stride + p.width];
        let fy = y as f32 + 0.5;
        for (x, px) in row.iter_mut().enumerate() {
            let fx = x as f32 + 0.5;
            // Soften the edge a little so hair doesn't flicker.
            let person = smoothstep(0.3, 0.7, sample(mask, mask_size, mask_size, fx * mx - 0.5, fy * my - 0.5));
            if person >= 0.999 {
                continue;
            }
            let blurred = sample(&small, sw, sh, fx * bx - 0.5, fy * by - 0.5);
            *px = (f32::from(*px) * person + blurred * (1.0 - person)).round().clamp(0.0, 255.0) as u8;
        }
    }
}

fn smoothstep(lo: f32, hi: f32, v: f32) -> f32 {
    let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Background blur for a camera: segmentation runs on its own thread at
/// whatever rate it manages (it takes tens of milliseconds); every frame is
/// blurred with the newest mask, eased over time so edges don't jump.
pub struct BackgroundBlur {
    frames: std::sync::mpsc::SyncSender<Vec<f32>>,
    latest: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<Vec<f32>>>>>,
    mask: Vec<f32>,
}

impl Default for BackgroundBlur {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundBlur {
    pub fn new() -> Self {
        use crate::segment::Model;
        let (frames, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(1);
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let out = latest.clone();
        // Ends when `frames` is dropped with the camera.
        let _ = std::thread::Builder::new().name("sivryn-segment".into()).spawn(move || {
            let model = Model::bundled();
            while let Ok(rgb) = rx.recv() {
                let mask = std::sync::Arc::new(model.run(&rgb));
                *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(mask);
            }
        });
        Self { frames, latest, mask: Vec::new() }
    }

    /// Blurs the background of one I420 frame in place.
    pub fn apply(&mut self, mut y: Plane, mut u: Plane, mut v: Plane) {
        use crate::segment::SIZE;
        // Hand the segmenter a new frame when it is free.
        if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = self.frames.try_send(model_input(&y, &u, &v, SIZE))
        {
            return;
        }
        let newest = self.latest.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(newest) = newest else { return };
        if self.mask.len() != newest.len() {
            self.mask = newest.to_vec();
        } else {
            for (m, n) in self.mask.iter_mut().zip(newest.iter()) {
                *m += (n - *m) * 0.5;
            }
        }
        blur_plane(&mut y, &self.mask, SIZE);
        blur_plane(&mut u, &self.mask, SIZE);
        blur_plane(&mut v, &self.mask, SIZE);
    }
}

#[cfg(test)]
mod blur_tests {
    use super::*;

    #[test]
    fn person_stays_sharp_background_gets_smooth() {
        // A checkerboard: blurring turns it grey.
        let (w, h) = (64, 32);
        let mut data: Vec<u8> =
            (0..w * h).map(|i| if ((i % w) / 2 + (i / w) / 2) % 2 == 0 { 0 } else { 255 }).collect();
        let original = data.clone();
        // Left half person, right half background.
        let size = 4;
        let mask: Vec<f32> = (0..size * size).map(|i| if i % size < size / 2 { 1.0 } else { 0.0 }).collect();
        let mut plane = Plane { data: &mut data, stride: w, width: w, height: h };
        blur_plane(&mut plane, &mask, size);
        // Far left untouched, far right close to grey.
        for y in 0..h {
            assert_eq!(data[y * w..y * w + 8], original[y * w..y * w + 8]);
            for x in w - 8..w {
                let v = data[y * w + x];
                assert!((96..=160).contains(&v), "pixel {x},{y} = {v}");
            }
        }
    }

    #[test]
    fn model_input_converts_yuv_to_rgb() {
        // Limited-range white and black.
        let (mut yd, mut ud, mut vd) = (vec![235u8; 16], vec![128u8; 4], vec![128u8; 4]);
        for x in [0, 1, 4, 5] {
            yd[x] = 16;
        }
        let y = Plane { data: &mut yd, stride: 4, width: 4, height: 4 };
        let u = Plane { data: &mut ud, stride: 2, width: 2, height: 2 };
        let v = Plane { data: &mut vd, stride: 2, width: 2, height: 2 };
        let rgb = model_input(&y, &u, &v, 2);
        assert_eq!(rgb.len(), 12);
        assert!(rgb[..3].iter().all(|c| *c < 0.01));
        assert!(rgb[9..].iter().all(|c| *c > 0.99));
    }
}
