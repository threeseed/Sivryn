"""Converts MediaPipe's selfie segmenter (.tflite) into Sivryn's small graph
format (.svsg), which `sivryn-media`'s segmentation module runs without a
machine learning runtime.

Usage: python convert.py selfie_segmenter.tflite selfie_segmenter.svsg

Needs `ai-edge-litert` (for its flatbuffer schema) and `numpy`.

Format (little endian):
  magic b"SVSG0001"
  u32 input tensor, u32 output tensor
  u32 tensor count, then per tensor: 4 x u32 shape (N, H, W, C),
      u32 element count of constant data (0 for activations), f16 data
  u32 op count, then per op: u32 kind, u32 n, i32 inputs[n], u32 n,
      i32 outputs[n], u32 n, i32 params[n]
"""

import struct
import sys

import numpy as np
from ai_edge_litert import schema_py_generated as schema
from ai_edge_litert.tools import flatbuffer_utils

OPS = {
    "CONV_2D": 1,
    "DEPTHWISE_CONV_2D": 2,
    "AVERAGE_POOL_2D": 3,
    "RELU": 4,
    "HARD_SWISH": 5,
    "LOGISTIC": 6,
    "MUL": 7,
    "ADD": 8,
    "RESIZE_BILINEAR": 9,
    "Convolution2DTransposeBias": 10,
}
# TfLitePadding: SAME = 0, VALID = 1 in the schema; activations as in the schema.
BUILTIN = {v: k for k, v in schema.BuiltinOperator.__dict__.items() if not k.startswith("_")}


def shape4(shape):
    shape = list(shape)
    while len(shape) < 4:
        shape.insert(0, 1)
    return shape


def main(src, dst):
    model = flatbuffer_utils.read_model(src)
    graph = model.subgraphs[0]
    buffers = model.buffers

    def constant(tensor_index):
        t = graph.tensors[tensor_index]
        data = buffers[t.buffer].data
        if data is None or len(data) == 0:
            return None
        dtype = {schema.TensorType.FLOAT16: np.float16, schema.TensorType.FLOAT32: np.float32,
                 schema.TensorType.INT32: np.int32}[t.type]
        return np.frombuffer(bytes(data), dtype=dtype).reshape(t.shape)

    # DEQUANTIZE only turns float16 weights into float32: fold it away.
    alias = {}
    consts = {}
    ops = []
    for op in graph.operators:
        code = model.operatorCodes[op.opcodeIndex]
        builtin = max(code.builtinCode, code.deprecatedBuiltinCode)
        name = BUILTIN[builtin] if builtin != schema.BuiltinOperator.CUSTOM else code.customCode.decode()
        if name == "DEQUANTIZE":
            alias[op.outputs[0]] = op.inputs[0]
            continue
        ops.append((name, op))

    def resolve(i):
        return alias.get(i, i) if i >= 0 else i

    used = {graph.inputs[0], graph.outputs[0]}
    encoded = []
    for name, op in ops:
        ins = [resolve(i) for i in op.inputs]
        outs = list(op.outputs)
        opts = op.builtinOptions
        if name == "CONV_2D":
            params = [opts.strideH, opts.strideW, opts.padding, opts.fusedActivationFunction,
                      opts.dilationHFactor, opts.dilationWFactor]
        elif name == "DEPTHWISE_CONV_2D":
            params = [opts.strideH, opts.strideW, opts.padding, opts.fusedActivationFunction,
                      opts.depthMultiplier, opts.dilationHFactor, opts.dilationWFactor]
        elif name == "AVERAGE_POOL_2D":
            params = [opts.filterHeight, opts.filterWidth, opts.strideH, opts.strideW, opts.padding,
                      opts.fusedActivationFunction]
        elif name in ("MUL", "ADD"):
            params = [opts.fusedActivationFunction]
        elif name == "RESIZE_BILINEAR":
            params = [int(opts.alignCorners), int(opts.halfPixelCenters)]
            ins = ins[:1]  # the size comes from the output shape
        elif name == "Convolution2DTransposeBias":
            padding, stride_w, stride_h = struct.unpack("<3i", bytes(op.customOptions))
            params = [stride_h, stride_w, padding]
        elif name in ("RELU", "HARD_SWISH", "LOGISTIC"):
            params = []
        else:
            raise SystemExit(f"unsupported op {name}")
        for i in ins + outs:
            used.add(i)
            if i >= 0 and i not in consts:
                c = constant(i)
                if c is not None:
                    consts[i] = c
        encoded.append((OPS[name], ins, outs, params))

    # Renumber the tensors that are used.
    order = sorted(used)
    index = {t: n for n, t in enumerate(order)}
    out = bytearray(b"SVSG0001")
    out += struct.pack("<II", index[graph.inputs[0]], index[graph.outputs[0]])
    out += struct.pack("<I", len(order))
    for t in order:
        out += struct.pack("<4I", *shape4(graph.tensors[t].shape))
        c = consts.get(t)
        if c is None:
            out += struct.pack("<I", 0)
        else:
            flat = c.astype(np.float16).ravel()
            out += struct.pack("<I", flat.size) + flat.tobytes()
    out += struct.pack("<I", len(encoded))
    for kind, ins, outs, params in encoded:
        out += struct.pack("<II", kind, len(ins)) + struct.pack(f"<{len(ins)}i", *[index.get(i, -1) for i in ins])
        out += struct.pack("<I", len(outs)) + struct.pack(f"<{len(outs)}i", *[index[i] for i in outs])
        out += struct.pack("<I", len(params)) + struct.pack(f"<{len(params)}i", *params)
    with open(dst, "wb") as f:
        f.write(out)
    print(f"{len(order)} tensors, {len(encoded)} ops, {len(out)} bytes")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
