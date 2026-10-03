Patches applied to FFmpeg by `scripts/build-frame-media.sh` (Steam Frame build only).

- `0001-aarch64-hevc-10bit-pel-pixels-neon.patch`: NEON versions of the 10-bit
  HEVC full-sample MC functions (`pel_pixels`, `pel_bi_pixels`, `pel_uni_w_pixels`),
  which FFmpeg 8.1.3 (and master as of 2026-09) only implements in C at 10-bit.
  Verified bit-exact with `checkasm --test=hevc_pel` (376/376) on the headset.
  Steam Frame: 4K60 HEVC Main10 142 → 178 fps; 8K60 unchanged (memory-latency bound).
  Upstream candidate.
- `0002-v4l2m2m-dec-flush-on-seek.patch`: gives the V4L2 mem2mem decoders a
  `flush` callback. Without it `avcodec_flush_buffers()` leaves the bitstream
  already queued in the driver, so after a seek ~20 frames from the old
  position still come out first. The callback stops and restarts the OUTPUT
  queue (the stateful decoder interface's seek sequence). Steam Frame, 6K60
  HEVC over SMB: keyframe seeks 0.4–1.7 s → 0.07–0.5 s, no stale frames.
