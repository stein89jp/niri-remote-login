/* Small FFmpeg ABI boundary. FFmpeg owns NVENC loading and H.264 encoding. */
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
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
    int64_t pts;
    int width, height;
};

void niri_video_free(struct niri_video *v) {
    if (!v) return;
    sws_freeContext(v->scale);
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

/* software is used by regression tests only; production always requests NVENC. */
struct niri_video *niri_video_new(int width, int height, int software,
                                 char *error, size_t length) {
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
    c->time_base = (AVRational){1, 30};
    c->framerate = (AVRational){30, 1};
    /* NVENC's packed RGB conversion is fixed to limited-range BT.601.
       RDP AVC420 requires full-range BT.709, so convert explicitly first. */
    c->pix_fmt = software ? AV_PIX_FMT_YUV420P : AV_PIX_FMT_NV12;
    c->bit_rate = 12000000;
    c->gop_size = 60;
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
        v->rgb = av_frame_alloc();
        if (!v->rgb) goto fail;
        v->rgb->format = AV_PIX_FMT_BGR0;
        v->rgb->width = c->width;
        v->rgb->height = c->height;
        if (av_frame_get_buffer(v->rgb, 32) < 0) goto fail;
        v->scale = sws_getContext(c->width, c->height, AV_PIX_FMT_BGR0,
                                  c->width, c->height, c->pix_fmt,
                                  SWS_FAST_BILINEAR, NULL, NULL, NULL);
        if (!v->scale) goto fail;
        const int *coefficients = sws_getCoefficients(SWS_CS_ITU709);
        if (sws_setColorspaceDetails(v->scale, coefficients, 1, coefficients, 1,
                                     0, 1 << 16, 1 << 16) < 0) goto fail;
    }
    return v;
fail:
    niri_video_free(v);
    return NULL;
}

/* Returned packet memory remains owned by v until the next call or free. */
int niri_video_encode(struct niri_video *v, const uint8_t *pixels, int stride,
                      int force_idr, const uint8_t **data, char *error, size_t length) {
    int result = av_frame_make_writable(v->frame);
    if (result < 0) goto fail;
    AVFrame *rgb = v->scale ? v->rgb : v->frame;
    if (v->scale) {
        result = av_frame_make_writable(rgb);
        if (result < 0) goto fail;
    }
    for (int y = 0; y < v->height; ++y) {
        uint8_t *row = rgb->data[0] + (size_t)y * rgb->linesize[0];
        memcpy(row, pixels + (size_t)y * stride, (size_t)v->width * 4);
        for (int x = v->width; x < v->codec->width; ++x)
            memcpy(row + (size_t)x * 4, row + (size_t)(v->width - 1) * 4, 4);
    }
    for (int y = v->height; y < v->codec->height; ++y)
        memcpy(rgb->data[0] + (size_t)y * rgb->linesize[0],
               rgb->data[0] + (size_t)(v->height - 1) * rgb->linesize[0],
               (size_t)v->codec->width * 4);
    if (v->scale) {
        result = sws_scale(v->scale, (const uint8_t * const *)rgb->data,
                           rgb->linesize, 0, v->codec->height,
                           v->frame->data, v->frame->linesize);
        if (result != v->codec->height) {
            result = AVERROR(EINVAL);
            goto fail;
        }
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
