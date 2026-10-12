#define _GNU_SOURCE
#include <assert.h>
#include <math.h>
#include <sys/mman.h>
#include <unistd.h>
#include "../bridge/src/audio.c"

static void test_adpcm(void) {
    char error[256] = {0};
    struct niri_audio *encoder = niri_audio_new(error, sizeof(error));
    assert(encoder && "MS ADPCM encoder must initialize");
    uint8_t extra[32];
    assert(niri_audio_extra(encoder, extra, sizeof(extra)) == 32);
    assert(AV_RL16(extra) == 1012 && AV_RL16(extra + 2) == 7);
    assert(niri_audio_extra(encoder, extra, sizeof(extra) - 1) == -1);
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_ADPCM_MS);
    assert(codec);
    AVCodecContext *decoder = avcodec_alloc_context3(codec);
    assert(decoder);
    decoder->sample_rate = 48000;
    decoder->block_align = 1024;
    av_channel_layout_default(&decoder->ch_layout, 2);
    decoder->extradata = av_mallocz(sizeof(extra) + AV_INPUT_BUFFER_PADDING_SIZE);
    assert(decoder->extradata);
    memcpy(decoder->extradata, extra, sizeof(extra));
    decoder->extradata_size = sizeof(extra);
    assert(avcodec_open2(decoder, codec, NULL) == 0);
    AVFrame *decoded = av_frame_alloc();
    AVPacket *packet = av_packet_alloc();
    assert(decoded && packet);
    assert(av_new_packet(packet, 1024) == 0);

    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    assert(page >= 4048);
    uint8_t *mapping = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    assert(mapping != MAP_FAILED);
    assert(mprotect(mapping + page, page, PROT_NONE) == 0);
    uint8_t *pcm = mapping + page - 4048;
    const uint8_t *encoded = NULL;
    /* Invalid lengths must fail before touching source padding, and leave the
       encoder usable for the next exact block. */
    assert(niri_audio_encode(encoder, pcm, 4049, &encoded, error, sizeof(error)) == -1);
    assert(encoded == NULL && error[0]);
    assert(niri_audio_encode(encoder, pcm, 4047, &encoded, error, sizeof(error)) == -1);
    assert(niri_audio_encode(encoder, NULL, 4048, &encoded, error, sizeof(error)) == -1);
    assert(niri_audio_encode(NULL, pcm, 4048, &encoded, error, sizeof(error)) == -1);

    double energy[2] = {0}, noise[2] = {0};
    const double pi = acos(-1.0);
    int decoded_blocks = 0;
    for (int block = 0; block < 80; ++block) {
        for (int frame = 0; frame < 1012; ++frame) {
            double time = (block * 1012 + frame) / 48000.0;
            int16_t left = (int16_t)(10000 * sin(2 * pi * 440 * time));
            int16_t right = (int16_t)(7000 * sin(2 * pi * 997 * time));
            AV_WL16(pcm + frame * 4, left);
            AV_WL16(pcm + frame * 4 + 2, right);
        }
        assert(niri_audio_encode(encoder, pcm, 4048, &encoded, error, sizeof(error)) == 1024);
        assert(encoded);
        /* Losing a capture block must not damage following codec blocks. */
        if (block % 7 == 3) continue;
        avcodec_flush_buffers(decoder);
        av_frame_unref(decoded);
        memcpy(packet->data, encoded, 1024);
        assert(avcodec_send_packet(decoder, packet) == 0);
        assert(avcodec_receive_frame(decoder, decoded) == 0);
        assert(decoded->nb_samples == 1012 && decoded->ch_layout.nb_channels == 2);
        assert(decoded->format == AV_SAMPLE_FMT_S16 || decoded->format == AV_SAMPLE_FMT_S16P);
        for (int frame = 0; frame < 1012; ++frame) {
            for (int channel = 0; channel < 2; ++channel) {
                int16_t expected = (int16_t)AV_RL16(pcm + frame * 4 + channel * 2);
                const int16_t *samples = (const int16_t *)decoded->data[
                    decoded->format == AV_SAMPLE_FMT_S16P ? channel : 0];
                int16_t actual = samples[decoded->format == AV_SAMPLE_FMT_S16P ? frame : frame * 2 + channel];
                double difference = (double)expected - actual;
                energy[channel] += (double)expected * expected;
                noise[channel] += difference * difference;
            }
        }
        ++decoded_blocks;
    }
    assert(decoded_blocks == 69);
    for (int channel = 0; channel < 2; ++channel)
        assert(noise[channel] / energy[channel] < 0.01);
    assert(munmap(mapping, page * 2) == 0);
    av_packet_free(&packet);
    av_frame_free(&decoded);
    avcodec_free_context(&decoder);
    niri_audio_free(encoder);
    niri_audio_free(NULL);
    puts("PASS: independent stereo ADPCM blocks, exact sample count, bounded input and failure recovery");
}

static AVCodecContext *aac_decoder_new(void) {
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_AAC);
    assert(codec);
    AVCodecContext *decoder = avcodec_alloc_context3(codec);
    assert(decoder);
    decoder->sample_rate = 44100;
    av_channel_layout_default(&decoder->ch_layout, 2);
    /* Derive the decoder configuration from the offered WAVE format, rather
       than copying encoder-specific extradata: AAC-LC, 44.1 kHz, stereo. */
    const uint8_t configuration[] = {0x12, 0x10};
    decoder->extradata = av_mallocz(sizeof(configuration) + AV_INPUT_BUFFER_PADDING_SIZE);
    assert(decoder->extradata);
    memcpy(decoder->extradata, configuration, sizeof(configuration));
    decoder->extradata_size = sizeof(configuration);
    assert(avcodec_open2(decoder, codec, NULL) == 0);
    return decoder;
}

static double aac_tone_fraction(const float *samples, int count, int frequency) {
    const double pi = acos(-1.0);
    double energy = 0, cosine = 0, sine = 0;
    for (int sample = 0; sample < count; ++sample) {
        double phase = 2 * pi * frequency * sample / 44100.0;
        energy += samples[sample] * samples[sample];
        cosine += samples[sample] * cos(phase);
        sine += samples[sample] * sin(phase);
        assert(isfinite(samples[sample]) && fabs(samples[sample]) < 0.8);
    }
    /* Silence and loud static must both fail the signal check. */
    assert(energy / count > 0.005);
    return 2 * (cosine * cosine + sine * sine) / (count * energy);
}

static void test_aac(void) {
    enum { INPUT_BLOCKS = 240 };
    char error[256] = {0};
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    assert(page >= AAC_INPUT_BYTES);
    uint8_t *mapping = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    assert(mapping != MAP_FAILED);
    assert(mprotect(mapping + page, page, PROT_NONE) == 0);
    uint8_t *pcm = mapping + page - AAC_INPUT_BYTES;
    const double pi = acos(-1.0);
    /* Reinitialization represents both reconnect and discarding codec state
       after an input gap. The second run also loses output packets and feeds
       a corrupt one to establish that subsequent speech is not persistent noise. */
    for (int reconnect = 0; reconnect < 2; ++reconnect) {
        struct niri_audio_aac *encoder = niri_audio_aac_new(error, sizeof(error));
        assert(encoder && "AAC encoder must initialize");
        AVCodecContext *decoder = aac_decoder_new();
        AVPacket *packet = av_packet_alloc();
        AVFrame *decoded = av_frame_alloc();
        assert(packet && decoded);
        const uint8_t *encoded = NULL;
        assert(niri_audio_aac_encode(encoder, pcm, AAC_INPUT_BYTES + 1,
                                     &encoded, error, sizeof(error)) == -1);
        assert(encoded == NULL && error[0]);
        assert(niri_audio_aac_encode(encoder, pcm, AAC_INPUT_BYTES - 1,
                                     &encoded, error, sizeof(error)) == -1);
        assert(niri_audio_aac_encode(encoder, NULL, AAC_INPUT_BYTES,
                                     &encoded, error, sizeof(error)) == -1);
        assert(niri_audio_aac_encode(NULL, pcm, AAC_INPUT_BYTES,
                                     &encoded, error, sizeof(error)) == -1);
        assert(niri_audio_aac_encode(encoder, pcm, AAC_INPUT_BYTES,
                                     NULL, error, sizeof(error)) == -1);
        assert(niri_audio_aac_receive(NULL, &encoded, error, sizeof(error)) == -1);
        assert(niri_audio_aac_receive(encoder, NULL, error, sizeof(error)) == -1);
        assert(niri_audio_aac_receive(encoder, &encoded, error, sizeof(error)) == 0);
        assert(encoded == NULL);
        int packets = 0, decoded_packets = 0, quality_packets = 0;
        int first_output_block = -1, last_disturbance = -10;
        size_t encoded_bytes = 0;
        for (int block = 0; block < INPUT_BLOCKS; ++block) {
            for (int frame = 0; frame < AAC_INPUT_FRAMES; ++frame) {
                double time = (block * AAC_INPUT_FRAMES + frame) / 48000.0;
                int16_t left = (int16_t)(10000 * sin(2 * pi * 440 * time));
                int16_t right = (int16_t)(7000 * sin(2 * pi * 997 * time));
                AV_WL16(pcm + frame * 4, left);
                AV_WL16(pcm + frame * 4 + 2, right);
            }
            int size = niri_audio_aac_encode(encoder, pcm, AAC_INPUT_BYTES,
                                              &encoded, error, sizeof(error));
            assert(size >= 0);
            if (!size) {
                assert(encoded == NULL);
                continue;
            }
            assert(encoded && size < 2048);
            assert(!(encoded[0] == 0xff && (encoded[1] & 0xf0) == 0xf0));
            if (first_output_block < 0) first_output_block = block;
            encoded_bytes += size;
            ++packets;
            if (reconnect && packets % 17 == 8) {
                last_disturbance = packets;
                continue;
            }
            av_packet_unref(packet);
            av_frame_unref(decoded);
            assert(av_new_packet(packet, size) == 0);
            memcpy(packet->data, encoded, size);
            if (reconnect && packets == 50) {
                memset(packet->data, 0xff, size);
                assert(avcodec_send_packet(decoder, packet) < 0);
                avcodec_flush_buffers(decoder);
                last_disturbance = packets;
                continue;
            }
            assert(avcodec_send_packet(decoder, packet) == 0);
            assert(avcodec_receive_frame(decoder, decoded) == 0);
            assert(decoded->nb_samples == 1024 && decoded->sample_rate == 44100);
            assert(decoded->ch_layout.nb_channels == 2 && decoded->format == AV_SAMPLE_FMT_FLTP);
            ++decoded_packets;
            /* AAC overlap requires the preceding packet. A dropped/corrupt
               packet may disturb its successor, but the stream must recover
               within the next two 23.2 ms packets. */
            if (packets > 4 && packets - last_disturbance > 2) {
                assert(aac_tone_fraction((const float *)decoded->data[0], 1024, 440) > 0.90);
                assert(aac_tone_fraction((const float *)decoded->data[1], 1024, 997) > 0.90);
                ++quality_packets;
            }
            assert(niri_audio_aac_receive(encoder, &encoded, error, sizeof(error)) == 0);
            assert(encoded == NULL);
        }
        int ideal_packets = INPUT_BLOCKS * 882 / 1024;
        assert(first_output_block >= 0 && first_output_block < 3);
        assert(packets >= ideal_packets - 2 && packets <= ideal_packets);
        assert(decoded_packets >= packets - packets / 17 - 3);
        assert(quality_packets > 150);
        /* The codec must keep the live stream below 140 kbit/s, with no
           seconds of retained samples hidden behind a small output queue. */
        double duration = packets * 1024 / 44100.0;
        assert(encoded_bytes * 8 / duration < 140000);
        av_packet_free(&packet);
        av_frame_free(&decoded);
        avcodec_free_context(&decoder);
        niri_audio_aac_free(encoder);
    }
    assert(munmap(mapping, page * 2) == 0);
    niri_audio_aac_free(NULL);
    puts("PASS: raw AAC-LC stereo, bounded resampling/delay/bitrate, signal quality and loss/reconnect recovery");
}

int main(void) {
    test_adpcm();
    test_aac();
    return 0;
}
