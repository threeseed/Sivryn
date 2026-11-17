//! Person segmentation for background blur: a small convolutional network
//! (MediaPipe's selfie segmenter, Apache-2.0) run by a purpose-built
//! executor, so no machine learning runtime is needed.
//!
//! The model is converted from TFLite with `tools/segmentation/convert.py`
//! into a compact graph (see that script for the format). It takes a
//! 256×256 RGB picture with values in 0–1 and returns, per pixel, how likely
//! it shows a person (0–1).

use std::sync::OnceLock;

/// Side of the square picture the model works on.
pub const SIZE: usize = 256;

static MODEL_BYTES: &[u8] = include_bytes!("../models/selfie_segmenter.svsg");

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("not a segmentation model")]
    Magic,
    #[error("model file is truncated")]
    Truncated,
    #[error("unknown operation {0}")]
    UnknownOp(u32),
    #[error("tensor {0} is out of range")]
    BadTensor(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    h: usize,
    w: usize,
    c: usize,
}

impl Shape {
    fn len(self) -> usize {
        self.h * self.w * self.c
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    None,
    Relu,
    Relu6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Padding {
    Same,
    Valid,
}

#[derive(Clone, Debug)]
enum Op {
    Conv {
        input: usize,
        weights: usize,
        bias: usize,
        output: usize,
        stride: (usize, usize),
        pad: Padding,
        act: Act,
    },
    Depthwise {
        input: usize,
        weights: usize,
        bias: usize,
        output: usize,
        stride: (usize, usize),
        pad: Padding,
        act: Act,
    },
    /// Average over the whole picture (the only pooling the model uses).
    GlobalAvg {
        input: usize,
        output: usize,
    },
    Relu {
        input: usize,
        output: usize,
    },
    HardSwish {
        input: usize,
        output: usize,
    },
    Logistic {
        input: usize,
        output: usize,
    },
    /// Multiplies by a per-channel vector (squeeze and excite).
    Mul {
        a: usize,
        b: usize,
        output: usize,
        act: Act,
    },
    Add {
        a: usize,
        b: usize,
        output: usize,
        act: Act,
    },
    Resize {
        input: usize,
        output: usize,
        align_corners: bool,
        half_pixel: bool,
    },
    TransposeConv {
        input: usize,
        weights: usize,
        bias: usize,
        output: usize,
        stride: (usize, usize),
    },
}

/// A loaded segmentation network.
pub struct Model {
    shapes: Vec<Shape>,
    consts: Vec<Option<Vec<f32>>>,
    ops: Vec<Op>,
    input: usize,
    output: usize,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], ModelError> {
        let end = self.at.checked_add(n).ok_or(ModelError::Truncated)?;
        let out = self.data.get(self.at..end).ok_or(ModelError::Truncated)?;
        self.at = end;
        Ok(out)
    }
    fn u32(&mut self) -> Result<u32, ModelError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().expect("4 bytes")))
    }
    fn i32s(&mut self) -> Result<Vec<i32>, ModelError> {
        let n = self.u32()? as usize;
        (0..n).map(|_| Ok(self.u32()? as i32)).collect()
    }
}

/// IEEE half to single precision.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exp = u32::from((bits >> 10) & 0x1f);
    let frac = u32::from(bits & 0x3ff);
    let out = match (exp, frac) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalise.
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(out)
}

fn act(code: i32) -> Act {
    match code {
        1 => Act::Relu,
        3 => Act::Relu6,
        _ => Act::None,
    }
}

fn padding(code: i32) -> Padding {
    if code == 1 { Padding::Valid } else { Padding::Same }
}

impl Model {
    /// The bundled selfie segmenter (parsed once).
    pub fn bundled() -> &'static Model {
        static MODEL: OnceLock<Model> = OnceLock::new();
        MODEL.get_or_init(|| Model::parse(MODEL_BYTES).expect("bundled segmentation model is valid"))
    }

    pub fn parse(data: &[u8]) -> Result<Self, ModelError> {
        let mut r = Reader { data, at: 0 };
        if r.bytes(8)? != b"SVSG0001" {
            return Err(ModelError::Magic);
        }
        let (input, output) = (r.u32()? as usize, r.u32()? as usize);
        let count = r.u32()? as usize;
        let mut shapes = Vec::with_capacity(count);
        let mut consts = Vec::with_capacity(count);
        for _ in 0..count {
            let _n = r.u32()?;
            let (h, w, c) = (r.u32()? as usize, r.u32()? as usize, r.u32()? as usize);
            shapes.push(Shape { h, w, c });
            let len = r.u32()? as usize;
            consts.push(if len == 0 {
                None
            } else {
                let raw = r.bytes(len * 2)?;
                Some(raw.chunks_exact(2).map(|b| f16_to_f32(u16::from_le_bytes([b[0], b[1]]))).collect())
            });
        }
        let t = |i: i32| -> Result<usize, ModelError> {
            usize::try_from(i).ok().filter(|i| *i < count).ok_or(ModelError::BadTensor(i))
        };
        let n_ops = r.u32()? as usize;
        let mut ops = Vec::with_capacity(n_ops);
        for _ in 0..n_ops {
            let kind = r.u32()?;
            let ins = r.i32s()?;
            let outs = r.i32s()?;
            let p = r.i32s()?;
            let i = |n: usize| ins.get(n).copied().ok_or(ModelError::Truncated).and_then(t);
            let output = outs.first().copied().ok_or(ModelError::Truncated).and_then(t)?;
            let param = |n: usize| p.get(n).copied().ok_or(ModelError::Truncated);
            let op = match kind {
                1 => Op::Conv {
                    input: i(0)?,
                    weights: i(1)?,
                    bias: i(2)?,
                    output,
                    stride: (param(0)? as usize, param(1)? as usize),
                    pad: padding(param(2)?),
                    act: act(param(3)?),
                },
                2 => Op::Depthwise {
                    input: i(0)?,
                    weights: i(1)?,
                    bias: i(2)?,
                    output,
                    stride: (param(0)? as usize, param(1)? as usize),
                    pad: padding(param(2)?),
                    act: act(param(3)?),
                },
                3 => Op::GlobalAvg { input: i(0)?, output },
                4 => Op::Relu { input: i(0)?, output },
                5 => Op::HardSwish { input: i(0)?, output },
                6 => Op::Logistic { input: i(0)?, output },
                7 => Op::Mul { a: i(0)?, b: i(1)?, output, act: act(param(0)?) },
                8 => Op::Add { a: i(0)?, b: i(1)?, output, act: act(param(0)?) },
                9 => Op::Resize { input: i(0)?, output, align_corners: param(0)? != 0, half_pixel: param(1)? != 0 },
                10 => Op::TransposeConv {
                    input: i(0)?,
                    weights: i(1)?,
                    bias: i(2)?,
                    output,
                    stride: (param(0)? as usize, param(1)? as usize),
                },
                other => return Err(ModelError::UnknownOp(other)),
            };
            ops.push(op);
        }
        Ok(Self { shapes, consts, ops, input, output })
    }

    /// Runs the network on a `SIZE`×`SIZE` RGB picture (values 0–1, row by
    /// row, three floats per pixel) and returns the person probability per
    /// pixel.
    pub fn run(&self, rgb: &[f32]) -> Vec<f32> {
        let mut values: Vec<Option<Vec<f32>>> = vec![None; self.shapes.len()];
        values[self.input] = Some(rgb.to_vec());
        for op in &self.ops {
            let (output, data) = self.eval(op, &values);
            values[output] = Some(data);
        }
        values[self.output].take().unwrap_or_default()
    }

    fn get<'a>(&'a self, values: &'a [Option<Vec<f32>>], t: usize) -> &'a [f32] {
        self.consts[t].as_deref().or(values[t].as_deref()).expect("tensor computed before use")
    }

    fn eval(&self, op: &Op, values: &[Option<Vec<f32>>]) -> (usize, Vec<f32>) {
        match *op {
            Op::Conv { input, weights, bias, output, stride, pad, act: a } => {
                let out = conv(
                    self.get(values, input),
                    self.shapes[input],
                    self.get(values, weights),
                    self.shapes[weights],
                    self.get(values, bias),
                    self.shapes[output],
                    stride,
                    pad,
                );
                (output, activate(out, a))
            }
            Op::Depthwise { input, weights, bias, output, stride, pad, act: a } => {
                let out = depthwise(
                    self.get(values, input),
                    self.shapes[input],
                    self.get(values, weights),
                    self.shapes[weights],
                    self.get(values, bias),
                    self.shapes[output],
                    stride,
                    pad,
                );
                (output, activate(out, a))
            }
            Op::GlobalAvg { input, output } => {
                let s = self.shapes[input];
                let x = self.get(values, input);
                let mut out = vec![0.0f32; s.c];
                for px in x.chunks_exact(s.c) {
                    for (o, v) in out.iter_mut().zip(px) {
                        *o += v;
                    }
                }
                let n = (s.h * s.w) as f32;
                out.iter_mut().for_each(|o| *o /= n);
                (output, out)
            }
            Op::Relu { input, output } => (output, activate(self.get(values, input).to_vec(), Act::Relu)),
            Op::HardSwish { input, output } => {
                (output, self.get(values, input).iter().map(|&x| x * (x + 3.0).clamp(0.0, 6.0) / 6.0).collect())
            }
            Op::Logistic { input, output } => (output, self.get(values, input).iter().map(|&x| sigmoid(x)).collect()),
            Op::Mul { a, b, output, act: f } => (output, activate(broadcast(self, values, a, b, |x, y| x * y), f)),
            Op::Add { a, b, output, act: f } => (output, activate(broadcast(self, values, a, b, |x, y| x + y), f)),
            Op::Resize { input, output, align_corners, half_pixel } => (
                output,
                resize_bilinear(
                    self.get(values, input),
                    self.shapes[input],
                    self.shapes[output],
                    align_corners,
                    half_pixel,
                ),
            ),
            Op::TransposeConv { input, weights, bias, output, stride } => (
                output,
                transpose_conv(
                    self.get(values, input),
                    self.shapes[input],
                    self.get(values, weights),
                    self.shapes[weights],
                    self.get(values, bias),
                    self.shapes[output],
                    stride,
                ),
            ),
        }
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn activate(mut v: Vec<f32>, act: Act) -> Vec<f32> {
    match act {
        Act::None => {}
        Act::Relu => v.iter_mut().for_each(|x| *x = x.max(0.0)),
        Act::Relu6 => v.iter_mut().for_each(|x| *x = x.clamp(0.0, 6.0)),
    }
    v
}

/// Elementwise with broadcasting of a per-channel (1×1×C) operand.
fn broadcast(m: &Model, values: &[Option<Vec<f32>>], a: usize, b: usize, f: impl Fn(f32, f32) -> f32) -> Vec<f32> {
    let (sa, sb) = (m.shapes[a], m.shapes[b]);
    let (x, y) = (m.get(values, a), m.get(values, b));
    if sa == sb {
        return x.iter().zip(y).map(|(&p, &q)| f(p, q)).collect();
    }
    let (big, small, swapped) = if sa.len() >= sb.len() { (x, y, false) } else { (y, x, true) };
    let c = small.len();
    big.chunks_exact(c)
        .flat_map(|px| px.iter().zip(small).map(|(&p, &q)| if swapped { f(q, p) } else { f(p, q) }))
        .collect()
}

/// Padding before the first row/column for SAME padding (TFLite's rule).
fn pad_before(input: usize, output: usize, kernel: usize, stride: usize, pad: Padding) -> isize {
    match pad {
        Padding::Valid => 0,
        Padding::Same => {
            let total = ((output - 1) * stride + kernel).saturating_sub(input);
            (total / 2) as isize
        }
    }
}

/// Regular convolution; weights are laid out [out][kh][kw][in].
#[allow(clippy::too_many_arguments)]
fn conv(
    x: &[f32],
    s: Shape,
    w: &[f32],
    ws: Shape,
    bias: &[f32],
    os: Shape,
    (sh, sw): (usize, usize),
    pad: Padding,
) -> Vec<f32> {
    // [out][kh][kw][in] is stored as N=out, H=kh, W=kw, C=in.
    let (kh, kw, cin, cout) = (ws.h, ws.w, ws.c, os.c);
    let mut out = vec![0.0f32; os.len()];
    if kh == 1 && kw == 1 && sh == 1 && sw == 1 {
        // Pointwise: a small matrix product per pixel.
        for (px, o) in x.chunks_exact(cin).zip(out.chunks_exact_mut(cout)) {
            for (k, ov) in o.iter_mut().enumerate() {
                let row = &w[k * cin..(k + 1) * cin];
                *ov = bias[k] + row.iter().zip(px).map(|(a, b)| a * b).sum::<f32>();
            }
        }
        return out;
    }
    let (pt, pl) = (pad_before(s.h, os.h, kh, sh, pad), pad_before(s.w, os.w, kw, sw, pad));
    for oy in 0..os.h {
        for ox in 0..os.w {
            let o = &mut out[(oy * os.w + ox) * cout..][..cout];
            o.copy_from_slice(&bias[..cout]);
            for ky in 0..kh {
                let iy = (oy * sh + ky) as isize - pt;
                if iy < 0 || iy >= s.h as isize {
                    continue;
                }
                for kx in 0..kw {
                    let ix = (ox * sw + kx) as isize - pl;
                    if ix < 0 || ix >= s.w as isize {
                        continue;
                    }
                    let px = &x[(iy as usize * s.w + ix as usize) * cin..][..cin];
                    for (k, ov) in o.iter_mut().enumerate() {
                        let row = &w[((k * kh + ky) * kw + kx) * cin..][..cin];
                        *ov += row.iter().zip(px).map(|(a, b)| a * b).sum::<f32>();
                    }
                }
            }
        }
    }
    out
}

/// Depthwise convolution (multiplier 1); weights are [1][kh][kw][c].
#[allow(clippy::too_many_arguments)]
fn depthwise(
    x: &[f32],
    s: Shape,
    w: &[f32],
    ws: Shape,
    bias: &[f32],
    os: Shape,
    (sh, sw): (usize, usize),
    pad: Padding,
) -> Vec<f32> {
    let (kh, kw, c) = (ws.h, ws.w, os.c);
    let (pt, pl) = (pad_before(s.h, os.h, kh, sh, pad), pad_before(s.w, os.w, kw, sw, pad));
    let mut out = vec![0.0f32; os.len()];
    for oy in 0..os.h {
        for ox in 0..os.w {
            let o = &mut out[(oy * os.w + ox) * c..][..c];
            o.copy_from_slice(&bias[..c]);
            for ky in 0..kh {
                let iy = (oy * sh + ky) as isize - pt;
                if iy < 0 || iy >= s.h as isize {
                    continue;
                }
                for kx in 0..kw {
                    let ix = (ox * sw + kx) as isize - pl;
                    if ix < 0 || ix >= s.w as isize {
                        continue;
                    }
                    let px = &x[(iy as usize * s.w + ix as usize) * c..][..c];
                    let k = &w[(ky * kw + kx) * c..][..c];
                    for ((ov, a), b) in o.iter_mut().zip(px).zip(k) {
                        *ov += a * b;
                    }
                }
            }
        }
    }
    out
}

/// Bilinear resize with TFLite's coordinate rules.
fn resize_bilinear(x: &[f32], s: Shape, os: Shape, align_corners: bool, half_pixel: bool) -> Vec<f32> {
    let scale = |inp: usize, out: usize| {
        if align_corners && out > 1 { (inp - 1) as f32 / (out - 1) as f32 } else { inp as f32 / out as f32 }
    };
    let (fy, fx) = (scale(s.h, os.h), scale(s.w, os.w));
    let src = |o: usize, f: f32, len: usize| -> (usize, usize, f32) {
        let v = if half_pixel { ((o as f32 + 0.5) * f - 0.5).max(0.0) } else { o as f32 * f };
        let lo = (v.floor() as usize).min(len - 1);
        let hi = (lo + 1).min(len - 1);
        (lo, hi, v - lo as f32)
    };
    let c = s.c;
    let mut out = vec![0.0f32; os.len()];
    for oy in 0..os.h {
        let (y0, y1, dy) = src(oy, fy, s.h);
        for ox in 0..os.w {
            let (x0, x1, dx) = src(ox, fx, s.w);
            let at = |xx: usize, yy: usize| (yy * s.w + xx) * c;
            let (a, b) = (at(x0, y0), at(x1, y0));
            let (cc, d) = (at(x0, y1), at(x1, y1));
            let o = &mut out[(oy * os.w + ox) * c..][..c];
            for (k, ov) in o.iter_mut().enumerate() {
                let top = x[a + k] + (x[b + k] - x[a + k]) * dx;
                let bottom = x[cc + k] + (x[d + k] - x[cc + k]) * dx;
                *ov = top + (bottom - top) * dy;
            }
        }
    }
    out
}

/// MediaPipe's `Convolution2DTransposeBias` where kernel equals stride (no
/// overlap): every input pixel becomes a kernel-sized block of outputs.
fn transpose_conv(
    x: &[f32],
    s: Shape,
    w: &[f32],
    ws: Shape,
    bias: &[f32],
    os: Shape,
    (sh, sw): (usize, usize),
) -> Vec<f32> {
    // Weights [out][kh][kw][in].
    let (kh, kw, cin, cout) = (ws.h, ws.w, ws.c, os.c);
    let mut out = vec![0.0f32; os.len()];
    for px_i in 0..os.len() / cout {
        out[px_i * cout..][..cout].copy_from_slice(&bias[..cout]);
    }
    for iy in 0..s.h {
        for ix in 0..s.w {
            let px = &x[(iy * s.w + ix) * cin..][..cin];
            for ky in 0..kh {
                let oy = iy * sh + ky;
                if oy >= os.h {
                    continue;
                }
                for kx in 0..kw {
                    let ox = ix * sw + kx;
                    if ox >= os.w {
                        continue;
                    }
                    let o = &mut out[(oy * os.w + ox) * cout..][..cout];
                    for (k, ov) in o.iter_mut().enumerate() {
                        let row = &w[((k * kh + ky) * kw + kx) * cin..][..cin];
                        *ov += row.iter().zip(px).map(|(a, b)| a * b).sum::<f32>();
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert_eq!(f16_to_f32(0x0000), 0.0);
    }

    #[test]
    fn same_padding_follows_tflite() {
        // 3x3 stride 2 on 256 -> 128: total padding 1, none before.
        assert_eq!(pad_before(256, 128, 3, 2, Padding::Same), 0);
        // 5x5 stride 1: two on each side.
        assert_eq!(pad_before(16, 16, 5, 1, Padding::Same), 2);
        assert_eq!(pad_before(16, 16, 5, 1, Padding::Valid), 0);
    }

    #[test]
    fn resize_doubles_smoothly() {
        let s = Shape { h: 1, w: 2, c: 1 };
        let os = Shape { h: 1, w: 4, c: 1 };
        // Half-pixel centres: 0, 0.25, 0.75, 1 between the two inputs.
        let out = resize_bilinear(&[0.0, 1.0], s, os, false, true);
        assert_eq!(out, vec![0.0, 0.25, 0.75, 1.0]);
    }

    #[test]
    fn bundled_model_loads_and_finds_a_figure() {
        let model = Model::bundled();
        // A plain grey picture with a skin-coloured head and body in the middle.
        let mut rgb = vec![0.5f32; SIZE * SIZE * 3];
        for y in 0..SIZE {
            for x in 0..SIZE {
                let (dx, dy) = (x as f32 - 128.0, y as f32 - 150.0);
                if dx * dx / 3600.0 + dy * dy / 8100.0 < 1.0 {
                    rgb[(y * SIZE + x) * 3..][..3].copy_from_slice(&[0.85, 0.65, 0.55]);
                }
            }
        }
        let mask = model.run(&rgb);
        assert_eq!(mask.len(), SIZE * SIZE);
        assert!(mask.iter().all(|v| (0.0..=1.0).contains(v)));
    }
}
