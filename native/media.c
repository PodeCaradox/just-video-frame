#include "media.h"
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/error.h>
#include <libavutil/fifo.h>
#include <libavutil/hwcontext.h>
#include <libavutil/pixdesc.h>
#include <libavutil/spherical.h>
#include <libavutil/stereo3d.h>
#include <libavutil/time.h>
#include <libavutil/channel_layout.h>
#include <libswresample/swresample.h>

#define IO_BUFFER_SIZE (256 * 1024)

#define MAX_SUBTITLES 32
#define CUE_QUEUE 64

struct JVMedia {
    AVFormatContext *format;
    AVIOContext *io;
    int video_stream;
    int audio_stream;
    int subtitle_streams[MAX_SUBTITLES];
    int subtitle_count;
    int audio_streams[MAX_SUBTITLES];
    int audio_count;
};

typedef struct {
    enum AVPixelFormat hw_format;
    int allow_software;
    // Hardware decoder that returns ordinary frames (V4L2 mem2mem wrappers).
    int hardware_wrapper;
} FormatChoice;

static void set_error(char *out, int size, const char *operation, int code) {
    char message[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, message, sizeof(message));
    snprintf(out, size, "%s: %s", operation, message);
}

static void copy_name(char *out, size_t size, const char *value) {
    snprintf(out, size, "%s", value ? value : "");
}

typedef struct {
    jv_read_fn read;
    jv_seek_fn seek;
    void *opaque;
} Callbacks;

static int io_read(void *opaque, uint8_t *buf, int size) {
    const Callbacks *cb = opaque;
    int n = cb->read(cb->opaque, buf, size);
    return n == 0 ? AVERROR_EOF : n;
}

static int64_t io_seek(void *opaque, int64_t offset, int whence) {
    const Callbacks *cb = opaque;
    return cb->seek(cb->opaque, offset, whence & ~AVSEEK_FORCE);
}

static void describe_vr(const AVCodecParameters *par, JVMediaInfo *info) {
    const AVPacketSideData *sd = av_packet_side_data_get(
        par->coded_side_data, par->nb_coded_side_data, AV_PKT_DATA_STEREO3D);
    if (sd) {
        const AVStereo3D *stereo = (const AVStereo3D *)sd->data;
        copy_name(info->stereo_mode, sizeof(info->stereo_mode), av_stereo3d_type_name(stereo->type));
        info->stereo_inverted = !!(stereo->flags & AV_STEREO3D_FLAG_INVERT);
    }
    sd = av_packet_side_data_get(par->coded_side_data, par->nb_coded_side_data, AV_PKT_DATA_SPHERICAL);
    if (sd) {
        const AVSphericalMapping *map = (const AVSphericalMapping *)sd->data;
        copy_name(info->projection, sizeof(info->projection), av_spherical_projection_name(map->projection));
        info->bound_left = map->bound_left;
        info->bound_top = map->bound_top;
        info->bound_right = map->bound_right;
        info->bound_bottom = map->bound_bottom;
    }
}

JVMedia *jv_media_open(const char *name, jv_read_fn read, jv_seek_fn seek, void *opaque,
                       JVMediaInfo *info, char *error, int error_size) {
    memset(info, 0, sizeof(*info));
    // JUST_VIDEO_FFMPEG_LOG=debug (or verbose, info…) shows more of FFmpeg's log.
    const char *level = getenv("JUST_VIDEO_FFMPEG_LOG");
    av_log_set_level(level && !strcmp(level, "debug")     ? AV_LOG_DEBUG
                     : level && !strcmp(level, "verbose") ? AV_LOG_VERBOSE
                     : level && !strcmp(level, "info")    ? AV_LOG_INFO
                                                          : AV_LOG_ERROR);
    JVMedia *media = av_mallocz(sizeof(*media));
    Callbacks *cb = av_malloc(sizeof(*cb));
    uint8_t *buffer = av_malloc(IO_BUFFER_SIZE);
    int ret = AVERROR(ENOMEM);
    if (!media || !cb || !buffer) goto fail;
    *cb = (Callbacks){ read, seek, opaque };
    media->io = avio_alloc_context(buffer, IO_BUFFER_SIZE, 0, cb, io_read, NULL, io_seek);
    if (!media->io) goto fail;
    buffer = NULL;
    cb = NULL;
    media->format = avformat_alloc_context();
    if (!media->format) goto fail;
    media->format->pb = media->io;
    media->format->flags |= AVFMT_FLAG_CUSTOM_IO;
    AVDictionary *options = NULL;
    // Nested references (playlists, external tracks) must not reach FFmpeg's own protocols.
    av_dict_set(&options, "protocol_whitelist", "", 0);
    ret = avformat_open_input(&media->format, name, NULL, &options);
    av_dict_free(&options);
    if (ret < 0) { set_error(error, error_size, "Open media", ret); goto fail; }
    ret = avformat_find_stream_info(media->format, NULL);
    if (ret < 0) { set_error(error, error_size, "Read stream information", ret); goto fail; }

    AVFormatContext *fmt = media->format;
    copy_name(info->container, sizeof(info->container), fmt->iformat->name);
    info->duration_seconds = fmt->duration > 0 ? fmt->duration / (double)AV_TIME_BASE : 0;
    info->bit_rate = fmt->bit_rate;
    media->video_stream = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (media->video_stream >= 0) {
        AVStream *video = fmt->streams[media->video_stream];
        const AVCodecParameters *par = video->codecpar;
        copy_name(info->video_codec, sizeof(info->video_codec), avcodec_get_name(par->codec_id));
        copy_name(info->video_profile, sizeof(info->video_profile), avcodec_profile_name(par->codec_id, par->profile));
        copy_name(info->pixel_format, sizeof(info->pixel_format), av_get_pix_fmt_name(par->format));
        info->width = par->width;
        info->height = par->height;
        const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(par->format);
        info->bit_depth = desc ? desc->comp[0].depth : 0;
        AVRational rate = av_guess_frame_rate(fmt, video, NULL);
        info->fps = rate.den ? av_q2d(rate) : 0;
        describe_vr(par, info);
    }
    int audio = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, media->video_stream, NULL, 0);
    media->audio_stream = audio;
    // Demux only what we play: skip extra audio tracks, attachments, and
    // subtitles until one is selected (jv_decoder_select_subtitle).
    for (unsigned i = 0; i < fmt->nb_streams; i++) {
        enum AVMediaType type = fmt->streams[i]->codecpar->codec_type;
        if (type == AVMEDIA_TYPE_SUBTITLE && media->subtitle_count < MAX_SUBTITLES)
            media->subtitle_streams[media->subtitle_count++] = (int)i;
        if (type == AVMEDIA_TYPE_AUDIO && media->audio_count < MAX_SUBTITLES)
            media->audio_streams[media->audio_count++] = (int)i;
        if ((int)i != media->video_stream && (int)i != audio) fmt->streams[i]->discard = AVDISCARD_ALL;
    }
    if (audio >= 0) {
        const AVCodecParameters *par = fmt->streams[audio]->codecpar;
        copy_name(info->audio_codec, sizeof(info->audio_codec), avcodec_get_name(par->codec_id));
        info->audio_channels = par->ch_layout.nb_channels;
        info->audio_sample_rate = par->sample_rate;
    }
    return media;
fail:
    if (!error[0]) set_error(error, error_size, "Allocate media", ret);
    av_free(buffer);
    av_free(cb);
    jv_media_close(media);
    return NULL;
}

static enum AVPixelFormat choose_format(AVCodecContext *ctx, const enum AVPixelFormat *formats) {
    const FormatChoice *choice = ctx->opaque;
    enum AVPixelFormat software = AV_PIX_FMT_NONE;
    for (const enum AVPixelFormat *p = formats; *p != AV_PIX_FMT_NONE; ++p) {
        if (*p == choice->hw_format) return *p;
        const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(*p);
        if (software == AV_PIX_FMT_NONE && desc && !(desc->flags & AV_PIX_FMT_FLAG_HWACCEL))
            software = *p;
    }
    return choice->allow_software ? software : AV_PIX_FMT_NONE;
}

// Attaches a hardware device when the backend offers one for this decoder.
static int attach_hardware(AVCodecContext *ctx, const AVCodec *codec, const char *backend,
                           FormatChoice *choice, AVBufferRef **device) {
    enum AVHWDeviceType type = av_hwdevice_find_type_by_name(backend);
    if (type == AV_HWDEVICE_TYPE_NONE) return AVERROR(ENOSYS);
    for (int i = 0; ; ++i) {
        const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
        if (!config) return AVERROR(ENOSYS);
        if (config->device_type == type && (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
            choice->hw_format = config->pix_fmt;
            break;
        }
    }
    int ret = av_hwdevice_ctx_create(device, type, NULL, NULL, 0);
    if (ret < 0) return ret;
    ctx->hw_device_ctx = av_buffer_ref(*device);
    return ctx->hw_device_ctx ? 0 : AVERROR(ENOMEM);
}

// Why the V4L2 stateful decoder (Qualcomm iris on Steam Frame) must not get
// this stream, or NULL if it may. Allow-list: only streams positively known to
// be 8-bit 4:2:0 within the advertised size. On SteamOS 0.3 / kernel 6.18,
// 10-bit HEVC crashes the iris firmware, so anything uncertain goes to software.
static const char *v4l2_unsuitable(const AVCodecParameters *par) {
    if (par->codec_id != AV_CODEC_ID_H264 && par->codec_id != AV_CODEC_ID_HEVC &&
        par->codec_id != AV_CODEC_ID_VP9)
        return "hardware decoder supports H.264/HEVC/VP9 only";
    if (par->width <= 0 || par->height <= 0 || par->width > 8192 || par->height > 8192)
        return "frame size outside the hardware decoder's 8192x8192 limit";
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(par->format);
    if (desc) {
        if (desc->comp[0].depth != 8)
            return "hardware decoder is 8-bit only (10-bit crashes its firmware)";
        if (desc->log2_chroma_w != 1 || desc->log2_chroma_h != 1 || desc->nb_components < 3)
            return "hardware decoder supports 4:2:0 chroma only";
        return NULL;
    }
    // No pixel format in the headers: trust only 8-bit 4:2:0 profiles.
    int profile = par->profile;
    int eight_bit =
        (par->codec_id == AV_CODEC_ID_H264 &&
         ((profile & 0xff) == AV_PROFILE_H264_BASELINE || profile == AV_PROFILE_H264_MAIN ||
          profile == AV_PROFILE_H264_EXTENDED || profile == AV_PROFILE_H264_HIGH)) ||
        (par->codec_id == AV_CODEC_ID_HEVC &&
         (profile == AV_PROFILE_HEVC_MAIN || profile == AV_PROFILE_HEVC_MAIN_STILL_PICTURE)) ||
        (par->codec_id == AV_CODEC_ID_VP9 && profile == AV_PROFILE_VP9_0);
    return eight_bit ? NULL : "cannot confirm the stream is 8-bit, so the hardware decoder is not used";
}

struct JVDecoder {
    JVMedia *media;
    AVCodecContext *ctx;
    AVBufferRef *device;
    AVPacket *packet;
    AVFrame *transfer;  // scratch for hwaccel -> CPU transfers
    FormatChoice choice;
    // The V4L2 decoder for this stream, kept after falling back to the CPU
    // so `jv_decoder_return_to_hardware` can try it again.
    const AVCodec *hw_codec;
    // A replaced V4L2 decoder still being freed (closing its session takes
    // ~50 ms): joined before this decoder next decodes or closes.
    pthread_t closer;
    int closing;
    int flushing;
    // Decode one picture, then drain (see `jv_decoder_first_picture`).
    int one_shot;
    // While catching up to a seek target: video packets before this time (stream
    // time base) skip non-reference frames, which nothing else needs.
    int64_t skip_until;
    // Skip every frame but keyframes instead, until a keyframe at or after
    // `skip_until` (frames after a skipped one can't be decoded until then).
    int skip_to_key;
    // Video packets read ahead of the video decoder (AVPacket *), so the sound,
    // decoded as it is read, stays ahead even when decoding the picture is
    // slower than real time (8K on the CPU). `held`: one the decoder refused for now.
    AVFifo *video_packets;
    AVPacket *held;
    int end_of_file;
    // No picture decoded since the last jump: read only what it needs.
    int awaiting_picture;
    // Audio (optional): decoded and resampled to interleaved float.
    AVCodecContext *audio;
    SwrContext *swr;
    AVFrame *audio_frame;
    int out_rate, out_channels;
    float *samples;      // interleaved, `sample_count` frames buffered
    int sample_count, sample_capacity;
    double samples_pts;  // time of samples[0], seconds from video start; < 0 unknown
    // Subtitles (optional): the selected stream's cues, queued for Rust.
    AVCodecContext *subtitle;
    int subtitle_stream;  // stream index, -1 for none
    JVSubtitleCue cues[CUE_QUEUE];
    int cue_first, cue_count;
};

static double video_start_seconds(const JVMedia *media) {
    const AVStream *video = media->format->streams[media->video_stream];
    return video->start_time != AV_NOPTS_VALUE ? video->start_time * av_q2d(video->time_base) : 0;
}

int jv_media_audio_count(const JVMedia *media) {
    return media->audio_count;
}

int jv_media_audio_track(const JVMedia *media, int track, JVAudioTrack *out) {
    memset(out, 0, sizeof(*out));
    if (track < 0 || track >= media->audio_count) return AVERROR(EINVAL);
    const AVStream *st = media->format->streams[media->audio_streams[track]];
    copy_name(out->codec, sizeof(out->codec), avcodec_get_name(st->codecpar->codec_id));
    const AVDictionaryEntry *lang = av_dict_get(st->metadata, "language", NULL, 0);
    const AVDictionaryEntry *title = av_dict_get(st->metadata, "title", NULL, 0);
    copy_name(out->language, sizeof(out->language), lang ? lang->value : NULL);
    copy_name(out->title, sizeof(out->title), title ? title->value : NULL);
    out->channels = st->codecpar->ch_layout.nb_channels;
    out->is_default = !!(st->disposition & AV_DISPOSITION_DEFAULT);
    return 0;
}

int jv_media_current_audio(const JVMedia *media) {
    for (int i = 0; i < media->audio_count; i++)
        if (media->audio_streams[i] == media->audio_stream) return i;
    return -1;
}

int jv_decoder_select_audio(JVDecoder *d, int track) {
    JVMedia *m = d->media;
    if (!d->audio) return AVERROR(EINVAL);  // audio was never enabled
    if (track < 0 || track >= m->audio_count) return AVERROR(EINVAL);
    int index = m->audio_streams[track];
    if (index == m->audio_stream) return 0;
    int rate = d->out_rate, channels = d->out_channels, old = m->audio_stream;
    avcodec_free_context(&d->audio);
    swr_free(&d->swr);
    av_frame_free(&d->audio_frame);
    d->sample_count = 0;
    m->format->streams[old]->discard = AVDISCARD_ALL;
    m->format->streams[index]->discard = AVDISCARD_DEFAULT;
    m->audio_stream = index;
    int ret = jv_decoder_enable_audio(d, rate, channels);
    if (ret < 0) {  // can't decode it: back to the old track
        avcodec_free_context(&d->audio);
        swr_free(&d->swr);
        av_frame_free(&d->audio_frame);
        m->format->streams[index]->discard = AVDISCARD_ALL;
        m->format->streams[old]->discard = AVDISCARD_DEFAULT;
        m->audio_stream = old;
        jv_decoder_enable_audio(d, rate, channels);
    }
    return ret;
}

int jv_media_subtitle_count(const JVMedia *media) {
    return media->subtitle_count;
}

int jv_media_subtitle_track(const JVMedia *media, int track, JVSubtitleTrack *out) {
    memset(out, 0, sizeof(*out));
    if (track < 0 || track >= media->subtitle_count) return AVERROR(EINVAL);
    const AVStream *st = media->format->streams[media->subtitle_streams[track]];
    const AVCodecDescriptor *desc = avcodec_descriptor_get(st->codecpar->codec_id);
    copy_name(out->codec, sizeof(out->codec), avcodec_get_name(st->codecpar->codec_id));
    const AVDictionaryEntry *lang = av_dict_get(st->metadata, "language", NULL, 0);
    const AVDictionaryEntry *title = av_dict_get(st->metadata, "title", NULL, 0);
    copy_name(out->language, sizeof(out->language), lang ? lang->value : NULL);
    copy_name(out->title, sizeof(out->title), title ? title->value : NULL);
    out->is_default = !!(st->disposition & AV_DISPOSITION_DEFAULT);
    out->forced = !!(st->disposition & AV_DISPOSITION_FORCED);
    out->supported = desc && (desc->props & (AV_CODEC_PROP_TEXT_SUB | AV_CODEC_PROP_BITMAP_SUB))
                     && avcodec_find_decoder(st->codecpar->codec_id);
    return 0;
}

static void drop_cues(JVDecoder *d);

int jv_decoder_select_subtitle(JVDecoder *d, int track) {
    JVMedia *m = d->media;
    if (d->subtitle_stream >= 0) m->format->streams[d->subtitle_stream]->discard = AVDISCARD_ALL;
    avcodec_free_context(&d->subtitle);
    d->subtitle_stream = -1;
    drop_cues(d);
    if (track < 0) return 0;
    if (track >= m->subtitle_count) return AVERROR(EINVAL);
    int index = m->subtitle_streams[track];
    AVStream *st = m->format->streams[index];
    const AVCodec *codec = avcodec_find_decoder(st->codecpar->codec_id);
    if (!codec) return AVERROR_DECODER_NOT_FOUND;
    d->subtitle = avcodec_alloc_context3(codec);
    if (!d->subtitle) return AVERROR(ENOMEM);
    int ret = avcodec_parameters_to_context(d->subtitle, st->codecpar);
    if (ret >= 0) {
        d->subtitle->pkt_timebase = st->time_base;
        ret = avcodec_open2(d->subtitle, codec, NULL);
    }
    if (ret < 0) { avcodec_free_context(&d->subtitle); return ret; }
    st->discard = AVDISCARD_DEFAULT;
    d->subtitle_stream = index;
    return 0;
}

// Text of an ASS "Dialogue" payload: the part after its 8 leading fields
// (ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect).
static const char *ass_dialogue_text(const char *ass) {
    const char *p = ass;
    int commas = 0;
    for (; *p && commas < 8; p++)
        if (*p == ',') commas++;
    return commas == 8 ? p : ass;
}

void jv_free(void *pointer) {
    av_free(pointer);
}

static void drop_cues(JVDecoder *d) {
    for (int i = 0; i < d->cue_count; i++) av_freep(&d->cues[(d->cue_first + i) % CUE_QUEUE].rgba);
    d->cue_first = d->cue_count = 0;
}

static void queue_cue(JVDecoder *d, const JVSubtitleCue *cue) {
    if (d->cue_count == CUE_QUEUE) {  // nobody reading: drop the oldest
        av_freep(&d->cues[d->cue_first].rgba);
        d->cue_first = (d->cue_first + 1) % CUE_QUEUE;
        d->cue_count--;
    }
    d->cues[(d->cue_first + d->cue_count++) % CUE_QUEUE] = *cue;
}

// All bitmap rectangles of a subtitle, merged into one premultiplied RGBA image.
static void bitmap_cue(JVDecoder *d, const AVSubtitle *sub, JVSubtitleCue *cue) {
    int x0 = INT32_MAX, y0 = INT32_MAX, x1 = 0, y1 = 0;
    for (unsigned i = 0; i < sub->num_rects; i++) {
        const AVSubtitleRect *r = sub->rects[i];
        if (r->type != SUBTITLE_BITMAP || r->w <= 0 || r->h <= 0 || !r->data[0] || !r->data[1]) continue;
        x0 = FFMIN(x0, r->x); y0 = FFMIN(y0, r->y);
        x1 = FFMAX(x1, r->x + r->w); y1 = FFMAX(y1, r->y + r->h);
    }
    if (x1 <= x0 || y1 <= y0 || (int64_t)(x1 - x0) * (y1 - y0) > 4096 * 4096) return;
    int w = x1 - x0, h = y1 - y0;
    uint8_t *rgba = av_mallocz((size_t)w * h * 4);
    if (!rgba) return;
    for (unsigned i = 0; i < sub->num_rects; i++) {
        const AVSubtitleRect *r = sub->rects[i];
        if (r->type != SUBTITLE_BITMAP || r->w <= 0 || r->h <= 0 || !r->data[0] || !r->data[1]) continue;
        const uint32_t *palette = (const uint32_t *)r->data[1];  // 0xAARRGGBB
        for (int y = 0; y < r->h; y++) {
            const uint8_t *src = r->data[0] + (size_t)y * r->linesize[0];
            uint8_t *dst = rgba + ((size_t)(r->y - y0 + y) * w + (r->x - x0)) * 4;
            for (int x = 0; x < r->w; x++, dst += 4) {
                uint32_t c = palette[src[x]];
                unsigned a = c >> 24;
                if (!a) continue;
                dst[0] = ((c >> 16) & 0xff) * a / 255;
                dst[1] = ((c >> 8) & 0xff) * a / 255;
                dst[2] = (c & 0xff) * a / 255;
                dst[3] = a;
            }
        }
    }
    const AVCodecParameters *video = d->media->format->streams[d->media->video_stream]->codecpar;
    cue->rgba = rgba;
    cue->x = x0; cue->y = y0; cue->width = w; cue->height = h;
    // Bitmap coordinates are in the subtitle's own frame (e.g. 720×576 for DVD).
    cue->frame_width = d->subtitle->width > 0 ? d->subtitle->width : video->width;
    cue->frame_height = d->subtitle->height > 0 ? d->subtitle->height : video->height;
}

static void decode_subtitle_packet(JVDecoder *d, AVPacket *packet) {
    AVSubtitle sub;
    int got = 0;
    if (avcodec_decode_subtitle2(d->subtitle, &sub, &got, packet) < 0 || !got) return;
    AVStream *st = d->media->format->streams[d->subtitle_stream];
    int64_t pts = sub.pts != AV_NOPTS_VALUE ? av_rescale_q(sub.pts, AV_TIME_BASE_Q, st->time_base) : packet->pts;
    if (pts != AV_NOPTS_VALUE) {
        double start = pts * av_q2d(st->time_base) - video_start_seconds(d->media)
                       + sub.start_display_time / 1000.0;
        // Without a duration (Blu-ray), a cue lasts until the next erase event.
        double length = packet->duration > 0 ? packet->duration * av_q2d(st->time_base)
                        : sub.end_display_time > sub.start_display_time && sub.end_display_time != UINT32_MAX
                            ? (sub.end_display_time - sub.start_display_time) / 1000.0
                            : 10.0;
        JVSubtitleCue cue = { .start = start, .end = start + length };
        size_t used = 0;
        for (unsigned i = 0; i < sub.num_rects; i++) {
            const AVSubtitleRect *r = sub.rects[i];
            if (r->type == SUBTITLE_BITMAP) continue;
            const char *text = r->ass ? ass_dialogue_text(r->ass) : r->text;
            if (!text || !*text) continue;
            used += snprintf(cue.text + used, sizeof(cue.text) - used, "%s%s", used ? "\\N" : "", text);
            if (used >= sizeof(cue.text)) break;
        }
        if (!cue.text[0]) bitmap_cue(d, &sub, &cue);
        if (cue.text[0] || cue.rgba) {
            queue_cue(d, &cue);
        } else if (sub.num_rects == 0) {
            cue.clear = 1;
            queue_cue(d, &cue);
        }
    }
    avsubtitle_free(&sub);
}

int jv_decoder_subtitle_read(JVDecoder *d, JVSubtitleCue *out) {
    if (d->cue_count == 0) return 0;
    *out = d->cues[d->cue_first];  // the caller now owns `rgba`
    d->cues[d->cue_first].rgba = NULL;
    d->cue_first = (d->cue_first + 1) % CUE_QUEUE;
    d->cue_count--;
    return 1;
}

static void drop_video_packets(JVDecoder *d) {
    AVPacket *p;
    while (d->video_packets && av_fifo_read(d->video_packets, &p, 1) >= 0) av_packet_free(&p);
    av_packet_free(&d->held);
    d->end_of_file = 0;
    d->awaiting_picture = 1;
}

static void *free_context(void *ctx) {
    avcodec_free_context((AVCodecContext **)&ctx);
    return NULL;
}

static void finish_closing(JVDecoder *d) {
    if (!d->closing) return;
    pthread_join(d->closer, NULL);
    d->closing = 0;
}

void jv_decoder_close(JVDecoder *d) {
    if (!d) return;
    finish_closing(d);
    drop_cues(d);
    avcodec_free_context(&d->subtitle);
    avcodec_free_context(&d->audio);
    swr_free(&d->swr);
    av_frame_free(&d->audio_frame);
    av_free(d->samples);
    avcodec_free_context(&d->ctx);
    av_buffer_unref(&d->device);
    av_packet_free(&d->packet);
    drop_video_packets(d);
    av_fifo_freep2(&d->video_packets);
    av_frame_free(&d->transfer);
    av_free(d);
}

// Capture buffers for the V4L2 decoder: those held by the player (queue,
// picture on screen and the next one) and the decoder's reference pictures
// all come out of them, so a small picture gets more. Too few and the driver
// has nothing to decode into. 8K takes all the memory it can have (see the
// notes on FFmpeg's default of 20 failing there).
static int capture_buffer_count(int64_t pixels) {
    return pixels > 4096 * 2304 ? 6 : pixels > 1920 * 1088 ? 12 : 20;
}

JVDecoder *jv_decoder_open(JVMedia *media, const char *hw_backend, int allow_software,
                           const char *decoder_options, JVDecodeStats *s) {
    memset(s, 0, sizeof(*s));
    JVDecoder *d = av_mallocz(sizeof(*d));
    AVDictionary *open_options = NULL;
    int ret = AVERROR(ENOMEM);
    if (!d) goto fail;
    d->media = media;
    d->subtitle_stream = -1;
    d->skip_until = AV_NOPTS_VALUE;
    d->packet = av_packet_alloc();
    d->video_packets = av_fifo_alloc2(64, sizeof(AVPacket *), AV_FIFO_FLAG_AUTO_GROW);
    d->transfer = av_frame_alloc();
    if (!d->packet || !d->transfer) goto fail;
    d->choice = (FormatChoice){ AV_PIX_FMT_NONE, allow_software, 0 };
    if (media->video_stream < 0) { ret = AVERROR_STREAM_NOT_FOUND; set_error(s->error, sizeof(s->error), "Find video stream", ret); goto fail; }
    AVStream *video = media->format->streams[media->video_stream];
    enum AVCodecID id = video->codecpar->codec_id;
    // Hardware decoding needs FFmpeg's native decoders (e.g. "av1", not libdav1d);
    // software decoding takes FFmpeg's preferred (fastest) implementation.
    const AVCodec *codec = NULL;
    if (hw_backend) codec = avcodec_find_decoder_by_name(avcodec_get_name(id));
    if (!codec) codec = avcodec_find_decoder(id);
    if (!codec) { ret = AVERROR_DECODER_NOT_FOUND; set_error(s->error, sizeof(s->error), "Find decoder", ret); goto fail; }
    if (hw_backend && !strcmp(hw_backend, "v4l2m2m")) {
        const char *why = v4l2_unsuitable(video->codecpar);
        const AVCodec *wrapper = NULL;
        if (!why) {
            char name[48];
            snprintf(name, sizeof(name), "%s_v4l2m2m", avcodec_get_name(id));
            wrapper = avcodec_find_decoder_by_name(name);
            if (!wrapper) why = "FFmpeg build lacks the V4L2 decoder for this codec";
        }
        hw_backend = NULL;  // no hwdevice context: the wrapper talks to /dev/video* itself
        if (wrapper) {
            codec = wrapper;
            d->hw_codec = wrapper;
            d->choice.hardware_wrapper = 1;
            copy_name(s->hw_backend, sizeof(s->hw_backend), "v4l2m2m");
            // FFmpeg's default of 20 capture buffers fails to allocate at 8K.
            int64_t pixels = (int64_t)video->codecpar->width * video->codecpar->height;
            av_dict_set_int(&open_options, "num_capture_buffers", capture_buffer_count(pixels), 0);
            av_dict_set_int(&open_options, "num_output_buffers", 16, 0);
        } else if (!allow_software) {
            ret = AVERROR(ENOSYS);
            snprintf(s->error, sizeof(s->error), "%s", why);
            goto fail;
        } else {
            copy_name(s->note, sizeof(s->note), why);
            codec = avcodec_find_decoder(id);
        }
    }
    int hw_retries = 0;
reopen:
    for (int attempt = 0; attempt < 2; ++attempt) {
        avcodec_free_context(&d->ctx);
        av_buffer_unref(&d->device);
        d->choice.hw_format = AV_PIX_FMT_NONE;
        d->ctx = avcodec_alloc_context3(codec);
        if (!d->ctx) { ret = AVERROR(ENOMEM); goto fail; }
        ret = avcodec_parameters_to_context(d->ctx, video->codecpar);
        if (ret < 0) { set_error(s->error, sizeof(s->error), "Copy codec parameters", ret); goto fail; }
        d->ctx->pkt_timebase = video->time_base;
        d->ctx->opaque = &d->choice;
        d->ctx->get_format = choose_format;
        d->ctx->thread_count = 0;
        if (attempt == 0 && hw_backend) {
            ret = attach_hardware(d->ctx, codec, hw_backend, &d->choice, &d->device);
            if (ret < 0) {
                if (!allow_software) { set_error(s->error, sizeof(s->error), "Create hardware decoder", ret); goto fail; }
                set_error(s->note, sizeof(s->note), "Hardware decoder unavailable", ret);
                codec = avcodec_find_decoder(id);  // prefer e.g. libdav1d for software AV1
                continue;
            }
            copy_name(s->hw_backend, sizeof(s->hw_backend), hw_backend);
        }
        break;
    }
    copy_name(s->decoder, sizeof(s->decoder), codec->name);
    if (decoder_options && *decoder_options) {
        ret = av_dict_parse_string(&open_options, decoder_options, "=", ":", 0);
        if (ret < 0) { set_error(s->error, sizeof(s->error), "Parse decoder options", ret); goto fail; }
    }
    ret = avcodec_open2(d->ctx, codec, &open_options);
    if (ret >= 0 && av_dict_count(open_options)) {
        const AVDictionaryEntry *unused = av_dict_iterate(open_options, NULL);
        snprintf(s->error, sizeof(s->error), "Unknown decoder option: %s", unused->key);
        ret = AVERROR_OPTION_NOT_FOUND;
        goto fail;
    }
    if (ret < 0 && d->choice.hardware_wrapper && hw_retries < 2) {
        // The driver sometimes can't get buffers right away ("REQBUFS: Cannot
        // allocate memory", seen after many decoder sessions): try again
        // shortly before settling for the CPU, which can't keep up with 8K.
        ++hw_retries;
        av_log(NULL, AV_LOG_ERROR, "Hardware decoder failed to open (%s); trying again\n", av_err2str(ret));
        av_usleep(250000 * hw_retries);
        av_dict_free(&open_options);
        int64_t pixels = (int64_t)video->codecpar->width * video->codecpar->height;
        av_dict_set_int(&open_options, "num_capture_buffers", capture_buffer_count(pixels), 0);
        av_dict_set_int(&open_options, "num_output_buffers", 16, 0);
        goto reopen;
    }
    if (ret < 0 && d->choice.hardware_wrapper && allow_software) {
        // Device missing, busy or its firmware recovering: never fail playback
        // over it when the CPU can decode instead.
        set_error(s->note, sizeof(s->note), "Hardware decoder failed to open", ret);
        d->choice.hardware_wrapper = 0;
        s->hw_backend[0] = '\0';
        av_dict_free(&open_options);
        codec = avcodec_find_decoder(id);
        goto reopen;
    }
    if (ret < 0) { set_error(s->error, sizeof(s->error), "Open decoder", ret); goto fail; }
    av_dict_free(&open_options);
    return d;
fail:
    if (!s->error[0]) set_error(s->error, sizeof(s->error), "Open decoder", ret);
    av_dict_free(&open_options);
    jv_decoder_close(d);
    return NULL;
}

// Replaces a failed V4L2 video decoder with a new one (the file, audio and
// subtitle state stay); falls back to the CPU if the device won't open, or
// goes straight there without `try_hardware`.
// Returns 1 when the new decoder is the hardware one, 0 for software, < 0 on failure.
int jv_decoder_reopen_video(JVDecoder *d, int try_hardware, int free_behind) {
    if (!d->choice.hardware_wrapper) return AVERROR(ENOSYS);
    AVStream *video = d->media->format->streams[d->media->video_stream];
    const AVCodec *codec = d->ctx->codec;
    finish_closing(d);
    // The old instance goes first, as the driver counts every open session's
    // load when one starts; with `free_behind`, while the new one opens, and
    // before it starts (`jv_decoder_next`).
    if (free_behind && pthread_create(&d->closer, NULL, free_context, d->ctx) == 0) {
        d->closing = 1;
        d->ctx = NULL;
    } else {
        avcodec_free_context(&d->ctx);
    }
    for (int hardware = !!try_hardware; hardware >= 0; --hardware) {
        if (!hardware) codec = avcodec_find_decoder(video->codecpar->codec_id);
        if (!codec) return AVERROR_DECODER_NOT_FOUND;
        d->choice = (FormatChoice){ AV_PIX_FMT_NONE, 1, hardware };
        d->ctx = avcodec_alloc_context3(codec);
        if (!d->ctx) return AVERROR(ENOMEM);
        int ret = avcodec_parameters_to_context(d->ctx, video->codecpar);
        if (ret < 0) return ret;
        d->ctx->pkt_timebase = video->time_base;
        d->ctx->opaque = &d->choice;
        d->ctx->get_format = choose_format;
        d->ctx->thread_count = 0;
        AVDictionary *options = NULL;
        if (hardware) {
            int64_t pixels = (int64_t)video->codecpar->width * video->codecpar->height;
            av_dict_set_int(&options, "num_capture_buffers", capture_buffer_count(pixels), 0);
            av_dict_set_int(&options, "num_output_buffers", 16, 0);
        }
        ret = avcodec_open2(d->ctx, codec, &options);
        av_dict_free(&options);
        if (ret >= 0) {
            d->skip_until = AV_NOPTS_VALUE;
            d->flushing = 0;
            return hardware;
        }
        avcodec_free_context(&d->ctx);
        // Perhaps short of memory while the old one closes: wait, try again.
        if (hardware && d->closing) {
            finish_closing(d);
            ++hardware;
            continue;
        }
        if (!hardware) return ret;
    }
    return AVERROR_BUG;
}

// Moves a decoder that fell back to the CPU back to the V4L2 device, once it
// opens again (its firmware restarted). The CPU decoder is only freed when the
// new one opened, so a failed try costs nothing else. Seek afterwards.
// Returns 0 on success, < 0 when the device still won't open.
int jv_decoder_return_to_hardware(JVDecoder *d) {
    if (!d->hw_codec || d->choice.hardware_wrapper) return AVERROR(ENOSYS);
    finish_closing(d);
    AVStream *video = d->media->format->streams[d->media->video_stream];
    FormatChoice software = d->choice;
    AVCodecContext *ctx = avcodec_alloc_context3(d->hw_codec);
    if (!ctx) return AVERROR(ENOMEM);
    int ret = avcodec_parameters_to_context(ctx, video->codecpar);
    if (ret < 0) { avcodec_free_context(&ctx); return ret; }
    ctx->pkt_timebase = video->time_base;
    ctx->opaque = &d->choice;
    ctx->get_format = choose_format;
    ctx->thread_count = 0;
    d->choice = (FormatChoice){ AV_PIX_FMT_NONE, 1, 1 };
    AVDictionary *options = NULL;
    int64_t pixels = (int64_t)video->codecpar->width * video->codecpar->height;
    av_dict_set_int(&options, "num_capture_buffers", capture_buffer_count(pixels), 0);
    av_dict_set_int(&options, "num_output_buffers", 16, 0);
    ret = avcodec_open2(ctx, d->hw_codec, &options);
    av_dict_free(&options);
    if (ret < 0) {
        avcodec_free_context(&ctx);
        d->choice = software;
        return ret;
    }
    avcodec_free_context(&d->ctx);
    d->ctx = ctx;
    d->skip_until = AV_NOPTS_VALUE;
    d->skip_to_key = 0;
    d->flushing = 0;
    return 0;
}

static int describe_frame(JVDecoder *d, AVFrame *frame, JVFrame *out) {
    AVStream *video = d->media->format->streams[d->media->video_stream];
    memset(out, 0, sizeof(*out));
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(frame->format);
    out->hardware = d->choice.hardware_wrapper ||
                    (desc && (desc->flags & AV_PIX_FMT_FLAG_HWACCEL) && frame->hw_frames_ctx);
    if (desc && (desc->flags & AV_PIX_FMT_FLAG_HWACCEL)) {
        // GPU surface (desktop Vulkan video): bring it to CPU memory for upload.
        av_frame_unref(d->transfer);
        int ret = av_hwframe_transfer_data(d->transfer, frame, 0);
        if (ret < 0) return ret;
        ret = av_frame_copy_props(d->transfer, frame);
        if (ret < 0) return ret;
        av_frame_unref(frame);
        av_frame_move_ref(frame, d->transfer);
    }
    switch (frame->format) {
    case AV_PIX_FMT_YUV420P:
    case AV_PIX_FMT_YUVJ420P:     out->layout = JV_LAYOUT_PLANAR; out->bits = 8; break;
    case AV_PIX_FMT_YUV420P10LE:  out->layout = JV_LAYOUT_PLANAR; out->bits = 10; break;
    case AV_PIX_FMT_NV12:         out->layout = JV_LAYOUT_SEMIPLANAR; out->bits = 8; break;
    case AV_PIX_FMT_P010LE:       out->layout = JV_LAYOUT_SEMIPLANAR_MSB; out->bits = 10; break;
    default: return AVERROR_PATCHWELCOME;
    }
    out->handle = frame;
    out->width = frame->width;
    out->height = frame->height;
    out->plane_count = out->layout == JV_LAYOUT_PLANAR ? 3 : 2;
    for (int i = 0; i < out->plane_count; i++) {
        out->data[i] = frame->data[i];
        out->linesize[i] = frame->linesize[i];
    }
    int64_t ts = frame->best_effort_timestamp;
    int64_t start = video->start_time != AV_NOPTS_VALUE ? video->start_time : 0;
    out->pts = ts == AV_NOPTS_VALUE ? -1 : (ts - start) * av_q2d(video->time_base);
    switch (frame->colorspace) {
    case AVCOL_SPC_BT709:      out->matrix = JV_MATRIX_BT709; break;
    case AVCOL_SPC_BT470BG:
    case AVCOL_SPC_SMPTE170M:  out->matrix = JV_MATRIX_BT601; break;
    case AVCOL_SPC_BT2020_NCL:
    case AVCOL_SPC_BT2020_CL:  out->matrix = JV_MATRIX_BT2020; break;
    default: out->matrix = frame->height >= 720 ? JV_MATRIX_BT709 : JV_MATRIX_BT601; break;
    }
    out->full_range = frame->color_range == AVCOL_RANGE_JPEG || frame->format == AV_PIX_FMT_YUVJ420P;
    out->transfer = frame->color_trc == AVCOL_TRC_SMPTE2084 ? JV_TRANSFER_PQ
                  : frame->color_trc == AVCOL_TRC_ARIB_STD_B67 ? JV_TRANSFER_HLG
                  : JV_TRANSFER_SDR;
    return 0;
}

int jv_decoder_enable_audio(JVDecoder *d, int rate, int channels) {
    JVMedia *m = d->media;
    if (m->audio_stream < 0) return AVERROR_STREAM_NOT_FOUND;
    const AVCodecParameters *par = m->format->streams[m->audio_stream]->codecpar;
    const AVCodec *codec = avcodec_find_decoder(par->codec_id);
    if (!codec) return AVERROR_DECODER_NOT_FOUND;
    d->audio = avcodec_alloc_context3(codec);
    d->audio_frame = av_frame_alloc();
    if (!d->audio || !d->audio_frame) return AVERROR(ENOMEM);
    int ret = avcodec_parameters_to_context(d->audio, par);
    if (ret < 0) return ret;
    d->audio->pkt_timebase = m->format->streams[m->audio_stream]->time_base;
    ret = avcodec_open2(d->audio, codec, NULL);
    if (ret < 0) return ret;
    AVChannelLayout out_layout;
    av_channel_layout_default(&out_layout, channels);
    ret = swr_alloc_set_opts2(&d->swr, &out_layout, AV_SAMPLE_FMT_FLT, rate,
                              &d->audio->ch_layout, d->audio->sample_fmt, d->audio->sample_rate, 0, NULL);
    av_channel_layout_uninit(&out_layout);
    if (ret < 0) return ret;
    ret = swr_init(d->swr);
    if (ret < 0) return ret;
    d->out_rate = rate;
    d->out_channels = channels;
    d->samples_pts = -1;
    return 0;
}

static void decode_audio_packet(JVDecoder *d, const AVPacket *packet) {
    if (avcodec_send_packet(d->audio, packet) < 0) return;  // damaged audio: skip it
    AVStream *stream = d->media->format->streams[d->media->audio_stream];
    while (avcodec_receive_frame(d->audio, d->audio_frame) == 0) {
        AVFrame *f = d->audio_frame;
        int max_out = swr_get_out_samples(d->swr, f->nb_samples);
        if (max_out > 0 && d->sample_count + max_out > d->sample_capacity) {
            int capacity = (d->sample_count + max_out) * 2;
            float *grown = av_realloc_array(d->samples, (size_t)capacity * d->out_channels, sizeof(float));
            if (!grown) { av_frame_unref(f); return; }
            d->samples = grown;
            d->sample_capacity = capacity;
        }
        if (d->sample_count == 0 && f->best_effort_timestamp != AV_NOPTS_VALUE)
            d->samples_pts = f->best_effort_timestamp * av_q2d(stream->time_base) - video_start_seconds(d->media)
                             - (double)swr_get_delay(d->swr, d->out_rate) / d->out_rate;
        uint8_t *dst = (uint8_t *)(d->samples + (size_t)d->sample_count * d->out_channels);
        int n = swr_convert(d->swr, &dst, max_out, (const uint8_t **)f->extended_data, f->nb_samples);
        if (n > 0) d->sample_count += n;
        av_frame_unref(f);
    }
}

int jv_decoder_audio_available(const JVDecoder *d) {
    return d->audio ? d->sample_count : 0;
}

int jv_decoder_audio_read(JVDecoder *d, float *out, int frames, double *pts) {
    if (!d->audio) return 0;
    int n = frames < d->sample_count ? frames : d->sample_count;
    if (n <= 0) return 0;
    *pts = d->samples_pts;
    memcpy(out, d->samples, (size_t)n * d->out_channels * sizeof(float));
    memmove(d->samples, d->samples + (size_t)n * d->out_channels,
            (size_t)(d->sample_count - n) * d->out_channels * sizeof(float));
    d->sample_count -= n;
    if (d->samples_pts >= 0) d->samples_pts += (double)n / d->out_rate;
    return n;
}

// Seconds of video packets read ahead of the video decoder (see `video_packets`;
// 8K: ~15 MB). Software decoding catches up from 1 s behind (see the player),
// and a cap in packets for streams without timestamps.
#define VIDEO_READ_AHEAD 1.0
#define VIDEO_READ_AHEAD_PACKETS 240

// Seconds between the first and last queued video packets (decode order).
static double queued_video_seconds(const JVDecoder *d) {
    size_t n = av_fifo_can_read(d->video_packets);
    if (n < 2) return 0;
    AVPacket *first, *last;
    av_fifo_peek(d->video_packets, &first, 1, 0);
    av_fifo_peek(d->video_packets, &last, 1, n - 1);
    int64_t a = first->dts != AV_NOPTS_VALUE ? first->dts : first->pts;
    int64_t b = last->dts != AV_NOPTS_VALUE ? last->dts : last->pts;
    if (a == AV_NOPTS_VALUE || b == AV_NOPTS_VALUE) return 0;
    return (b - a) * av_q2d(d->media->format->streams[d->media->video_stream]->time_base);
}

// Reads packets until a video packet is queued, decoding sound and subtitles
// as they come. With sound, the queue then grows by up to three packets per
// call (four read, one decoded) until it holds VIDEO_READ_AHEAD: filling it at once
// would delay the first picture after every jump by a second of reading.
static int read_ahead(JVDecoder *d) {
    // Right after a jump, the picture comes first: one packet at a time.
    size_t extra = 0, room = d->awaiting_picture ? 1 : 4;
    for (;;) {
        size_t queued = av_fifo_can_read(d->video_packets);
        if (d->end_of_file) return 0;
        if (queued > 0 && (!d->audio || extra >= room || queued >= VIDEO_READ_AHEAD_PACKETS ||
                           queued_video_seconds(d) >= VIDEO_READ_AHEAD))
            return 0;
        int ret = av_read_frame(d->media->format, d->packet);
        if (ret == AVERROR_EOF) { d->end_of_file = 1; return 0; }
        if (ret < 0) return queued > 0 ? 0 : ret;  // a read error waits until it matters
        int stream = d->packet->stream_index;
        if (d->audio && stream == d->media->audio_stream) {
            decode_audio_packet(d, d->packet);
        } else if (d->subtitle && stream == d->subtitle_stream) {
            decode_subtitle_packet(d, d->packet);
        } else if (stream == d->media->video_stream) {
            AVPacket *p = av_packet_alloc();
            if (!p) return AVERROR(ENOMEM);
            av_packet_move_ref(p, d->packet);
            if (av_fifo_write(d->video_packets, &p, 1) < 0) { av_packet_free(&p); return AVERROR(ENOMEM); }
            ++extra;
            continue;
        }
        av_packet_unref(d->packet);
    }
}

// How long one call to jv_decoder_next may wait for the hardware decoder
// (it returns an error then, and the player replaces the decoder).
#define HARDWARE_STALL_US 3000000

int jv_decoder_next(JVDecoder *d, JVFrame *out) {
    finish_closing(d);
    if (!d->ctx) return AVERROR(EINVAL);  // a failed reopen left no decoder
    AVFrame *frame = av_frame_alloc();
    if (!frame) return AVERROR(ENOMEM);
    // Time spent waiting in the hardware decoder for a picture (reading and
    // other work don't count): it blocks only when the driver is slow, or has
    // stopped (every buffer it can decode into held elsewhere, a lost session).
    int64_t waited = 0;
    for (;;) {
        int64_t began = av_gettime_relative();
        int ret = avcodec_receive_frame(d->ctx, frame);
        if (ret == AVERROR(EAGAIN) && (d->ctx->codec->capabilities & AV_CODEC_CAP_HARDWARE)) {
            waited += av_gettime_relative() - began;
            if (waited > HARDWARE_STALL_US) {
                av_log(NULL, AV_LOG_ERROR, "Hardware decoder returned no picture for %lld ms\n",
                       (long long)(waited / 1000));
                av_frame_free(&frame);
                return AVERROR(ETIMEDOUT);
            }
        }
        if (ret == 0) {
            d->awaiting_picture = 0;
            ret = describe_frame(d, frame, out);
            if (ret < 0) av_frame_free(&frame);
            return ret;
        }
        if (ret != AVERROR(EAGAIN)) { av_frame_free(&frame); return ret; }  // EOF or error
        ret = read_ahead(d);
        if (ret < 0) { av_frame_free(&frame); return ret; }
        AVPacket *p = d->held;
        d->held = NULL;
        if (!p && av_fifo_read(d->video_packets, &p, 1) < 0) p = NULL;
        if (!p) {
            if (d->flushing) { av_frame_free(&frame); return AVERROR_EOF; }
            d->flushing = 1;  // end of file: drain the decoder
            avcodec_send_packet(d->ctx, NULL);
            continue;
        }
        if (d->skip_until != AV_NOPTS_VALUE) {
            // Decode order: once a packet reaches the target, every later one
            // decodes normally (a packet without a time ends skipping too).
            int skip = p->pts != AV_NOPTS_VALUE && p->pts < d->skip_until;
            if (d->skip_to_key) skip = skip || !(p->flags & AV_PKT_FLAG_KEY);
            d->ctx->skip_frame = !skip ? AVDISCARD_DEFAULT
                               : d->skip_to_key ? AVDISCARD_NONKEY : AVDISCARD_NONREF;
            if (!skip) { d->skip_until = AV_NOPTS_VALUE; d->skip_to_key = 0; }
        }
        ret = avcodec_send_packet(d->ctx, p);
        if (ret == AVERROR(EAGAIN)) { d->held = p; continue; }  // take a frame first
        int key = p->flags & AV_PKT_FLAG_KEY;
        av_packet_free(&p);
        // A damaged packet costs a glitch, not the whole playback.
        if (ret < 0 && ret != AVERROR_INVALIDDATA) { av_frame_free(&frame); return ret; }
        if (d->one_shot && key) {
            // Frame threads and reordering hold the picture back for later
            // packets: drain instead of reading on.
            d->one_shot = 0;
            d->flushing = 1;
            avcodec_send_packet(d->ctx, NULL);
        }
    }
}

void jv_frame_release(void *handle) {
    AVFrame *frame = handle;
    av_frame_free(&frame);
}

static int64_t video_ts(const JVDecoder *d, double seconds) {
    const AVStream *video = d->media->format->streams[d->media->video_stream];
    int64_t start = video->start_time != AV_NOPTS_VALUE ? video->start_time : 0;
    // Rounded, so a keyframe time from jv_decoder_keyframe maps back to that keyframe.
    return start + llrint(seconds / av_q2d(video->time_base));
}

int jv_decoder_seek(JVDecoder *d, double seconds) {
    if (!d->ctx) return AVERROR(EINVAL);
    int ret = av_seek_frame(d->media->format, d->media->video_stream, video_ts(d, seconds), AVSEEK_FLAG_BACKWARD);
    if (ret < 0) return ret;
    avcodec_flush_buffers(d->ctx);
    d->ctx->skip_frame = AVDISCARD_DEFAULT;
    d->skip_until = AV_NOPTS_VALUE;
    d->skip_to_key = 0;
    drop_video_packets(d);
    if (d->audio) {
        avcodec_flush_buffers(d->audio);
        swr_close(d->swr);
        swr_init(d->swr);
        d->sample_count = 0;
        d->samples_pts = -1;
    }
    if (d->subtitle) avcodec_flush_buffers(d->subtitle);
    d->flushing = 0;
    return 0;
}

void jv_decoder_first_picture(JVDecoder *d) {
    d->one_shot = 1;
    d->ctx->skip_frame = AVDISCARD_NONKEY;
}

void jv_decoder_skip_nonref_until(JVDecoder *d, double seconds) {
    d->skip_until = seconds > 0 ? video_ts(d, seconds) : AV_NOPTS_VALUE;
    d->skip_to_key = 0;
    if (d->skip_until == AV_NOPTS_VALUE) d->ctx->skip_frame = AVDISCARD_DEFAULT;
}

void jv_decoder_skip_to_keyframe_after(JVDecoder *d, double seconds) {
    d->skip_until = video_ts(d, seconds);
    d->skip_to_key = 1;
}

double jv_decoder_keyframe(JVDecoder *d, double seconds, int after) {
    AVStream *video = d->media->format->streams[d->media->video_stream];
    const AVIndexEntry *e = avformat_index_get_entry_from_timestamp(
        video, video_ts(d, seconds), after ? 0 : AVSEEK_FLAG_BACKWARD);
    if (!e || !(e->flags & AVINDEX_KEYFRAME) || e->timestamp == AV_NOPTS_VALUE) return -1;
    int64_t start = video->start_time != AV_NOPTS_VALUE ? video->start_time : 0;
    double t = (e->timestamp - start) * av_q2d(video->time_base);
    return t >= 0 ? t : 0;
}

int jv_media_decode(JVMedia *media, const char *hw_backend, int allow_software,
                    const char *decoder_options, int frame_limit, JVDecodeStats *s) {
    JVDecoder *d = jv_decoder_open(media, hw_backend, allow_software, decoder_options, s);
    if (!d) return AVERROR(EINVAL);
    int ret = jv_decoder_seek(d, 0);
    if (ret < 0) avformat_seek_file(media->format, -1, INT64_MIN, 0, 0, 0);
    int64_t start = av_gettime_relative();
    JVFrame frame;
    while (s->frames < frame_limit) {
        ret = jv_decoder_next(d, &frame);
        if (ret == AVERROR_EOF) { ret = 0; break; }
        if (ret == AVERROR_PATCHWELCOME) { snprintf(s->error, sizeof(s->error), "Unsupported decoded pixel format"); break; }
        if (ret < 0) { set_error(s->error, sizeof(s->error), "Decode", ret); break; }
        if (frame.hardware) ++s->hardware_frames; else ++s->software_frames;
        const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(((AVFrame *)frame.handle)->format);
        copy_name(s->pixel_format, sizeof(s->pixel_format), desc ? desc->name : NULL);
        ++s->frames;
        jv_frame_release(frame.handle);
    }
    s->elapsed_seconds = (av_gettime_relative() - start) / 1000000.0;
    jv_decoder_close(d);
    return ret < 0 ? ret : 0;
}

void jv_media_close(JVMedia *media) {
    if (!media) return;
    avformat_close_input(&media->format);
    if (media->io) {
        av_freep(&media->io->opaque);
        av_freep(&media->io->buffer);
        avio_context_free(&media->io);
    }
    av_free(media);
}
