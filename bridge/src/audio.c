/* FFmpeg owns compressed audio encoding; RDPSND carries raw codec packets. */
#include <libavcodec/avcodec.h>
#include <libavutil/audio_fifo.h>
#include <libavutil/intreadwrite.h>
#include <libavutil/log.h>
#include <libswresample/swresample.h>
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

/* Match GNOME Remote Desktop's AAC_MS offer: stereo AAC-LC at 44.1 kHz,
   96 kbit/s, with raw 1024-frame AAC packets and no WAVE extra-format bytes.
   Capture remains stereo S16LE at 48 kHz in exact 20 ms blocks. */
#define AAC_INPUT_FRAMES 960
#define AAC_INPUT_BYTES (AAC_INPUT_FRAMES * 4)
#define AAC_FRAME_SAMPLES 1024
#define AAC_FIFO_SAMPLES (AAC_FRAME_SAMPLES * 2)

struct niri_audio_aac {
    AVCodecContext *codec;
    AVFrame *frame;
    AVFrame *converted;
    AVPacket *packet;
    SwrContext *resampler;
    AVAudioFifo *fifo;
    int64_t pts;
};

void niri_audio_aac_free(struct niri_audio_aac *a) {
    if (!a) return;
    /* Finish the codec's bounded lookahead only to release its frame queue.
       Teardown never forwards delayed tail packets to the RDP client. */
    if (a->codec && a->packet && avcodec_is_open(a->codec)) {
        int result = avcodec_send_frame(a->codec, NULL);
        if (result == AVERROR(EAGAIN)) {
            while (avcodec_receive_packet(a->codec, a->packet) >= 0)
                av_packet_unref(a->packet);
            result = avcodec_send_frame(a->codec, NULL);
        }
        if (result >= 0) {
            while (avcodec_receive_packet(a->codec, a->packet) >= 0)
                av_packet_unref(a->packet);
        }
    }
    av_packet_free(&a->packet);
    av_frame_free(&a->frame);
    av_frame_free(&a->converted);
    avcodec_free_context(&a->codec);
    swr_free(&a->resampler);
    av_audio_fifo_free(a->fifo);
    free(a);
}

static int aac_frame_allocate(AVFrame *frame, const AVCodecContext *codec) {
    frame->format = codec->sample_fmt;
    frame->sample_rate = codec->sample_rate;
    frame->nb_samples = AAC_FRAME_SAMPLES;
    int result = av_channel_layout_copy(&frame->ch_layout, &codec->ch_layout);
    return result < 0 ? result : av_frame_get_buffer(frame, 0);
}

struct niri_audio_aac *niri_audio_aac_new(char *error, size_t length) {
    const AVCodec *codec = avcodec_find_encoder(AV_CODEC_ID_AAC);
    if (!codec) {
        snprintf(error, length, "FFmpeg lacks AAC");
        return NULL;
    }
    struct niri_audio_aac *a = calloc(1, sizeof(*a));
    if (!a) {
        snprintf(error, length, "allocate AAC encoder");
        return NULL;
    }
    a->codec = avcodec_alloc_context3(codec);
    a->frame = av_frame_alloc();
    a->converted = av_frame_alloc();
    a->packet = av_packet_alloc();
    if (!a->codec || !a->frame || !a->converted || !a->packet) {
        snprintf(error, length, "allocate AAC encoder buffers");
        goto fail;
    }
    AVCodecContext *c = a->codec;
    c->sample_rate = 44100;
    /* AAC's routine teardown statistics include "Qavg: nan" when negotiation
       closes an unused encoder. Keep those at verbose level, retaining errors. */
    c->log_level_offset = AV_LOG_VERBOSE - AV_LOG_INFO;
    c->sample_fmt = AV_SAMPLE_FMT_FLTP;
    c->bit_rate = 96000;
    c->profile = AV_PROFILE_AAC_LOW;
    c->time_base = (AVRational){1, c->sample_rate};
    av_channel_layout_default(&c->ch_layout, 2);
    int result = avcodec_open2(c, codec, NULL);
    if (result < 0) {
        audio_error(error, length, "open AAC encoder", result);
        goto fail;
    }
    if (c->frame_size != AAC_FRAME_SAMPLES || c->sample_fmt != AV_SAMPLE_FMT_FLTP ||
        c->sample_rate != 44100 || c->ch_layout.nb_channels != 2 ||
        c->profile != AV_PROFILE_AAC_LOW || c->initial_padding > AAC_FRAME_SAMPLES * 2) {
        snprintf(error, length, "unsupported AAC encoder layout or delay");
        goto fail;
    }
    result = aac_frame_allocate(a->frame, c);
    if (result >= 0) result = aac_frame_allocate(a->converted, c);
    if (result < 0) {
        audio_error(error, length, "allocate AAC frames", result);
        goto fail;
    }
    AVChannelLayout input_layout;
    av_channel_layout_default(&input_layout, 2);
    result = swr_alloc_set_opts2(&a->resampler, &c->ch_layout, c->sample_fmt,
                                 c->sample_rate, &input_layout, AV_SAMPLE_FMT_S16,
                                 48000, 0, NULL);
    av_channel_layout_uninit(&input_layout);
    if (result >= 0) result = swr_init(a->resampler);
    if (result < 0) {
        audio_error(error, length, "initialize AAC resampler", result);
        goto fail;
    }
    a->fifo = av_audio_fifo_alloc(c->sample_fmt, 2, AAC_FIFO_SAMPLES);
    if (!a->fifo) {
        snprintf(error, length, "allocate AAC sample queue");
        goto fail;
    }
    return a;
fail:
    niri_audio_aac_free(a);
    return NULL;
}

/* Return zero during normal encoder lookahead, a positive raw-packet length,
   or -1 on failure. No ADTS header or format extra data is transmitted. Packet
   storage remains valid until the next encode/receive call or destruction. */
int niri_audio_aac_receive(struct niri_audio_aac *a, const uint8_t **data,
                           char *error, size_t error_length) {
    if (data) *data = NULL;
    if (!a || !data) {
        snprintf(error, error_length, "invalid AAC packet request");
        return -1;
    }
    av_packet_unref(a->packet);
    int result = avcodec_receive_packet(a->codec, a->packet);
    if (result == AVERROR(EAGAIN)) return 0;
    if (result < 0) {
        audio_error(error, error_length, "receive AAC packet", result);
        return -1;
    }
    /* One raw AAC packet must fit the RDPSND Wave2 header's 16-bit body size. */
    if (a->packet->size <= 0 || a->packet->size > UINT16_MAX - 12) {
        snprintf(error, error_length, "invalid AAC packet size");
        return -1;
    }
    *data = a->packet->data;
    return a->packet->size;
}

int niri_audio_aac_encode(struct niri_audio_aac *a, const uint8_t *pcm,
                          size_t length, const uint8_t **data,
                          char *error, size_t error_length) {
    if (data) *data = NULL;
    if (!a || !pcm || !data || length != AAC_INPUT_BYTES) {
        snprintf(error, error_length, "invalid AAC input block");
        return -1;
    }
    int result = av_frame_make_writable(a->converted);
    if (result < 0) {
        audio_error(error, error_length, "prepare AAC resampler frame", result);
        return -1;
    }
    int16_t samples[AAC_INPUT_FRAMES * 2];
    for (size_t sample = 0; sample < AAC_INPUT_FRAMES * 2; ++sample)
        samples[sample] = (int16_t)AV_RL16(pcm + sample * 2);
    const uint8_t *input[] = {(const uint8_t *)samples};
    result = swr_convert(a->resampler, a->converted->data, AAC_FRAME_SAMPLES,
                         input, AAC_INPUT_FRAMES);
    if (result < 0) {
        audio_error(error, error_length, "resample AAC input", result);
        return -1;
    }
    /* A 20 ms input block becomes 882 sample frames. The remainder from the
       preceding call is below 1024, so this queue never grows beyond 1905
       frames and never needs to resize. */
    if (av_audio_fifo_space(a->fifo) < result ||
        av_audio_fifo_write(a->fifo, (void **)a->converted->data, result) != result) {
        snprintf(error, error_length, "AAC sample queue capacity exceeded");
        return -1;
    }
    if (av_audio_fifo_size(a->fifo) < AAC_FRAME_SAMPLES)
        return niri_audio_aac_receive(a, data, error, error_length);
    result = av_frame_make_writable(a->frame);
    if (result < 0) {
        audio_error(error, error_length, "prepare AAC frame", result);
        return -1;
    }
    if (av_audio_fifo_read(a->fifo, (void **)a->frame->data,
                            AAC_FRAME_SAMPLES) != AAC_FRAME_SAMPLES) {
        snprintf(error, error_length, "read AAC sample queue");
        return -1;
    }
    a->frame->pts = a->pts;
    result = avcodec_send_frame(a->codec, a->frame);
    if (result < 0) {
        audio_error(error, error_length, "encode AAC frame", result);
        return -1;
    }
    a->pts += AAC_FRAME_SAMPLES;
    return niri_audio_aac_receive(a, data, error, error_length);
}
