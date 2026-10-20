/* FFmpeg owns compressed audio encoding; RDPSND carries raw codec packets. */
#include <libavcodec/avcodec.h>
#include <libavutil/audio_fifo.h>
#include <libavutil/intreadwrite.h>
#include <libavutil/log.h>
#include <libavutil/opt.h>
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

/* Uncompressed 44.1 kHz PCM for Windows mstsc, which played 48 kHz PCM about
   8.8% slow (user-approved baseline 2026-10-20). Capture stays 48 kHz S16LE;
   each exact 20 ms block of 960 frames becomes exactly 882 frames
   (960 * 147 / 160), so the output rate equals the input rate exactly.
   swr_convert holds back its filter delay (33 frames with a 64-tap filter)
   on the first block after a reset, then returns exactly 882 frames for
   every further 960-frame block. That first shortfall is filled with leading
   zero frames once, so every call emits one full block from the first and
   the queue is empty between calls. Should a build of libswresample ever
   return a different count, the queue keeps the remainder (at most one
   block, so it can never grow) and a shortfall is filled with zeros. */
#define PCM44_INPUT_FRAMES 960
#define PCM44_OUTPUT_FRAMES 882
#define PCM44_SCRATCH_FRAMES 2048
#define PCM44_FIFO_FRAMES (PCM44_OUTPUT_FRAMES * 4)

struct niri_audio_resampler {
    SwrContext *resampler;
    AVAudioFifo *fifo;
    int channels;
    int primed;
    /* Frames of zeros inserted or of output discarded after priming. */
    uint64_t adjusted_frames;
    int16_t input[PCM44_INPUT_FRAMES * 2];
    int16_t output[PCM44_SCRATCH_FRAMES * 2];
    int16_t zeros[PCM44_OUTPUT_FRAMES * 2]; /* calloc'd, never written */
};

void niri_audio_resampler_free(struct niri_audio_resampler *r) {
    if (!r) return;
    swr_free(&r->resampler);
    av_audio_fifo_free(r->fifo);
    free(r);
}

/* Discard filter history and queued output. The next block primes again. */
int niri_audio_resampler_reset(struct niri_audio_resampler *r, char *error,
                               size_t length) {
    if (!r) {
        snprintf(error, length, "invalid PCM resampler");
        return -1;
    }
    av_audio_fifo_reset(r->fifo);
    r->primed = 0;
    int result = swr_init(r->resampler);
    if (result < 0) {
        audio_error(error, length, "reset PCM resampler", result);
        return -1;
    }
    return 0;
}

struct niri_audio_resampler *niri_audio_resampler_new(int channels, char *error,
                                                      size_t length) {
    if (channels != 1 && channels != 2) {
        snprintf(error, length, "unsupported PCM resampler channel count");
        return NULL;
    }
    struct niri_audio_resampler *r = calloc(1, sizeof(*r));
    if (!r) {
        snprintf(error, length, "allocate PCM resampler");
        return NULL;
    }
    r->channels = channels;
    AVChannelLayout layout;
    av_channel_layout_default(&layout, channels);
    int result = swr_alloc_set_opts2(&r->resampler, &layout, AV_SAMPLE_FMT_S16,
                                     44100, &layout, AV_SAMPLE_FMT_S16, 48000,
                                     0, NULL);
    av_channel_layout_uninit(&layout);
    /* 64 taps keep 19 kHz flat and attenuate 22-24 kHz content that would
       alias, for 0.75 ms of delay; the default 32 taps pass more aliasing. */
    if (result >= 0) result = av_opt_set_int(r->resampler, "filter_size", 64, 0);
    if (result >= 0) result = swr_init(r->resampler);
    if (result < 0) {
        audio_error(error, length, "initialize PCM resampler", result);
        goto fail;
    }
    r->fifo = av_audio_fifo_alloc(AV_SAMPLE_FMT_S16, channels, PCM44_FIFO_FRAMES);
    if (!r->fifo) {
        snprintf(error, length, "allocate PCM resampler queue");
        goto fail;
    }
    return r;
fail:
    niri_audio_resampler_free(r);
    return NULL;
}

/* Test observation: frames queued between calls (zero in steady state) and
   frames ever padded or discarded after priming (zero unless the library
   returns an unexpected count). */
int niri_audio_resampler_state(const struct niri_audio_resampler *r, int *queued,
                               uint64_t *adjusted) {
    if (!r || !queued || !adjusted) return -1;
    *queued = av_audio_fifo_size(r->fifo);
    *adjusted = r->adjusted_frames;
    return 0;
}

/* Queue `frames` (at most one block) of silence. */
static int resampler_write_zeros(struct niri_audio_resampler *r, int frames) {
    void *planes[] = {r->zeros};
    if (frames > PCM44_OUTPUT_FRAMES || av_audio_fifo_space(r->fifo) < frames) return -1;
    return av_audio_fifo_write(r->fifo, planes, frames) == frames ? 0 : -1;
}

/* Convert one 20 ms 48 kHz S16LE block (960 frames of `channels`) into
   exactly 882 frames at 44.1 kHz. Returns the output byte count. */
int niri_audio_resample(struct niri_audio_resampler *r, const uint8_t *pcm,
                        size_t length, uint8_t *output, size_t capacity,
                        char *error, size_t error_length) {
    size_t frame_bytes = r ? (size_t)r->channels * 2 : 0;
    if (!r || !pcm || !output || length != PCM44_INPUT_FRAMES * frame_bytes ||
        capacity < PCM44_OUTPUT_FRAMES * frame_bytes) {
        snprintf(error, error_length, "invalid PCM resampler block");
        return -1;
    }
    size_t samples = PCM44_INPUT_FRAMES * (size_t)r->channels;
    for (size_t sample = 0; sample < samples; ++sample)
        r->input[sample] = (int16_t)AV_RL16(pcm + sample * 2);
    const uint8_t *input[] = {(const uint8_t *)r->input};
    uint8_t *converted[] = {(uint8_t *)r->output};
    int produced = swr_convert(r->resampler, converted, PCM44_SCRATCH_FRAMES,
                               input, PCM44_INPUT_FRAMES);
    if (produced < 0) {
        audio_error(error, error_length, "resample PCM block", produced);
        return -1;
    }
    if (!r->primed) {
        /* Lead with the filter delay as silence so this block is full. */
        int lead = PCM44_OUTPUT_FRAMES - produced;
        if (lead > 0 && resampler_write_zeros(r, lead) < 0) goto overflow;
        r->primed = 1;
    }
    void *planes[] = {r->output};
    if (av_audio_fifo_space(r->fifo) < produced ||
        av_audio_fifo_write(r->fifo, planes, produced) != produced)
        goto overflow;
    int queued = av_audio_fifo_size(r->fifo);
    if (queued < PCM44_OUTPUT_FRAMES) {
        r->adjusted_frames += (uint64_t)(PCM44_OUTPUT_FRAMES - queued);
        if (resampler_write_zeros(r, PCM44_OUTPUT_FRAMES - queued) < 0) goto overflow;
    }
    void *destination[] = {r->output};
    if (av_audio_fifo_read(r->fifo, destination, PCM44_OUTPUT_FRAMES) !=
        PCM44_OUTPUT_FRAMES) {
        snprintf(error, error_length, "read PCM resampler queue");
        return -1;
    }
    /* Keep at most one block of remainder: the queue can never grow. */
    int excess = av_audio_fifo_size(r->fifo) - PCM44_OUTPUT_FRAMES;
    if (excess > 0) {
        r->adjusted_frames += (uint64_t)excess;
        av_audio_fifo_drain(r->fifo, excess);
    }
    for (size_t sample = 0; sample < PCM44_OUTPUT_FRAMES * (size_t)r->channels; ++sample)
        AV_WL16(output + sample * 2, (uint16_t)r->output[sample]);
    return (int)(PCM44_OUTPUT_FRAMES * frame_bytes);
overflow:
    snprintf(error, error_length, "PCM resampler queue capacity exceeded");
    return -1;
}
