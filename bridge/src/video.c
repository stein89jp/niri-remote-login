/* Small FFmpeg ABI boundary. FFmpeg owns NVENC loading and H.264 encoding. */
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/mem.h>
#include <libavutil/cpu.h>
#include <libswscale/swscale.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct niri_gpu;
static void video_gpu_free(struct niri_gpu *gpu);

/* RDPGFX is reliable TCP and every recovery point (refresh, resume, dropped
   frame, resize, caps change, rate change) already forces an IDR. A periodic
   IDR is therefore only a rare safety net, not a loss-recovery mechanism. */
#define NIRI_VIDEO_DEFAULT_GOP_SECONDS 30
#define NIRI_VIDEO_MAX_GOP_SECONDS 3600

struct niri_video {
    AVCodecContext *codec;
    AVFrame *frame;
    AVFrame *rgb;
    AVPacket *packet;
    struct SwsContext *scale;
    uint8_t *tail;
    int tail_stride;
    int direct_height;
    int64_t pts;
    int width, height;
    int avc444;
    AVFrame *full_chroma;
    AVFrame *auxiliary;
    AVPacket *main_packet;
    double convert_ms, encode_ms;
    int preset, spatial_aq;
    int fps, gop_seconds;
    struct niri_gpu *gpu;
};

static double video_now_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (double)now.tv_sec * 1000.0 + now.tv_nsec / 1000000.0;
}

void niri_video_free(struct niri_video *v) {
    if (!v) return;
    video_gpu_free(v->gpu);
    sws_freeContext(v->scale);
    av_free(v->tail);
    av_packet_free(&v->packet);
    av_packet_free(&v->main_packet);
    av_frame_free(&v->frame);
    av_frame_free(&v->full_chroma);
    av_frame_free(&v->auxiliary);
    av_frame_free(&v->rgb);
    avcodec_free_context(&v->codec);
    free(v);
}

static void error_text(char *error, size_t length, const char *step, int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, detail, sizeof(detail));
    snprintf(error, length, "%s: %s", step, detail);
}

/* FFmpeg's NVENC wrapper reconfigures rate control before the next frame when
   these public context fields change. Keep the context alive; FFmpeg requests
   an IDR as part of NVENC's rate reconfiguration for decoder continuity. */
int niri_video_set_bitrate(struct niri_video *v, int bitrate) {
    if (!v || !v->codec || bitrate < 1000000 || bitrate > 100000000) return -1;
    v->codec->bit_rate = bitrate;
    v->codec->rc_max_rate = bitrate;
    v->codec->rc_buffer_size = bitrate / v->codec->framerate.num;
    return 0;
}

int niri_video_set_rate(struct niri_video *v, int bitrate, int fps) {
    if (!v || !v->codec || fps < 1 || fps > 60 || bitrate < 1000000 || bitrate > 100000000) return -1;
    /* FFmpeg's supported NVENC reconfigure updates bitrate/VBV, not the initial
       frameRateNum. A cadence change needs a fresh context and IDR, rather than
       merely changing an AVCodecContext field the driver never consumes. */
    if (v->codec->framerate.num != fps * (v->avc444 ? 2 : 1)) return 1;
    return niri_video_set_bitrate(v, bitrate);
}

/* Codec fields shared by libx264 (tests), SHM NVENC and DMA-BUF NVENC. */
static void video_configure_codec(const struct niri_video *v, AVCodecContext *c, int bitrate) {
    /* MS-RDPEGFX 2.2.4.4 requires 16-aligned bitstreams. RDP regions crop
       the padding; the SPS must not crop the coded dimensions instead. */
    c->width = (v->width + 15) & ~15;
    c->height = (v->height + 15) & ~15;
    /* AVC444 submits main and auxiliary views as two codec frames. */
    int rate = v->fps * (v->avc444 ? 2 : 1);
    c->time_base = (AVRational){1, rate};
    c->framerate = (AVRational){rate, 1};
    c->bit_rate = bitrate;
    /* Counted in codec frames. AVC444's even length keeps periodic IDRs on
       main views; an auxiliary IDR would discard the main view reference. */
    c->gop_size = v->gop_seconds * rate;
    c->max_b_frames = 0;
    /* Two reference pictures let the alternating RDP main/auxiliary views
       refer to their own previous view instead of predicting only from the
       unrelated immediately preceding one. This adds no reorder/lookahead. */
    if (v->avc444) c->refs = 2;
    c->thread_count = 1;
    c->color_range = AVCOL_RANGE_JPEG;
    c->colorspace = AVCOL_SPC_BT709;
    c->color_primaries = AVCOL_PRI_BT709;
    c->color_trc = AVCOL_TRC_BT709;
    c->flags |= AV_CODEC_FLAG_LOW_DELAY;
}

/* One NVENC configuration for SHM and DMA-BUF contexts, so GPU promotion
   reproduces the SHM encoder exactly, including an adapted bitrate. */
static void nvenc_configure(const struct niri_video *v, AVCodecContext *c, int bitrate,
                            AVDictionary **options) {
    video_configure_codec(v, c, bitrate);
    char preset_name[3] = {'p', (char)('0' + v->preset), '\0'};
    av_dict_set(options, "preset", preset_name, 0);
    av_dict_set(options, "tune", "ull", 0);
    av_dict_set(options, "profile", "high", 0);
    /* FFmpeg otherwise defaults to VBR with a two-second VBV and an
       unspecified maximum rate. A one-frame VBV constrains interactive
       transport bursts; bounded VBR saves bandwidth on simple frames. */
    c->rc_max_rate = bitrate;
    c->rc_buffer_size = bitrate / c->framerate.num;
    av_dict_set(options, "rc", "vbr", 0);
    av_dict_set(options, "multipass", "disabled", 0);
    /* This interface submits and drains one frame at a time. */
    av_dict_set(options, "surfaces", "2", 0);
    av_dict_set(options, "zerolatency", "1", 0);
    av_dict_set(options, "rc-lookahead", "0", 0);
    av_dict_set(options, "delay", "0", 0);
    av_dict_set(options, "forced-idr", "1", 0);
    av_dict_set(options, "spatial-aq", v->spatial_aq ? "1" : "0", 0);
    av_dict_set(options, "aq-strength", "8", 0);
    if (v->avc444) av_dict_set(options, "dpb_size", "2", 0);
}

/* software is used by regression tests only; production always requests NVENC. */
struct niri_video *niri_video_new_encoder(int width, int height, int software, int fps, int bitrate,
                                         int preset, int spatial_aq, int avc444, int gop_seconds,
                                         char *error, size_t length) {
    if (length) error[0] = '\0';
    if (width < 1 || height < 1 || width > 4096 || height > 4096 ||
        fps < 1 || fps > 60 || bitrate < 1000000 || bitrate > 100000000 ||
        preset < 1 || preset > 7 || (spatial_aq != 0 && spatial_aq != 1) ||
        (avc444 != 0 && avc444 != 1) || gop_seconds < 1 ||
        gop_seconds > NIRI_VIDEO_MAX_GOP_SECONDS) {
        snprintf(error, length, "invalid H.264 dimensions, framerate, bitrate or GOP");
        return NULL;
    }
    const AVCodec *codec = avcodec_find_encoder_by_name(software ? "libx264" : "h264_nvenc");
    if (!codec) {
        snprintf(error, length, "FFmpeg lacks %s", software ? "libx264" : "h264_nvenc");
        return NULL;
    }
    struct niri_video *v = calloc(1, sizeof(*v));
    if (!v) {
        snprintf(error, length, "allocate H.264 encoder state");
        return NULL;
    }
    v->codec = avcodec_alloc_context3(codec);
    v->frame = av_frame_alloc();
    v->packet = av_packet_alloc();
    if (!v->codec || !v->frame || !v->packet) {
        snprintf(error, length, "allocate H.264 encoder context");
        goto fail;
    }
    AVCodecContext *c = v->codec;
    v->width = width;
    v->height = height;
    v->avc444 = avc444;
    v->preset = preset;
    v->spatial_aq = spatial_aq;
    v->fps = fps;
    v->gop_seconds = gop_seconds;
    /* NVENC's packed RGB conversion is fixed to limited-range BT.601.
       RDP AVC420 requires full-range BT.709, so convert explicitly first. */
    c->pix_fmt = software ? AV_PIX_FMT_YUV420P : AV_PIX_FMT_NV12;
    AVDictionary *options = NULL;
    if (software) {
        video_configure_codec(v, c, bitrate);
        av_dict_set(&options, "preset", "ultrafast", 0);
        av_dict_set(&options, "tune", "zerolatency", 0);
        av_dict_set(&options, "profile", "high", 0);
    } else {
        nvenc_configure(v, c, bitrate, &options);
    }
    int result = avcodec_open2(c, codec, &options);
    av_dict_free(&options);
    if (result < 0) {
        error_text(error, length, "open H.264 encoder", result);
        goto fail;
    }
    v->frame->format = c->pix_fmt;
    v->frame->width = c->width;
    v->frame->height = c->height;
    v->frame->color_range = c->color_range;
    v->frame->colorspace = c->colorspace;
    result = av_frame_get_buffer(v->frame, 32);
    if (result < 0) {
        error_text(error, length, "allocate encoder frame", result);
        goto fail;
    }
    if (avc444) {
        v->full_chroma = av_frame_alloc();
        v->auxiliary = av_frame_alloc();
        v->main_packet = av_packet_alloc();
        v->rgb = av_frame_alloc();
        if (!v->full_chroma || !v->auxiliary || !v->main_packet || !v->rgb) {
            snprintf(error, length, "allocate AVC444 frames");
            goto fail;
        }
        v->full_chroma->format = AV_PIX_FMT_YUV444P;
        v->full_chroma->width = c->width;
        v->full_chroma->height = c->height;
        v->auxiliary->format = c->pix_fmt;
        v->auxiliary->width = c->width;
        v->auxiliary->height = c->height;
        v->rgb->format = AV_PIX_FMT_BGR0;
        v->rgb->width = width;
        v->rgb->height = height;
        if (av_frame_get_buffer(v->full_chroma, 32) < 0 ||
            av_frame_get_buffer(v->auxiliary, 32) < 0 || av_frame_get_buffer(v->rgb, 32) < 0) {
            snprintf(error, length, "allocate AVC444 frame buffers");
            goto fail;
        }
        v->scale = sws_getContext(width, height, AV_PIX_FMT_BGR0,
                                  width, height, AV_PIX_FMT_YUV444P,
                                  SWS_POINT | SWS_FULL_CHR_H_INP | SWS_ACCURATE_RND,
                                  NULL, NULL, NULL);
        if (!v->scale) {
            snprintf(error, length, "create AVC444 color conversion");
            goto fail;
        }
        const int *coefficients = sws_getCoefficients(SWS_CS_ITU709);
        if (sws_setColorspaceDetails(v->scale, coefficients, 1, coefficients, 1,
                                     0, 1 << 16, 1 << 16) < 0) {
            snprintf(error, length, "configure AVC444 BT.709 color conversion");
            goto fail;
        }
        return v;
    }
    {
        /* sws_scale() does not use libswscale's workers. For larger desktops,
           a padded BGR staging frame enables sws_scale_frame()'s parallel
           conversion, which outweighs the copy cost at 1080p and 4K. */
#if LIBSWSCALE_VERSION_MAJOR >= 6
        int threads = av_cpu_count();
        if (threads > 4) threads = 4;
        if (width * height >= 640 * 480 && threads > 1) {
            v->scale = sws_alloc_context();
            if (!v->scale) {
                snprintf(error, length, "allocate parallel color conversion");
                goto fail;
            }
            if (av_opt_set_int(v->scale, "srcw", width, 0) < 0 ||
                av_opt_set_int(v->scale, "srch", height, 0) < 0 ||
                av_opt_set_int(v->scale, "dstw", width, 0) < 0 ||
                av_opt_set_int(v->scale, "dsth", height, 0) < 0 ||
                av_opt_set_int(v->scale, "src_format", AV_PIX_FMT_BGR0, 0) < 0 ||
                av_opt_set_int(v->scale, "dst_format", c->pix_fmt, 0) < 0 ||
                av_opt_set_int(v->scale, "sws_flags", SWS_FAST_BILINEAR, 0) < 0 ||
                av_opt_set_int(v->scale, "threads", threads, 0) < 0) {
                /* Keep compatibility with FFmpeg builds without workers. */
                sws_freeContext(v->scale);
                v->scale = NULL;
            } else {
                result = sws_init_context(v->scale, NULL, NULL);
                if (result < 0) {
                    error_text(error, length, "initialize parallel color conversion", result);
                    goto fail;
                }
                v->rgb = av_frame_alloc();
                if (!v->rgb) {
                    snprintf(error, length, "allocate color conversion frame");
                    goto fail;
                }
                v->rgb->format = AV_PIX_FMT_BGR0;
                v->rgb->width = width;
                v->rgb->height = height;
                result = av_frame_get_buffer(v->rgb, 32);
                if (result < 0) {
                    error_text(error, length, "allocate color conversion frame", result);
                    goto fail;
                }
            }
        }
#endif
        if (!v->scale) {
            /* Small/single-core frames avoid staging a full BGR desktop.
               SIMD readers need a chroma pair plus 64 bytes after a direct
               slice; convert those final rows from padded scratch instead. */
            v->scale = sws_getContext(width, height, AV_PIX_FMT_BGR0,
                                      width, height, c->pix_fmt,
                                      SWS_FAST_BILINEAR, NULL, NULL, NULL);
            if (!v->scale) {
                snprintf(error, length, "create color conversion");
                goto fail;
            }
            v->tail_stride = width * 4 + 64;
            int tail_rows = 2 + (64 + width * 4 - 1) / (width * 4);
            v->direct_height = height > tail_rows ? (height - tail_rows) & ~1 : 0;
            v->tail = av_mallocz((size_t)v->tail_stride * (height - v->direct_height) + 64);
            if (!v->tail) {
                snprintf(error, length, "allocate color conversion tail rows");
                goto fail;
            }
        }
        const int *coefficients = sws_getCoefficients(SWS_CS_ITU709);
        if (sws_setColorspaceDetails(v->scale, coefficients, 1, coefficients, 1,
                                     0, 1 << 16, 1 << 16) < 0) {
            snprintf(error, length, "configure BT.709 color conversion");
            goto fail;
        }
    }
    return v;
fail:
    niri_video_free(v);
    return NULL;
}

/* Stable entry points for fixtures and benchmarks using the default GOP. */
struct niri_video *niri_video_new_config(int width, int height, int software, int fps, int bitrate,
                                        int preset, int spatial_aq, int avc444,
                                        char *error, size_t length) {
    return niri_video_new_encoder(width, height, software, fps, bitrate, preset, spatial_aq,
                                  avc444, NIRI_VIDEO_DEFAULT_GOP_SECONDS, error, length);
}

struct niri_video *niri_video_new(int width, int height, int software, int fps, int bitrate,
                                 char *error, size_t length) {
    return niri_video_new_config(width, height, software, fps, bitrate, 1, 0, 0, error, length);
}

/* Replicate the final sample/row into RDP's 16-aligned coded dimensions.
   step is two for interleaved NV12 chroma, one for luma or planar chroma. */
static void pad_plane(uint8_t *data, int stride, int width, int height,
                      int padded_width, int padded_height, int step) {
    if (width < padded_width) {
        for (int y = 0; y < height; ++y) {
            uint8_t *row = data + (size_t)y * stride;
            for (int x = width; x < padded_width; x += step)
                memcpy(row + x, row + width - step, step);
        }
    }
    for (int y = height; y < padded_height; ++y)
        memcpy(data + (size_t)y * stride, data + (size_t)(height - 1) * stride,
               padded_width);
}

/* MS-RDPEGFX 3.3.8.3.2, B1-B7. Main chroma averages a 2x2 block;
   auxiliary Y packs odd U rows above odd V rows in every 16-line macroblock.
   Auxiliary U/V hold the odd column of even source rows. Both views use
   one encoder/decoder reference chain, in main-then-auxiliary order. */
static void pack_avc444(struct niri_video *v) {
    int w = v->codec->width, h = v->codec->height;
    int nv12 = v->frame->format == AV_PIX_FMT_NV12;
    for (int y = 0; y < h; ++y) {
        memcpy(v->frame->data[0] + (size_t)y * v->frame->linesize[0],
               v->full_chroma->data[0] + (size_t)y * v->full_chroma->linesize[0], w);
    }
    for (int y = 0; y < h / 2; ++y) {
        for (int x = 0; x < w / 2; ++x) {
            for (int p = 1; p <= 2; ++p) {
                const uint8_t *source = v->full_chroma->data[p] + (size_t)(2 * y) * v->full_chroma->linesize[p] + 2 * x;
                int average = (source[0] + source[1] + source[v->full_chroma->linesize[p]] + source[v->full_chroma->linesize[p] + 1] + 2) / 4;
                int plane = nv12 ? 1 : p;
                int column = nv12 ? 2 * x + p - 1 : x;
                v->frame->data[plane][(size_t)y * v->frame->linesize[plane] + column] = average;
                v->auxiliary->data[plane][(size_t)y * v->auxiliary->linesize[plane] + column] = source[1];
            }
        }
        int aux_row = y / 8 * 16 + y % 8;
        for (int p = 1; p <= 2; ++p)
            memcpy(v->auxiliary->data[0] + (size_t)(aux_row + 8 * (p - 1)) * v->auxiliary->linesize[0],
                   v->full_chroma->data[p] + (size_t)(2 * y + 1) * v->full_chroma->linesize[p], w);
    }
}

static int encode_view(struct niri_video *v, AVFrame *frame, int force_idr,
                       const uint8_t **data, char *error, size_t length) {
    frame->pts = v->pts++;
    frame->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    double start = video_now_ms();
    int result = avcodec_send_frame(v->codec, frame);
    if (result < 0) goto fail;
    av_packet_unref(v->packet);
    result = avcodec_receive_packet(v->codec, v->packet);
    v->encode_ms += video_now_ms() - start;
    if (result < 0) goto fail;
    *data = v->packet->data;
    return v->packet->size;
fail:
    error_text(error, length, "encode H.264 view", result);
    return -1;
}

int niri_video_encode_pair(struct niri_video *v, const uint8_t *pixels, int stride,
                           int force_idr, const uint8_t **main_data, int *main_size,
                           const uint8_t **aux_data, int *aux_size,
                           char *error, size_t length) {
    if (!v || v->gpu || !v->avc444 || !pixels || stride < v->width * 4) {
        snprintf(error, length, "invalid AVC444 input"); return -1;
    }
    if (!v->codec) {
        snprintf(error, length, "H.264 encoder context released by failed GPU promotion");
        return -1;
    }
    double start = video_now_ms();
    v->encode_ms = 0;
    int result = av_frame_make_writable(v->rgb);
    if (result >= 0) result = av_frame_make_writable(v->full_chroma);
    if (result >= 0) result = av_frame_make_writable(v->frame);
    if (result >= 0) result = av_frame_make_writable(v->auxiliary);
    if (result < 0) {
        error_text(error, length, "prepare AVC444 frames", result);
        return -1;
    }
    for (int y = 0; y < v->height; ++y)
        memcpy(v->rgb->data[0] + (size_t)y * v->rgb->linesize[0], pixels + (size_t)y * stride, v->width * 4);
    result = sws_scale(v->scale, (const uint8_t *const *)v->rgb->data, v->rgb->linesize,
                       0, v->height, v->full_chroma->data, v->full_chroma->linesize);
    if (result != v->height) {
        if (result < 0) error_text(error, length, "convert AVC444 RGB to YUV444", result);
        else snprintf(error, length, "convert AVC444 RGB to YUV444: %d of %d rows", result, v->height);
        return -1;
    }
    for (int p = 0; p < 3; ++p)
        pad_plane(v->full_chroma->data[p], v->full_chroma->linesize[p], v->width, v->height,
                  v->codec->width, v->codec->height, 1);
    pack_avc444(v);
    v->convert_ms = video_now_ms() - start;
    *main_size = encode_view(v, v->frame, force_idr, main_data, error, length);
    if (*main_size < 0) return -1;
    av_packet_unref(v->main_packet);
    result = av_packet_ref(v->main_packet, v->packet);
    if (result < 0) {
        error_text(error, length, "retain AVC444 main view", result);
        return -1;
    }
    *main_data = v->main_packet->data;
    /* An auxiliary IDR would erase the main decoder reference before combining.
       Recover the whole pair with an IDR main followed by its auxiliary P view. */
    *aux_size = encode_view(v, v->auxiliary, 0, aux_data, error, length);
    return *aux_size < 0 ? -1 : 0;
}

void niri_video_timings(const struct niri_video *v, double *conversion_ms, double *encode_ms) {
    *conversion_ms = v ? v->convert_ms : 0; *encode_ms = v ? v->encode_ms : 0;
}

/* Returned packet memory remains owned by v until the next call or free. */
int niri_video_encode(struct niri_video *v, const uint8_t *pixels, int stride,
                      int force_idr, const uint8_t **data, char *error, size_t length) {
    if(!v || v->gpu || !pixels || stride<v->width*4) {
        snprintf(error,length,"CPU input requires a SHM encoder context");return -1;
    }
    if (!v->codec) {
        snprintf(error, length, "H.264 encoder context released by failed GPU promotion");
        return -1;
    }
    double start = video_now_ms();
    v->encode_ms = 0;
    int result = av_frame_make_writable(v->frame);
    if (result < 0) goto fail;
#if LIBSWSCALE_VERSION_MAJOR >= 6
    if (v->rgb) {
        result = av_frame_make_writable(v->rgb);
        if (result < 0) goto fail;
        for (int row = 0; row < v->height; ++row)
            memcpy(v->rgb->data[0] + (size_t)row * v->rgb->linesize[0],
                   pixels + (size_t)row * stride, (size_t)v->width * 4);
        /* The storage includes the coded padding, but conversion must retain
           the logical dimensions so pixels are never stretched. */
        AVFrame output = *v->frame;
        output.extended_data = output.data;
        output.width = v->width;
        output.height = v->height;
        result = sws_scale_frame(v->scale, &output, v->rgb);
        if (result < 0) goto fail;
    } else
#endif
    {
        const uint8_t *source[4] = {pixels, NULL, NULL, NULL};
        const int strides[4] = {stride, 0, 0, 0};
        int direct_height = v->direct_height;
        int tail_height = v->height - direct_height;
        int converted = 0;
        if (direct_height > 0) {
            result = sws_scale(v->scale, source, strides, 0, direct_height,
                               v->frame->data, v->frame->linesize);
            if (result < 0) goto fail;
            converted = result;
        }
        for (int row = 0; row < tail_height; ++row)
            memcpy(v->tail + (size_t)row * v->tail_stride,
                   pixels + (size_t)(direct_height + row) * stride, (size_t)v->width * 4);
        const uint8_t *tail_source[4] = {v->tail, NULL, NULL, NULL};
        const int tail_strides[4] = {v->tail_stride, 0, 0, 0};
        result = sws_scale(v->scale, tail_source, tail_strides, direct_height, tail_height,
                           v->frame->data, v->frame->linesize);
        if (result < 0) goto fail;
        if (converted + result != v->height) {
            result = AVERROR(EINVAL);
            goto fail;
        }
    }
    pad_plane(v->frame->data[0], v->frame->linesize[0], v->width, v->height,
              v->codec->width, v->codec->height, 1);
    int chroma_width = (v->width + 1) / 2;
    int chroma_height = (v->height + 1) / 2;
    if (v->codec->pix_fmt == AV_PIX_FMT_NV12) {
        pad_plane(v->frame->data[1], v->frame->linesize[1], chroma_width * 2,
                  chroma_height, v->codec->width, v->codec->height / 2, 2);
    } else {
        for (int plane = 1; plane <= 2; ++plane)
            pad_plane(v->frame->data[plane], v->frame->linesize[plane],
                      chroma_width, chroma_height, v->codec->width / 2,
                      v->codec->height / 2, 1);
    }
    v->convert_ms = video_now_ms() - start;
    return encode_view(v, v->frame, force_idr, data, error, length);
fail:
    error_text(error, length, "encode H.264 frame", result);
    return -1;
}

#include "video_gpu.c"
