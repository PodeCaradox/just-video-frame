#ifndef JUST_VIDEO_MEDIA_H
#define JUST_VIDEO_MEDIA_H
#include <stdint.h>

// Player media layer. Input bytes come from Rust through callbacks (SMB or a
// local file), so FFmpeg never sees credentials or opens network URLs itself.

// Returns bytes read, 0 at end of file, or a negative AVERROR.
typedef int (*jv_read_fn)(void *opaque, uint8_t *buf, int size);
// fseek-style offset/whence; also answers AVSEEK_SIZE. Negative AVERROR on failure.
typedef int64_t (*jv_seek_fn)(void *opaque, int64_t offset, int whence);

typedef struct JVMedia JVMedia;

typedef struct {
    char container[64];
    double duration_seconds;
    int64_t bit_rate;
    // Video (codec is empty when the file has no video stream).
    char video_codec[32];
    char video_profile[48];
    char pixel_format[32];
    int32_t width;
    int32_t height;
    int32_t bit_depth;
    double fps;
    // Container/codec VR hints; empty when absent. Names come from FFmpeg.
    char stereo_mode[32];
    int32_t stereo_inverted;
    char projection[48];
    // Equirectangular crop bounds, 0.32 fixed point fractions of the full sphere.
    uint32_t bound_left, bound_top, bound_right, bound_bottom;
    // Audio (codec is empty when absent).
    char audio_codec[32];
    int32_t audio_channels;
    int32_t audio_sample_rate;
} JVMediaInfo;

typedef struct {
    int32_t frames;
    int32_t hardware_frames;
    int32_t software_frames;
    double elapsed_seconds;
    char decoder[48];
    char hw_backend[16];
    char pixel_format[32];
    // Why hardware decoding was not used, when software fallback happened.
    char note[128];
    char error[256];
} JVDecodeStats;

// `name` is used only for FFmpeg's format probing hints and messages.
JVMedia *jv_media_open(const char *name, jv_read_fn read, jv_seek_fn seek, void *opaque,
                       JVMediaInfo *info, char *error, int error_size);

// Decodes up to `frame_limit` video frames. `hw_backend` is an FFmpeg device
// type name such as "vulkan"/"vaapi", "v4l2m2m" for the V4L2 stateful decoder
// (Steam Frame), or NULL for software. With
// `allow_software`, frames are still counted if the decoder falls back.
// `decoder_options` is NULL or "key=value:key=value" of FFmpeg codec options
// (e.g. "threads=8:thread_type=frame"), applied when the decoder opens.
int jv_media_decode(JVMedia *media, const char *hw_backend, int allow_software,
                    const char *decoder_options, int frame_limit, JVDecodeStats *stats);

// Frame-by-frame decoding for playback.
typedef struct JVDecoder JVDecoder;

enum { JV_LAYOUT_PLANAR = 0, JV_LAYOUT_SEMIPLANAR = 1, JV_LAYOUT_SEMIPLANAR_MSB = 2 };
enum { JV_MATRIX_BT709 = 0, JV_MATRIX_BT601 = 1, JV_MATRIX_BT2020 = 2 };
enum { JV_TRANSFER_SDR = 0, JV_TRANSFER_PQ = 1, JV_TRANSFER_HLG = 2 };

typedef struct {
    void *handle;            // owned; release with jv_frame_release
    int32_t layout;          // JV_LAYOUT_*: Y,U,V planes / Y + interleaved UV (NV12, P010)
    int32_t width, height;
    int32_t bits;            // 8 or 10; PLANAR 10-bit is LSB-aligned, P010 MSB-aligned
    int32_t plane_count;
    const uint8_t *data[3];
    int32_t linesize[3];
    double pts;              // seconds from stream start; < 0 when unknown
    int32_t matrix;          // JV_MATRIX_*
    int32_t full_range;
    int32_t transfer;        // JV_TRANSFER_*
    int32_t hardware;        // produced by a hardware decoder
} JVFrame;

// Opens the video decoder (same selection/fallback rules as jv_media_decode);
// fills `stats` decoder/hw_backend/note/error. NULL on failure.
JVDecoder *jv_decoder_open(JVMedia *media, const char *hw_backend, int allow_software,
                           const char *decoder_options, JVDecodeStats *stats);
// 0 with a frame, AVERROR_EOF at the end, AVERROR_PATCHWELCOME for an
// unsupported pixel format, other negative AVERROR on failure.
int jv_decoder_next(JVDecoder *decoder, JVFrame *frame);
int jv_decoder_seek(JVDecoder *decoder, double seconds);
int jv_decoder_reopen_video(JVDecoder *decoder);
// While catching up after a seek, skip non-reference frames of packets before
// `seconds` (they would be discarded anyway); <= 0 turns it off.
void jv_decoder_skip_nonref_until(JVDecoder *decoder, double seconds);
// The indexed keyframe at or before (`after` = 0) or at or after `seconds`, in
// seconds from the video start; < 0 when the index has none.
double jv_decoder_keyframe(JVDecoder *decoder, double seconds, int after);
// Also decode the audio stream, resampled to interleaved float at `rate` Hz
// with `channels` channels. AVERROR_STREAM_NOT_FOUND when there is none.
int jv_decoder_enable_audio(JVDecoder *decoder, int rate, int channels);
// Decoded audio frames waiting to be read (audio arrives as a side effect of
// jv_decoder_next reading packets).
int jv_decoder_audio_available(const JVDecoder *decoder);
// Copies up to `frames` frames into `out`; `pts` receives the time of the
// first frame (seconds from video start, < 0 if unknown). Returns frames copied.
int jv_decoder_audio_read(JVDecoder *decoder, float *out, int frames, double *pts);
// Subtitle tracks: the file's subtitle streams, in order.
typedef struct {
    char codec[32];
    char language[16];
    char title[64];
    int32_t is_default;
    int32_t forced;
    int32_t supported;       // we can decode and show it (text or bitmap)
} JVSubtitleTrack;

// Audio tracks: the file's audio streams, in order.
typedef struct {
    char codec[32];
    char language[16];
    char title[64];
    int32_t channels;
    int32_t is_default;
} JVAudioTrack;

int jv_media_audio_count(const JVMedia *media);
int jv_media_audio_track(const JVMedia *media, int track, JVAudioTrack *out);
// The audio track played (index into the audio tracks), -1 for none.
int jv_media_current_audio(const JVMedia *media);
// Plays audio track `track` instead (after jv_decoder_enable_audio). Seek
// afterwards so the new track starts where the picture is.
int jv_decoder_select_audio(JVDecoder *decoder, int track);

int jv_media_subtitle_count(const JVMedia *media);
int jv_media_subtitle_track(const JVMedia *media, int track, JVSubtitleTrack *out);

// A decoded subtitle: dialogue text (ASS override tags still inside) or,
// for DVD/Blu-ray subtitles, a picture.
typedef struct {
    double start, end;       // seconds from video start
    int32_t clear;           // an erase event: whatever shows at `start` ends there
    char text[1024];
    // Bitmap subtitles: premultiplied RGBA, owned by the cue (free with
    // jv_free), placed at x, y in a frame_width × frame_height picture.
    uint8_t *rgba;
    int32_t x, y, width, height;
    int32_t frame_width, frame_height;
} JVSubtitleCue;

void jv_free(void *pointer);

// Decodes `track` (index into the subtitle tracks, -1 for none) as a side
// effect of jv_decoder_next, like audio.
int jv_decoder_select_subtitle(JVDecoder *decoder, int track);
// Takes the next decoded cue: 1 with a cue, 0 when none is waiting.
int jv_decoder_subtitle_read(JVDecoder *decoder, JVSubtitleCue *out);
void jv_frame_release(void *handle);
void jv_decoder_close(JVDecoder *decoder);

void jv_media_close(JVMedia *media);
#endif
