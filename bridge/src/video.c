/* Small FFmpeg ABI boundary. FFmpeg owns NVENC loading and H.264 encoding. */
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/mem.h>
#include <libavutil/cpu.h>
#include <libswscale/swscale.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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
};

void niri_video_free(struct niri_video *v) {
    if (!v) return;
    sws_freeContext(v->scale);
    av_free(v->tail);
    av_packet_free(&v->packet);
    av_frame_free(&v->frame);
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
    if (!v || bitrate < 1000000 || bitrate > 100000000) return -1;
    v->codec->bit_rate = bitrate;
    v->codec->rc_max_rate = bitrate;
    v->codec->rc_buffer_size = bitrate / v->codec->framerate.num;
    return 0;
}

/* software is used by regression tests only; production always requests NVENC. */
struct niri_video *niri_video_new(int width, int height, int software, int fps, int bitrate,
                                 char *error, size_t length) {
    if (width < 1 || height < 1 || width > 4096 || height > 4096 ||
        fps < 1 || fps > 60 || bitrate < 1000000 || bitrate > 100000000) {
        snprintf(error, length, "invalid H.264 dimensions, framerate or bitrate");
        return NULL;
    }
    const AVCodec *codec = avcodec_find_encoder_by_name(software ? "libx264" : "h264_nvenc");
    if (!codec) {
        snprintf(error, length, "FFmpeg lacks %s", software ? "libx264" : "h264_nvenc");
        return NULL;
    }
    struct niri_video *v = calloc(1, sizeof(*v));
    if (!v) return NULL;
    v->codec = avcodec_alloc_context3(codec);
    v->frame = av_frame_alloc();
    v->packet = av_packet_alloc();
    if (!v->codec || !v->frame || !v->packet) goto fail;
    AVCodecContext *c = v->codec;
    v->width = width;
    v->height = height;
    /* MS-RDPEGFX 2.2.4.4 requires 16-aligned bitstreams. RDP regions crop
       the padding; the SPS must not crop the coded dimensions instead. */
    c->width = (width + 15) & ~15;
    c->height = (height + 15) & ~15;
    c->time_base = (AVRational){1, fps};
    c->framerate = (AVRational){fps, 1};
    /* NVENC's packed RGB conversion is fixed to limited-range BT.601.
       RDP AVC420 requires full-range BT.709, so convert explicitly first. */
    c->pix_fmt = software ? AV_PIX_FMT_YUV420P : AV_PIX_FMT_NV12;
    c->bit_rate = bitrate;
    c->gop_size = 2 * fps;
    c->max_b_frames = 0;
    c->thread_count = 1;
    c->color_range = AVCOL_RANGE_JPEG;
    c->colorspace = AVCOL_SPC_BT709;
    c->color_primaries = AVCOL_PRI_BT709;
    c->color_trc = AVCOL_TRC_BT709;
    c->flags |= AV_CODEC_FLAG_LOW_DELAY;
    AVDictionary *options = NULL;
    av_dict_set(&options, "preset", software ? "ultrafast" : "p1", 0);
    av_dict_set(&options, "tune", software ? "zerolatency" : "ull", 0);
    av_dict_set(&options, "profile", "high", 0);
    if (!software) {
        /* FFmpeg otherwise defaults to VBR with a two-second VBV and an
           unspecified maximum rate. A one-frame VBV constrains interactive
           transport bursts; bounded VBR saves bandwidth on simple frames. */
        c->rc_max_rate = bitrate;
        c->rc_buffer_size = bitrate / fps;
        av_dict_set(&options, "rc", "vbr", 0);
        av_dict_set(&options, "multipass", "disabled", 0);
        /* This interface submits and drains one frame at a time. */
        av_dict_set(&options, "surfaces", "2", 0);
        av_dict_set(&options, "zerolatency", "1", 0);
        av_dict_set(&options, "rc-lookahead", "0", 0);
        av_dict_set(&options, "delay", "0", 0);
        av_dict_set(&options, "forced-idr", "1", 0);
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
    {
        /* sws_scale() does not use libswscale's workers. For larger desktops,
           a padded BGR staging frame enables sws_scale_frame()'s parallel
           conversion, which outweighs the copy cost at 1080p and 4K. */
#if LIBSWSCALE_VERSION_MAJOR >= 6
        int threads = av_cpu_count();
        if (threads > 4) threads = 4;
        if (width * height >= 640 * 480 && threads > 1) {
            v->scale = sws_alloc_context();
            if (!v->scale) goto fail;
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
                if (!v->rgb) goto fail;
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
            if (!v->scale) goto fail;
            v->tail_stride = width * 4 + 64;
            int tail_rows = 2 + (64 + width * 4 - 1) / (width * 4);
            v->direct_height = height > tail_rows ? (height - tail_rows) & ~1 : 0;
            v->tail = av_mallocz((size_t)v->tail_stride * (height - v->direct_height) + 64);
            if (!v->tail) goto fail;
        }
        const int *coefficients = sws_getCoefficients(SWS_CS_ITU709);
        if (sws_setColorspaceDetails(v->scale, coefficients, 1, coefficients, 1,
                                     0, 1 << 16, 1 << 16) < 0) goto fail;
    }
    return v;
fail:
    niri_video_free(v);
    return NULL;
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

/* Returned packet memory remains owned by v until the next call or free. */
int niri_video_encode(struct niri_video *v, const uint8_t *pixels, int stride,
                      int force_idr, const uint8_t **data, char *error, size_t length) {
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
    v->frame->pts = v->pts++;
    v->frame->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    result = avcodec_send_frame(v->codec, v->frame);
    if (result < 0) goto fail;
    av_packet_unref(v->packet);
    result = avcodec_receive_packet(v->codec, v->packet);
    /* No B frames/lookahead: every submitted frame must immediately yield a packet. */
    if (result < 0) goto fail;
    *data = v->packet->data;
    return v->packet->size;
fail:
    error_text(error, length, "encode H.264 frame", result);
    return -1;
}
