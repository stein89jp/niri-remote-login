/* FFmpeg owns MS ADPCM encoding; each packet is an independent WAVE block. */
#include <libavcodec/avcodec.h>
#include <libavutil/intreadwrite.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct niri_audio {
    AVCodecContext *codec;
    AVFrame *frame;
    AVPacket *packet;
    int64_t pts;
};

void niri_audio_free(struct niri_audio *a) {
    if (!a) return;
    av_packet_free(&a->packet);
    av_frame_free(&a->frame);
    avcodec_free_context(&a->codec);
    free(a);
}

static void audio_error(char *error, size_t length, const char *step, int code) {
    char detail[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, detail, sizeof(detail));
    snprintf(error, length, "%s: %s", step, detail);
}

struct niri_audio *niri_audio_new(char *error, size_t length) {
    const AVCodec *codec = avcodec_find_encoder(AV_CODEC_ID_ADPCM_MS);
    if (!codec) {
        snprintf(error, length, "FFmpeg lacks MS ADPCM");
        return NULL;
    }
    struct niri_audio *a = calloc(1, sizeof(*a));
    if (!a) {
        snprintf(error, length, "allocate MS ADPCM encoder");
        return NULL;
    }
    a->codec = avcodec_alloc_context3(codec);
    a->frame = av_frame_alloc();
    a->packet = av_packet_alloc();
    if (!a->codec || !a->frame || !a->packet) {
        snprintf(error, length, "allocate MS ADPCM encoder buffers");
        goto fail;
    }
    AVCodecContext *c = a->codec;
    c->sample_rate = 48000;
    c->sample_fmt = AV_SAMPLE_FMT_S16;
    c->time_base = (AVRational){1, c->sample_rate};
    av_channel_layout_default(&c->ch_layout, 2);
    AVDictionary *options = NULL;
    av_dict_set(&options, "block_size", "1024", 0);
    int result = avcodec_open2(c, codec, &options);
    av_dict_free(&options);
    if (result < 0) {
        audio_error(error, length, "open MS ADPCM encoder", result);
        goto fail;
    }
    /* 1012 stereo frames = 21.08 ms; extra data is ADPCMWAVEFORMAT's
       samples-per-block, coefficient count and seven coefficient pairs. */
    if (c->frame_size != 1012 || c->block_align != 1024 ||
        c->extradata_size != 32 || !c->extradata ||
        AV_RL16(c->extradata) != c->frame_size ||
        AV_RL16(c->extradata + 2) != 7) {
        snprintf(error, length, "unsupported MS ADPCM block layout");
        goto fail;
    }
    a->frame->format = c->sample_fmt;
    a->frame->sample_rate = c->sample_rate;
    a->frame->nb_samples = c->frame_size;
    result = av_channel_layout_copy(&a->frame->ch_layout, &c->ch_layout);
    if (result >= 0) result = av_frame_get_buffer(a->frame, 0);
    if (result < 0) {
        audio_error(error, length, "allocate MS ADPCM frame", result);
        goto fail;
    }
    return a;
fail:
    niri_audio_free(a);
    return NULL;
}

int niri_audio_extra(struct niri_audio *a, uint8_t *data, size_t capacity) {
    if (!a || !data || capacity < (size_t)a->codec->extradata_size) return -1;
    memcpy(data, a->codec->extradata, a->codec->extradata_size);
    return a->codec->extradata_size;
}

/* Packet storage remains valid until the next call or encoder destruction. */
int niri_audio_encode(struct niri_audio *a, const uint8_t *pcm, size_t length,
                      const uint8_t **data, char *error, size_t error_length) {
    if (data) *data = NULL;
    if (!a || !pcm || !data || length != (size_t)a->codec->frame_size * 4) {
        snprintf(error, error_length, "invalid MS ADPCM input block");
        return -1;
    }
    int result = av_frame_make_writable(a->frame);
    if (result < 0) {
        audio_error(error, error_length, "prepare MS ADPCM frame", result);
        return -1;
    }
    int16_t *samples = (int16_t *)a->frame->data[0];
    for (size_t sample = 0; sample < length / 2; ++sample)
        samples[sample] = (int16_t)AV_RL16(pcm + sample * 2);
    a->frame->pts = a->pts;
    av_packet_unref(a->packet);
    result = avcodec_send_frame(a->codec, a->frame);
    if (result >= 0) result = avcodec_receive_packet(a->codec, a->packet);
    if (result < 0) {
        audio_error(error, error_length, "encode MS ADPCM block", result);
        return -1;
    }
    if (a->packet->size != a->codec->block_align) {
        snprintf(error, error_length, "unexpected MS ADPCM packet size");
        return -1;
    }
    a->pts += a->frame->nb_samples;
    *data = a->packet->data;
    return a->packet->size;
}
