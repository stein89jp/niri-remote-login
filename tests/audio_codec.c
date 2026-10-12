#define _GNU_SOURCE
#include <assert.h>
#include <math.h>
#include <sys/mman.h>
#include <unistd.h>
#include "../bridge/src/audio.c"

int main(void) {
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
    return 0;
}
