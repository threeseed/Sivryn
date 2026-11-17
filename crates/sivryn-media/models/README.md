# Models

`selfie_segmenter.svsg` is MediaPipe's selfie segmenter
(`selfie_segmenter.tflite`, float16, from
`storage.googleapis.com/mediapipe-models/image_segmenter/selfie_segmenter/`),
licensed under the Apache License 2.0 by Google LLC. It was converted with
`tools/segmentation/convert.py`; the weights are unchanged (float16).

The executor in `src/segment.rs` was checked against LiteRT on the same
input: the largest difference in the output mask was 0.0002.
